//! Backend-neutral real-Linux network acceptance through the actual Driver.
//!
//! An idle, unattached oracle and the shared `network-probe` server and client
//! (see `network_scenario`) run under one `Driver` whose network routes their
//! frames. Host faults are scheduled through `Barrier::schedule`, either at a
//! fixed time, once a machine's committed stdout shows a marker, or once it
//! has published TCP data, so their timing is a function of the simulation,
//! never of host progress.
//!
//! The run ends when the client application exits (with any status), at
//! the scenario's simulated limit, or when the Driver can make no further
//! progress. A host wall-time budget is a diagnostic only and never shapes
//! simulation results.
use super::network_scenario::{
    check_client, definition, definitions, disk_definitions, mac, tcp_record, token_record, Routed,
    Transcript,
};
use crate::{
    definition::MachineDefinition,
    driver::{
        Action, Driver, Event, EventKind, FaultOrigin, FaultTarget, InputKey, MachineId,
        NetworkConfig, SettlementStatus, StepOutcome, Target,
    },
    machine::{Machine, Timestamp},
    pv::{Response, Stream},
    rng::SeededRng,
};
use seedbox_test_time::HangGuard;
use std::{fmt::Debug, time::Duration};

/// Default simulated bound for one run, matching the manual exchange scenario.
const LIMIT: u64 = 120_000_000_000;
/// Host diagnostic only; it cannot interrupt a call that never returns.
const HOST_LIMIT: Duration = Duration::from_secs(900);
/// The Driver's fixed quantum; rounds are shorter while networked machines run.
const QUANTUM: Duration = Duration::from_millis(10);
/// Machine names in identity order: oracle, server, client.
pub(crate) const NAMES: [&str; 3] = ["oracle", "server", "client"];

/// When a rule schedules its action.
#[derive(Clone, Debug)]
pub(crate) enum When {
    /// At this simulation time, scheduled before the first step.
    At(u64),
    /// `after` nanoseconds after the first barrier at which machine `machine`
    /// (an index into [`NAMES`]) has committed stdout containing `text`.
    Output {
        machine: usize,
        text: &'static str,
        after: u64,
    },
    /// `after` nanoseconds after the first barrier at which machine `machine`
    /// has published a nonempty TCP data segment: a guest event that places
    /// the action inside the stream however fast the backend transfers it.
    TcpData { machine: usize, after: u64 },
}

/// A host input scheduled once under `key`.
#[derive(Clone, Debug)]
pub(crate) struct Rule {
    pub(crate) key: &'static str,
    pub(crate) when: When,
    pub(crate) action: Action,
}

/// Driver configuration and host faults of one run.
#[derive(Clone, Debug)]
pub(crate) struct Scenario {
    pub(crate) workers: usize,
    pub(crate) seed: u64,
    pub(crate) network: NetworkConfig,
    pub(crate) rules: Vec<Rule>,
    /// Run the server as `server-disk` on a zeroed 1 MiB disk.
    pub(crate) server_disk: bool,
    /// Simulated time at which the run stops if the client has not exited.
    pub(crate) limit: u64,
}

impl Default for Scenario {
    fn default() -> Self {
        Self {
            workers: 1,
            seed: 1,
            network: NetworkConfig::default(),
            rules: Vec::new(),
            server_disk: false,
            limit: LIMIT,
        }
    }
}

/// Everything observable about one run, owned for comparison.
#[derive(Debug)]
pub(crate) struct Run {
    /// Every drained event, in publication order.
    pub(crate) events: Vec<Event>,
    /// Identities of oracle, server and client.
    pub(crate) ids: [MachineId; 3],
    /// Committed application stdout of each machine, across its boots.
    pub(crate) stdout: [String; 3],
    /// Committed application stderr of each machine, for diagnostics.
    pub(crate) stderr: [String; 3],
    /// The barrier at which each rule was scheduled, by key.
    pub(crate) scheduled: Vec<(&'static str, u64)>,
    /// The last step outcome.
    pub(crate) outcome: StepOutcome,
}

/// A rule's key as an input key; keys are fixed test literals.
fn key(text: &str) -> InputKey {
    InputKey::new(text).expect("valid test input key")
}

/// The target naming machine `index` of [`NAMES`].
pub(crate) fn named(index: usize) -> Target {
    Target::Name(NAMES[index].into())
}

/// The idle oracle (unattached) followed by the networked server and client.
/// With `server_disk`, the server records its progress on its own zeroed disk.
pub(crate) fn scenario_definitions(
    probe: &[u8],
    memory_bytes: u64,
    server_disk: bool,
) -> [MachineDefinition; 3] {
    let mut oracle = definition(NAMES[0], probe, &["idle"], 2, memory_bytes);
    oracle.network = None;
    let [server, client] = if server_disk {
        disk_definitions(probe, memory_bytes)
    } else {
        definitions(probe, memory_bytes)
    };
    [oracle, server, client]
}

/// Drive the oracle, server and client until the client finishes.
pub(crate) fn run<M, F>(probe: &[u8], memory_bytes: u64, scenario: &Scenario, factory: F) -> Run
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
    F: FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
{
    let [oracle, server, client] = scenario_definitions(probe, memory_bytes, scenario.server_disk);
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        QUANTUM,
        scenario.workers,
        SeededRng::new(scenario.seed),
        || Ok(()),
        oracle,
        vec![server, client],
        Vec::new(),
        scenario.network,
        factory,
    )
    .expect("driver construction");
    let ids: Vec<MachineId> = driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))
        .unwrap();
    let ids: [MachineId; 3] = ids.try_into().expect("three machines");
    let mut run = Run {
        events: Vec::new(),
        ids,
        stdout: Default::default(),
        stderr: Default::default(),
        scheduled: Vec::new(),
        outcome: StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(0),
        },
    };
    let mut pending: Vec<Rule> = scenario.rules.clone();
    // Whether each machine has published TCP data, for `When::TcpData`.
    let mut sent_data = [false; 3];
    let mut now = 0;
    let guard = HangGuard::new("native network response transport must return", HOST_LIMIT);
    loop {
        let due: Vec<(Rule, u64)> = pending
            .iter()
            .filter_map(|rule| match rule.when {
                When::At(at) => Some((rule.clone(), at.max(now))),
                When::Output {
                    machine,
                    text,
                    after,
                } => run.stdout[machine]
                    .contains(text)
                    .then(|| (rule.clone(), now.checked_add(after).expect("rule time"))),
                When::TcpData { machine, after } => sent_data[machine]
                    .then(|| (rule.clone(), now.checked_add(after).expect("rule time"))),
            })
            .collect();
        pending.retain(|rule| !due.iter().any(|(fired, _)| fired.key == rule.key));
        for (rule, at) in due {
            driver
                .with_barrier(|barrier| {
                    barrier.schedule(
                        Timestamp::from_nanos(at),
                        key(rule.key),
                        rule.action.clone(),
                    )
                })
                .unwrap_or_else(|error| panic!("schedule {}: {error:?}", rule.key));
            run.scheduled.push((rule.key, now));
        }
        let outcome = driver
            .step(Timestamp::from_nanos(scenario.limit))
            .unwrap_or_else(|error| panic!("driver step: {error:?}"));
        for event in driver.drain_events() {
            if let EventKind::Output(output) = &event.kind {
                let index = ids.iter().position(|id| *id == output.machine).unwrap();
                match &output.payload {
                    Response::ApplicationOutput { stream, bytes } => {
                        let text = match stream {
                            Stream::Stdout => &mut run.stdout[index],
                            Stream::Stderr => &mut run.stderr[index],
                        };
                        text.push_str(&String::from_utf8_lossy(bytes));
                    }
                    Response::TransmitFrame { bytes } => {
                        sent_data[index] |= segment(index, bytes).is_some();
                    }
                    _ => (),
                }
            }
            run.events.push(event);
        }
        run.outcome = outcome.clone();
        // Stop once the client's exit has committed, whatever its status.
        if run.client_exit().is_some() {
            break;
        }
        match outcome {
            StepOutcome::BarrierReached { at } if at.as_nanos() < scenario.limit => {
                now = at.as_nanos()
            }
            _ => break,
        }
        guard.remaining().unwrap_or_else(|error| {
            panic!(
                "network response infrastructure hang at {now} ns: {error}; {:?} {:?}",
                run.stdout, run.stderr
            )
        });
    }
    run
}

impl Run {
    /// Frames committed by machine `index`, with their simulation times.
    pub(crate) fn transmitted(&self, index: usize) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Output(output)
                    if output.machine == self.ids[index]
                        && matches!(output.payload, Response::TransmitFrame { .. }) =>
                {
                    Some(output.at.as_nanos())
                }
                _ => None,
            })
            .collect()
    }

    /// When the fault admitted for input `key` settled Applied.
    pub(crate) fn applied(&self, input: &str) -> Option<u64> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::FaultSettled {
                record,
                status: SettlementStatus::Applied,
                ..
            } if matches!(&record.origin, FaultOrigin::Input { key } if key.as_str() == input) => {
                Some(event.at.as_nanos())
            }
            _ => None,
        })
    }

    /// The target recorded for input `key`'s admitted fault.
    pub(crate) fn target(&self, input: &str) -> Option<FaultTarget> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::FaultAdmitted(record)
                if matches!(&record.origin, FaultOrigin::Input { key } if key.as_str() == input) =>
            {
                Some(record.target.clone())
            }
            _ => None,
        })
    }

    /// The client's exit status, if it exited.
    pub(crate) fn client_exit(&self) -> Option<u8> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::Output(output) if output.machine == self.ids[2] => match output.payload {
                Response::ApplicationOutcome { code } => Some(code),
                _ => None,
            },
            _ => None,
        })
    }
}

/// Semantic success: the client exited 0 after ARP resolution, UDP echo and
/// the verified 256 KiB TCP echo (every byte compared against the probe's
/// pattern), with traffic in both directions. The oracle stayed idle, live
/// and unattached, so the Driver never finished.
pub(crate) fn check(run: &Run) {
    let context = || {
        format!(
            "stdout {:?}, stderr {:?}, outcome {:?}",
            run.stdout, run.stderr, run.outcome
        )
    };
    assert_eq!(run.client_exit(), Some(0), "{}", context());
    assert!(
        !matches!(run.outcome, StepOutcome::Finished { .. }),
        "the oracle ended the run: {}",
        context()
    );
    assert!(
        run.stdout[1].contains("NETPROBE ready port=7000\n"),
        "{}",
        context()
    );
    assert_eq!(run.stdout[0], "NETPROBE idle\n", "{}", context());
    assert!(run.transmitted(0).is_empty());
    // The client's markers and the traffic both ways, as for every backend.
    check_client(&run.transcript());
}

/// Runs of one scenario under different worker counts are identical: every
/// event in order, all application output, the barriers at which rules were
/// scheduled and the final outcome.
pub(crate) fn check_worker_invariance(one: &Run, other: &Run) {
    assert_eq!(one.ids, other.ids);
    assert_eq!(one.outcome, other.outcome);
    assert_eq!(one.scheduled, other.scheduled);
    assert_eq!(one.stdout, other.stdout);
    assert_eq!(one.stderr, other.stderr);
    assert_eq!(one.events.len(), other.events.len());
    for (a, b) in one.events.iter().zip(&other.events) {
        assert_eq!(a, b);
    }
}

/// A TCP data segment's identity: sender, source address and port,
/// destination address and port, sequence number and payload length.
type Segment = (usize, [u8; 4], u16, [u8; 4], u16, u32, usize);

/// Parse an unfragmented Ethernet II IPv4 TCP frame carrying a nonempty
/// payload. Anything malformed, truncated or fragmented is `None`.
fn segment(from: usize, frame: &[u8]) -> Option<Segment> {
    if frame.get(12..14)? != [0x08, 0x00] {
        return None;
    }
    let ip = &frame[14..];
    let header = usize::from(ip.first()? & 0x0f) * 4;
    let total = usize::from(u16::from_be_bytes([*ip.get(2)?, *ip.get(3)?]));
    let fragment = u16::from_be_bytes([*ip.get(6)?, *ip.get(7)?]);
    if ip[0] >> 4 != 4
        || header < 20
        || total < header
        || total > ip.len()
        || fragment & 0x3fff != 0
        || *ip.get(9)? != 6
    {
        return None;
    }
    let tcp = &ip[header..total];
    let offset = usize::from(tcp.get(12)? >> 4) * 4;
    if offset < 20 || offset > tcp.len() {
        return None;
    }
    let payload = Some(tcp.len() - offset).filter(|&n| n > 0)?;
    let address = |at: usize| -> [u8; 4] { ip[at..at + 4].try_into().unwrap() };
    let port = |at: usize| u16::from_be_bytes([tcp[at], tcp[at + 1]]);
    let sequence = u32::from_be_bytes(tcp[4..8].try_into().unwrap());
    Some((
        from,
        address(12),
        port(0),
        address(16),
        port(2),
        sequence,
        payload,
    ))
}

impl Run {
    /// The server/client exchange as the shared scenario's transcript: frames
    /// keyed by their send times, without offer times (Driver delivery is
    /// covered by its own tests), and the client's exit time as `done`.
    pub(crate) fn transcript(&self) -> Transcript {
        use sha2::{Digest, Sha256};
        let frames: Vec<Routed> = self
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Output(output) => match &output.payload {
                    Response::TransmitFrame { bytes } => {
                        let from = self.ids[1..].iter().position(|id| *id == output.machine)?;
                        let other = 1 - from;
                        let destination: [u8; 6] = bytes[..6].try_into().unwrap();
                        let to = (destination == [0xff; 6] || destination == mac(other as u8))
                            .then_some(other);
                        Some((
                            output.at.as_nanos(),
                            from,
                            to,
                            None,
                            Sha256::digest(bytes).into(),
                        ))
                    }
                    _ => None,
                },
                _ => None,
            })
            .collect();
        let done = self
            .events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::Output(output)
                    if output.machine == self.ids[2]
                        && matches!(output.payload, Response::ApplicationOutcome { .. }) =>
                {
                    Some(event.at.as_nanos())
                }
                _ => None,
            })
            .unwrap_or_default();
        Transcript {
            output: [self.stdout[1].clone(), self.stdout[2].clone()],
            frames,
            done,
        }
    }

    /// Publication index of the Applied settlement of input `key`'s fault.
    /// Frames published after it were routed after the effect took its FIFO
    /// turn, whatever their stamps: a round ending at the effect's time
    /// publishes, and routes, its frames before that turn.
    pub(crate) fn applied_index(&self, input: &str) -> Option<usize> {
        self.events.iter().position(|event| {
            matches!(&event.kind, EventKind::FaultSettled {
                record,
                status: SettlementStatus::Applied,
                ..
            } if matches!(&record.origin, FaultOrigin::Input { key } if key.as_str() == input))
        })
    }

    /// Frames machine `index` published strictly between event indices
    /// `after` and `before`, as (simulation time, bytes).
    pub(crate) fn frames_between(
        &self,
        index: usize,
        after: usize,
        before: usize,
    ) -> Vec<(u64, &[u8])> {
        self.events[after + 1..before]
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Output(output) if output.machine == self.ids[index] => {
                    match &output.payload {
                        Response::TransmitFrame { bytes } => {
                            Some((output.at.as_nanos(), &bytes[..]))
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// When the client's exit was observed, in simulation time.
    pub(crate) fn client_exit_at(&self) -> Option<u64> {
        self.events.iter().find_map(|event| match &event.kind {
            EventKind::Output(output)
                if output.machine == self.ids[2]
                    && matches!(output.payload, Response::ApplicationOutcome { .. }) =>
            {
                Some(event.at.as_nanos())
            }
            _ => None,
        })
    }

    /// Published clock-rate transitions of machine `index`, in ppb.
    pub(crate) fn clock_rates(&self, index: usize) -> Vec<i32> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Transition {
                    machine,
                    change: crate::driver::Transition::ClockRate(rate),
                    ..
                } if *machine == self.ids[index] => Some(rate.ppb()),
                _ => None,
            })
            .collect()
    }

    /// Data segments repeated strictly between `after` and `before`, counting copies
    /// of any segment first sent anywhere earlier in the trace.
    pub(crate) fn tcp_retransmissions_between(&self, after: usize, before: usize) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        let mut repeated = 0;
        for (position, event) in self.events.iter().enumerate().take(before) {
            let EventKind::Output(output) = &event.kind else {
                continue;
            };
            let Response::TransmitFrame { bytes } = &output.payload else {
                continue;
            };
            let from = self
                .ids
                .iter()
                .position(|id| *id == output.machine)
                .unwrap();
            if let Some((from, source, sport, destination, dport, sequence, _)) =
                segment(from, bytes)
            {
                let fresh = seen.insert((from, source, sport, destination, dport, sequence));
                if !fresh && position > after {
                    repeated += 1;
                }
            }
        }
        repeated
    }

    /// Simulation times of nonempty TCP data segments machine `index` sent.
    pub(crate) fn tcp_data(&self, index: usize) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Output(output) if output.machine == self.ids[index] => {
                    match &output.payload {
                        Response::TransmitFrame { bytes } => {
                            segment(index, bytes).map(|_| output.at.as_nanos())
                        }
                        _ => None,
                    }
                }
                _ => None,
            })
            .collect()
    }

    /// TCP data segments a machine sent again: the same flow and starting
    /// sequence number with a nonempty payload. IP identifiers, timestamps,
    /// acknowledgments and the resent length may differ and are ignored.
    pub(crate) fn tcp_retransmissions(&self) -> usize {
        // The first event cannot repeat an earlier segment.
        self.tcp_retransmissions_between(0, self.events.len())
    }
}

/// Partition evidence, by publication (routing) order rather than stamps:
/// the client sent TCP data after the observing barrier and before the cut
/// applied; while cut it transmitted, including retransmissions of segments
/// first sent earlier in the trace; and the run succeeded after `heal`.
pub(crate) fn check_partition(run: &Run, cut: &str, heal: &str) {
    check(run);
    let (cut_at, heal_at) = (
        run.applied(cut).expect("partition applied"),
        run.applied(heal).expect("heal applied"),
    );
    assert!(cut_at < heal_at);
    assert!(matches!(run.target(cut), Some(FaultTarget::Links { .. })));
    assert!(matches!(run.target(heal), Some(FaultTarget::Links { .. })));
    let (cut_index, heal_index) = (
        run.applied_index(cut).unwrap(),
        run.applied_index(heal).unwrap(),
    );
    let observed = run.scheduled.iter().find(|(key, _)| *key == cut).unwrap().1;
    let before = run
        .frames_between(2, 0, cut_index)
        .into_iter()
        .filter(|(at, bytes)| *at >= observed && segment(2, bytes).is_some())
        .count();
    assert!(
        before > 0,
        "no client TCP data between {observed} and the cut"
    );
    let while_cut = run.frames_between(2, cut_index, heal_index).len();
    assert!(while_cut > 0, "no client frames routed while cut");
    let resent = run.tcp_retransmissions_between(cut_index, heal_index);
    assert!(resent > 0, "no TCP retransmission routed while cut");
    assert!(run.client_exit_at().unwrap() > heal_at);
}

/// Combined network and storage evidence after a server crash that followed
/// its fsynced token: the second boot found the flushed records, then
/// recorded the verified stream before closing it.
pub(crate) fn check_disk_crash(run: &Run) {
    check(run);
    let (token, tcp) = (token_record(), tcp_record());
    assert_eq!(
        run.stdout[1],
        format!(
            "NETPROBE disk boot=1 token=none tcp=none\n\
             NETPROBE ready port=7000\n\
             NETPROBE disk token={token}\n\
             NETPROBE disk boot=2 token={token} tcp=none\n\
             NETPROBE ready port=7000\n\
             NETPROBE disk tcp={tcp}\n"
        ),
        "stderr {:?}",
        run.stderr
    );
}

// Shared scenarios. Each runs `runner` with one and four workers, requires
// identical runs and applies the backend-neutral assertions; backend entries
// supply only the runner that builds their machines.
const MS: u64 = 1_000_000;

fn after_client(key: &'static str, text: &'static str, after: u64, action: Action) -> Rule {
    Rule {
        key,
        when: When::Output {
            machine: 2,
            text,
            after,
        },
        action,
    }
}

fn after_client_data(key: &'static str, after: u64, action: Action) -> Rule {
    Rule {
        key,
        when: When::TcpData { machine: 2, after },
        action,
    }
}

/// Run `scenario` with one and four workers; the runs must be identical.
fn both(runner: &impl Fn(&Scenario) -> Run, scenario: Scenario) -> Run {
    let one = runner(&Scenario {
        workers: 1,
        ..scenario.clone()
    });
    let four = runner(&Scenario {
        workers: 4,
        ..scenario
    });
    check_worker_invariance(&one, &four);
    one
}

/// The fault-free exchange; retransmission evidence stays quiet without faults.
pub(crate) fn clean_exchange(runner: impl Fn(&Scenario) -> Run) {
    let run = both(&runner, Scenario::default());
    check(&run);
    assert_eq!(run.tcp_retransmissions(), 0);
}

/// Seeded jitter and 2% loss: dropped TCP data is resent and the run succeeds.
pub(crate) fn seeded_loss(runner: impl Fn(&Scenario) -> Run) {
    let network =
        NetworkConfig::new(Duration::from_millis(1), Duration::from_micros(300), 20_000).unwrap();
    let run = both(
        &runner,
        Scenario {
            network,
            ..Scenario::default()
        },
    );
    check(&run);
    assert!(run.tcp_retransmissions() > 0);
}

/// A partition 2 ms after the barrier at which the client's first TCP data
/// segment was observed, healed one second after the cut. Both follow that
/// guest event rather than a fixed delay after an earlier marker, so the cut
/// lands inside the stream on every backend: an x86 TCG client transfers the
/// whole stream within about 10 ms of its ARP marker.
pub(crate) fn partition_and_heal(runner: impl Fn(&Scenario) -> Run) {
    let cut = Action::Partition {
        left: vec![named(1)],
        right: vec![named(2)],
    };
    let heal = Action::Heal(crate::driver::HealTarget::Input(key("cut")));
    let run = both(
        &runner,
        Scenario {
            rules: vec![
                after_client_data("cut", 2 * MS, cut),
                after_client_data("heal", 1_002 * MS, heal),
            ],
            ..Scenario::default()
        },
    );
    check_partition(&run, "cut", "heal");
}

/// Opposite ±10% clock drift from the start.
pub(crate) fn opposite_drift(runner: impl Fn(&Scenario) -> Run) {
    let drift = |key, machine, ppb| Rule {
        key,
        when: When::At(0),
        action: Action::Drift {
            target: named(machine),
            rate: crate::clock::ClockRate::new(ppb).unwrap(),
        },
    };
    let run = both(
        &runner,
        Scenario {
            rules: vec![
                drift("fast-client", 2, 100_000_000),
                drift("slow-server", 1, -100_000_000),
            ],
            ..Scenario::default()
        },
    );
    check(&run);
    assert!(run.applied("fast-client").is_some() && run.applied("slow-server").is_some());
    assert_eq!(run.clock_rates(2), [100_000_000]);
    assert_eq!(run.clock_rates(1), [-100_000_000]);
    assert!(run.clock_rates(0).is_empty());
    // The drifted run is measurably different from the clean one.
    let clean = runner(&Scenario::default());
    check(&clean);
    assert_ne!(run.client_exit_at(), clean.client_exit_at());
    assert_ne!(run.events, clean.events);
}

/// The server paused at the barrier observing the client's UDP marker and
/// resumed 500 ms later. A paused server transmits nothing; its NIC accepts
/// the client's frames, and Resume lets it answer, so the client finishes
/// only after the resume.
pub(crate) fn paused_server(runner: impl Fn(&Scenario) -> Run) {
    let run = both(
        &runner,
        Scenario {
            rules: vec![
                after_client("pause", "udp ok", 0, Action::Pause(named(1))),
                after_client("resume", "udp ok", 500 * MS, Action::Resume(named(1))),
            ],
            ..Scenario::default()
        },
    );
    check(&run);
    let (pause_at, resume_at) = (
        run.applied("pause").unwrap(),
        run.applied("resume").unwrap(),
    );
    assert!(pause_at < resume_at);
    let (pause, resume) = (
        run.applied_index("pause").unwrap(),
        run.applied_index("resume").unwrap(),
    );
    let paused = run.frames_between(1, pause, resume);
    assert!(
        paused.is_empty(),
        "server transmitted while paused: {:?}",
        paused.iter().map(|f| f.0).collect::<Vec<_>>()
    );
    assert!(
        !run.frames_between(2, pause, resume).is_empty(),
        "the client sent nothing while the server was paused"
    );
    assert!(run.client_exit_at().unwrap() > resume_at);
}

fn crash_and_reboot_server(runner: &impl Fn(&Scenario) -> Run, server_disk: bool) -> Run {
    both(
        runner,
        Scenario {
            server_disk,
            rules: vec![
                after_client("crash", "udp ok", 0, Action::Crash(named(1))),
                after_client("reboot", "udp ok", 200 * MS, Action::Reboot(named(1))),
            ],
            ..Scenario::default()
        },
    )
}

/// The server crashed at the client's UDP marker and rebooted 200 ms later.
pub(crate) fn server_crash(runner: impl Fn(&Scenario) -> Run) {
    let run = crash_and_reboot_server(&runner, false);
    check(&run);
    // The rebooted server started its application again before serving TCP.
    assert_eq!(run.stdout[1].matches("NETPROBE ready").count(), 2);
    assert!(run.applied("crash").unwrap() < run.applied("reboot").unwrap());
}

/// The same crash with the server recording its progress on its disk.
pub(crate) fn disk_server_crash(runner: impl Fn(&Scenario) -> Run) {
    check_disk_crash(&crash_and_reboot_server(&runner, true));
}

#[cfg(test)]
mod tests {
    //! Portable checks of the harness itself with scripted machines; real
    //! guests run in the backend entries.
    use super::*;
    use crate::{
        machine::{ExitReason, RunContext, StopReason},
        pv::{Output, Request},
    };
    use std::collections::VecDeque;

    /// One scripted output: a stdout line or a transmitted frame.
    enum Emit {
        Line(&'static str),
        Frame(Vec<u8>),
    }

    /// Emits scripted stdout lines and frames at fixed times, then exits 0 if
    /// `exit`. A `busy` script never blocks and prints nothing.
    struct Script {
        lines: VecDeque<(u64, Emit)>,
        exit: bool,
        busy: bool,
        exited: bool,
    }
    impl Machine for Script {
        type Context = ();
        type Error = std::io::Error;
        type Snapshot = ();
        fn run(
            &mut self,
            context: &mut RunContext<'_, ()>,
            upto: Timestamp,
        ) -> Result<StopReason, Self::Error> {
            let now = context.clock.now();
            while let Some(input) = context.inbox.take() {
                if matches!(input.payload, Request::Crash) {
                    self.exited = true;
                }
            }
            if self.exited {
                return Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(now),
                    reason: ExitReason::Crashed,
                });
            }
            if self.busy {
                return Ok(StopReason::BoundaryReached);
            }
            let mut reached = now;
            while let Some((at, line)) = self.lines.pop_front_if(|(at, _)| *at <= upto.as_nanos()) {
                let at = at.max(now);
                reached = reached.max(at);
                let emit = |payload| Output {
                    at: Timestamp::from_nanos(at),
                    payload,
                };
                let payload = match line {
                    Emit::Line(line) => Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: line.as_bytes().to_vec(),
                    },
                    Emit::Frame(bytes) => Response::TransmitFrame { bytes },
                };
                context.outbox.emit(emit(payload)).unwrap();
                if self.lines.is_empty() && self.exit {
                    context
                        .outbox
                        .emit(emit(Response::ApplicationOutcome { code: 0 }))
                        .unwrap();
                }
            }
            Ok(StopReason::Blocked {
                reached: Timestamp::from_nanos(reached),
                deadline: self.lines.front().map(|(at, _)| Timestamp::from_nanos(*at)),
            })
        }
        fn snapshot(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    fn scripted(scenario: &Scenario, busy_client: bool) -> Run {
        let client = vec![
            (2_000_000, Emit::Line("NETPROBE udp ok attempts=1\n")),
            (5_000_000, Emit::Line("NETPROBE done\n")),
        ];
        scripted_client(scenario, client, busy_client)
    }

    /// The scripted oracle and server around a client emitting `client`.
    fn scripted_client(scenario: &Scenario, client: Vec<(u64, Emit)>, busy_client: bool) -> Run {
        let mut scripts = VecDeque::from([
            (vec![(0, Emit::Line("NETPROBE idle\n"))], false, false),
            (
                vec![(1_000_000, Emit::Line("NETPROBE ready port=7000\n"))],
                false,
                false,
            ),
            (client, true, busy_client),
        ]);
        run(b"probe", 128 << 20, scenario, move |definition| {
            assert!(NAMES.contains(&definition.name.as_str()));
            let (lines, exit, busy) = scripts.pop_front().unwrap();
            Ok(Script {
                lines: lines.into(),
                exit,
                busy,
                exited: false,
            })
        })
    }

    #[test]
    fn definitions_attach_only_server_and_client() {
        let [oracle, server, client] = scenario_definitions(b"probe", 128 << 20, false);
        assert_eq!(oracle.application.launch.argv, ["/application", "idle"]);
        assert!(oracle.network.is_none());
        assert!(server.network.is_some() && client.network.is_some());
        assert_eq!(
            [oracle.name, server.name, client.name],
            NAMES.map(String::from)
        );
        let [_, disk, _] = scenario_definitions(b"probe", 128 << 20, true);
        assert_eq!(disk, disk_definitions(b"probe", 128 << 20)[0]);
    }

    #[test]
    fn output_rules_schedule_at_the_observing_barrier_and_stop_at_client_exit() {
        let scenario = Scenario {
            rules: vec![
                Rule {
                    key: "pause-server",
                    when: When::Output {
                        machine: 2,
                        text: "udp ok",
                        after: 500_000,
                    },
                    action: Action::Pause(named(1)),
                },
                Rule {
                    key: "drift-client",
                    when: When::At(0),
                    action: Action::Drift {
                        target: named(2),
                        rate: crate::clock::ClockRate::new(100_000_000).unwrap(),
                    },
                },
            ],
            ..Scenario::default()
        };
        let run = scripted(&scenario, false);
        assert!(run.stdout[2].ends_with("NETPROBE done\n"));
        assert_eq!(run.client_exit(), Some(0));
        assert_eq!(run.stdout[0], "NETPROBE idle\n");
        assert_eq!(run.scheduled[0], ("drift-client", 0));
        // The marker commits by the 2 ms round; the pause follows it.
        let (key, seen) = run.scheduled[1];
        assert_eq!(key, "pause-server");
        assert_eq!(run.applied("pause-server"), Some(seen + 500_000));
        assert!(matches!(
            run.target("pause-server"),
            Some(FaultTarget::Machine { machine, .. }) if machine == run.ids[1]
        ));
        assert!(run.applied("drift-client").is_some());
        assert_eq!(run.clock_rates(2), [100_000_000]);
        assert!(run.clock_rates(1).is_empty());
        // Publication order places the settlement after the marker's output.
        let pause = run.applied_index("pause-server").unwrap();
        assert!(run.frames_between(1, pause, run.events.len()).is_empty());
        assert!(run.client_exit_at().unwrap() > seen);
        let wide = scripted(
            &Scenario {
                workers: 4,
                ..scenario
            },
            false,
        );
        check_worker_invariance(&run, &wide);
    }

    /// A client TCP data segment, client to server, with `sequence`.
    fn client_data(sequence: u32) -> Vec<u8> {
        let mut bytes = mac(0).to_vec();
        bytes.extend(mac(1));
        bytes.extend([0x08, 0x00]);
        let payload = 100;
        let mut ip = vec![
            0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 6, 0, 0, 10, 0, 0, 2, 10, 0, 0, 1,
        ];
        ip[2..4].copy_from_slice(&((20 + 20 + payload) as u16).to_be_bytes());
        let mut tcp = vec![0; 20];
        tcp[0..2].copy_from_slice(&40000u16.to_be_bytes());
        tcp[2..4].copy_from_slice(&7000u16.to_be_bytes());
        tcp[4..8].copy_from_slice(&sequence.to_be_bytes());
        tcp[12] = 5 << 4;
        bytes.extend(ip);
        bytes.extend(tcp);
        bytes.extend(vec![1; payload]);
        bytes
    }

    // bd-c25: on x86 TCG the client sent its whole stream within about 10 ms
    // of its ARP marker, so a cut 20 ms after that marker landed after the
    // stream and the heal never applied before the client exited.
    #[test]
    fn tcp_data_rules_land_inside_a_stream_that_outruns_a_marker_delay() {
        // ARP marker at 2 ms, then data from 3 ms to 7.5 ms, done at 8 ms.
        let mut client = vec![
            (2_000_000, Emit::Line("NETPROBE udp ok attempts=1\n")),
            (2_000_000, Emit::Line("NETPROBE arp ok\n")),
        ];
        for (index, at) in (3_000_000..=7_500_000).step_by(500_000).enumerate() {
            client.push((at, Emit::Frame(client_data(1 + 100 * index as u32))));
        }
        client.push((8_000_000, Emit::Line("NETPROBE done\n")));
        let scenario = Scenario {
            rules: vec![
                Rule {
                    key: "after-data",
                    when: When::TcpData {
                        machine: 2,
                        after: 2_000_000,
                    },
                    action: Action::Pause(named(1)),
                },
                // The old placement, a fixed delay after the ARP marker.
                Rule {
                    key: "after-marker",
                    when: When::Output {
                        machine: 2,
                        text: "arp ok",
                        after: 20_000_000,
                    },
                    action: Action::Pause(named(0)),
                },
            ],
            ..Scenario::default()
        };
        let run = scripted_client(&scenario, client, false);
        assert_eq!(run.client_exit(), Some(0));
        assert_eq!(run.tcp_data(2).len(), 10);
        // Scheduled at the barrier that first observed published data, which
        // cannot precede the first segment.
        let scheduled = &run.scheduled;
        let &(_, seen) = scheduled
            .iter()
            .find(|(key, _)| *key == "after-data")
            .unwrap();
        assert!((3_000_000..7_500_000).contains(&seen), "{scheduled:?}");
        let applied = run.applied("after-data").unwrap();
        assert_eq!(applied, seen + 2_000_000);
        // Data was still being published after the action applied.
        let index = run.applied_index("after-data").unwrap();
        let later = run.frames_between(2, index, run.events.len());
        assert!(
            later.iter().any(|(_, bytes)| segment(2, bytes).is_some()),
            "no client data after {applied}"
        );
        // The marker rule was scheduled but the client exited first.
        assert!(scheduled.iter().any(|(key, _)| *key == "after-marker"));
        assert_eq!(run.applied("after-marker"), None);
    }

    #[test]
    fn a_client_that_never_exits_stops_at_the_simulated_limit() {
        let run = scripted(
            &Scenario {
                limit: 30_000_000,
                ..Scenario::default()
            },
            true,
        );
        assert_eq!(
            run.outcome,
            StepOutcome::BarrierReached {
                at: Timestamp::from_nanos(30_000_000)
            }
        );
        assert_eq!(run.client_exit(), None);
        assert!(std::panic::catch_unwind(|| check(&run)).is_err());
    }

    #[test]
    fn retransmitted_tcp_data_is_recognized_by_flow_and_sequence() {
        let frame = |id: u16, sequence: u32, payload: usize| {
            let mut bytes = vec![0; 14];
            bytes[12..14].copy_from_slice(&[0x08, 0x00]);
            let total = (20 + 20 + payload) as u16;
            let mut ip = vec![
                0x45, 0, 0, 0, 0, 0, 0x40, 0, 64, 6, 0, 0, 10, 0, 0, 2, 10, 0, 0, 1,
            ];
            ip[2..4].copy_from_slice(&total.to_be_bytes());
            ip[4..6].copy_from_slice(&id.to_be_bytes());
            let mut tcp = vec![0; 20];
            tcp[0..2].copy_from_slice(&40000u16.to_be_bytes());
            tcp[2..4].copy_from_slice(&7000u16.to_be_bytes());
            tcp[4..8].copy_from_slice(&sequence.to_be_bytes());
            tcp[12] = 5 << 4;
            bytes.extend(ip);
            bytes.extend(tcp);
            bytes.extend(vec![1; payload]);
            bytes
        };
        // The same flow and sequence under another IP ID is one segment; a
        // pure acknowledgment carries no data and is never counted.
        assert_eq!(
            segment(2, &frame(1, 100, 10)),
            segment(2, &frame(9, 100, 10))
        );
        assert!(segment(2, &frame(1, 100, 0)).is_none());
        // Malformed, truncated and fragmented frames are never segments.
        let good = frame(1, 100, 10);
        let mutate = |at: usize, value: u8| {
            let mut bytes = good.clone();
            bytes[at] = value;
            bytes
        };
        for bad in [
            mutate(14, 0x44),              // IHL of 16 bytes
            mutate(14, 0x65),              // not IPv4
            mutate(17, 255),               // total length past the frame
            mutate(17, 30),                // total length inside the TCP header
            mutate(20, 0x20),              // more fragments
            mutate(21, 1),                 // nonzero fragment offset
            mutate(23, 17),                // UDP
            mutate(14 + 20 + 12, 4 << 4),  // TCP header of 16 bytes
            mutate(14 + 20 + 12, 15 << 4), // TCP header past the segment
            good[..14 + 19].to_vec(),
            good[..13].to_vec(),
        ] {
            assert!(segment(2, &bad).is_none(), "{bad:?}");
        }
        assert_ne!(
            segment(2, &frame(1, 100, 10)),
            segment(2, &frame(1, 110, 10))
        );

        // All stamps match: only publication order selects the window. A
        // repeat inside it must retain history from before its lower bound.
        let mut run = scripted(&Scenario::default(), false);
        let template = run.events.iter().find(|event| {
            matches!(&event.kind, EventKind::Output(output) if output.machine == run.ids[2])
        }).unwrap().clone();
        run.events = [100, 200, 100, 200]
            .into_iter()
            .enumerate()
            .map(|(i, sequence)| {
                let mut event = template.clone();
                event.sequence = i as u64;
                let EventKind::Output(output) = &mut event.kind else {
                    unreachable!()
                };
                output.payload = Response::TransmitFrame {
                    bytes: frame(i as u16, sequence, 10),
                };
                event
            })
            .collect();
        assert_eq!(run.tcp_retransmissions(), 2);
        assert_eq!(run.tcp_retransmissions_between(1, 2), 0);
        assert_eq!(run.tcp_retransmissions_between(1, 3), 1);
        assert_eq!(run.tcp_retransmissions_between(2, 4), 1);
        assert_eq!(run.tcp_retransmissions_between(1, 4), 2);
        assert_eq!(run.frames_between(2, 1, 3).len(), 1);
    }
}
