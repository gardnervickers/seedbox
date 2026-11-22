//! What does not depend on where rounds end, on scripted machines.
//!
//! Each scenario runs once with the Driver's own round boundaries and once
//! under each of several seeded [`StopPolicy`]s, which end rounds early at
//! host-drawn times. The runs must agree on:
//!
//! - the event stream, with each event's `position` removed (it counts commits);
//! - Driver policy evidence, with the quantum end and the round counters
//!   written as zero (`Barrier::write_policy_evidence_without_stops`);
//! - frame facts as a multiset, without positions (their interleaving follows
//!   commits);
//! - each machine's own record of what it observed: every input with the
//!   machine time it was offered, every draw from its RNG stream and every
//!   output it emitted. The bounds of its runs are not part of it.
//!
//! The ignored tests fail today: a stop policy changes their results. Each
//! names the rule of the coordination design that will make it pass.
use seedbox_driver::definition::{MacAddress, MachineDefinition, NetworkInterface};
use seedbox_driver::driver::faults::Engine;
use seedbox_driver::driver::{
    Action, Driver, HealTarget, MachineId, NetworkConfig, NetworkFaults, NetworkPolicy,
    NetworkSetup, StepOutcome, Target,
};
use seedbox_driver::test_support::{definition, key, StopPolicy};
use seedbox_engine_protocol::property::Frame;
use seedbox_machine::{
    clock::ClockRate,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{CommandResult, Output, Request, Response, SimulationControlId, Stream},
};
use seedbox_spec::{backend::CpuBackend, definition::Architecture};
use std::{
    collections::VecDeque,
    num::NonZeroU32,
    sync::{Arc, Mutex},
    time::Duration,
};

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
/// Stop-policy seeds every scenario runs under, besides the Driver's own stops.
const SEEDS: [u64; 6] = [1, 2, 3, 0x5eed, 0xdead_beef, u64::MAX];
/// Frames with a tag at or above this are not echoed, which bounds chains.
const ECHO_LIMIT: u8 = 40;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn mac(n: usize) -> [u8; 6] {
    [2, 0, 0, 0, 0, n as u8 + 1]
}
const BROADCAST: [u8; 6] = [0xff; 6];
fn frame(to: [u8; 6], tag: u8, body: &[u8]) -> Vec<u8> {
    let mut bytes = to.to_vec();
    bytes.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0x88, 0xb5, tag]);
    bytes.extend_from_slice(body);
    bytes
}
fn call(request: u32, op: &str) -> Response {
    call_with(request, &format!(r#""op":"{op}""#))
}
/// A guest call with the operation's own fields, such as `"op":"stop",...`.
fn call_with(request: u32, fields: &str) -> Response {
    Response::SimulationControl {
        id: SimulationControlId {
            session: NonZeroU32::new(1).unwrap(),
            request: NonZeroU32::new(request).unwrap(),
        },
        payload: format!(r#"{{"version":1,"id":{request},{fields}}}"#).into_bytes(),
    }
}
/// A property record on session `session`.
fn property(session: u32, seq: u64, record: &str) -> Response {
    Response::Property {
        frame: Frame {
            session,
            seq,
            payload: record.as_bytes().to_vec(),
        },
    }
}

/// What a scripted machine observed, in its own order.
type Seen = Arc<Mutex<Vec<String>>>;

/// Emits its script at machine times, blocking on a timer until the next
/// entry unless it is busy. It echoes each frame it receives one nanosecond
/// later, with a body drawn from its RNG stream, when it has an echo target.
/// When it acknowledges, it acknowledges each guest command one nanosecond
/// after taking it: a realtime step by an odd number of nanoseconds is
/// refused. It holds at a
/// declared breakpoint the way a PV machine does, until it is resumed.
struct Script {
    sends: VecDeque<(u64, Response)>,
    busy: bool,
    echo: Option<[u8; 6]>,
    acks: bool,
    exit: Option<u64>,
    exited: bool,
    declared: Vec<String>,
    held: bool,
    reached: u64,
    seen: Seen,
}

impl Script {
    /// Queue `payload` for emission at machine time `when`, behind entries
    /// already due then.
    fn later(&mut self, when: u64, payload: Response) {
        let index = self.sends.partition_point(|(t, _)| *t <= when);
        self.sends.insert(index, (when, payload));
    }
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
        let start = context.clock.now();
        self.reached = self.reached.max(start);
        let seen = self.seen.clone();
        let mut seen = seen.lock().unwrap();
        while let Some(input) = context.inbox.take() {
            seen.push(format!("in {} {:?}", input.at.as_nanos(), input.payload));
            let ack = |id, result| Response::CommandAcknowledged { id, result };
            match input.payload {
                Request::Crash => {
                    self.exited = true;
                    self.held = false;
                }
                Request::Reboot => self.exited = false,
                Request::ReceiveFrame { bytes } if !self.exited => {
                    let mut body = [0; 4];
                    context.rng.fill_bytes(&mut body);
                    seen.push(format!("draw {body:?}"));
                    let tag = bytes[14];
                    if let Some(to) = self.echo.filter(|_| tag < ECHO_LIMIT) {
                        let reply = Response::TransmitFrame {
                            bytes: frame(to, tag + 1, &body),
                        };
                        self.later(start + 1, reply);
                    }
                }
                Request::Application { id, .. } if self.acks && !self.exited => {
                    self.later(start + 1, ack(id, CommandResult::Completed));
                }
                Request::StepRealtime { id, nanoseconds } if self.acks && !self.exited => {
                    let result = if nanoseconds.get() % 2 == 0 {
                        CommandResult::Completed
                    } else {
                        CommandResult::Rejected
                    };
                    self.later(start + 1, ack(id, result));
                }
                Request::DeclareBreaks { names } => self.declared = names,
                Request::ResumeBreak => self.held = false,
                _ => {}
            }
        }
        if self.exited {
            return Ok(StopReason::Exited {
                reached: at(self.reached),
                reason: ExitReason::Crashed,
            });
        }
        if self.held {
            return Ok(StopReason::Blocked {
                reached: at(self.reached),
                deadline: None,
            });
        }
        let end = upto.as_nanos();
        let exit = self
            .exit
            .filter(|&exit| exit <= end)
            .map(|exit| exit.max(start));
        while self
            .sends
            .front()
            .is_some_and(|(t, _)| *t <= exit.unwrap_or(end))
        {
            let (t, payload) = self.sends.pop_front().unwrap();
            let t = t.max(start);
            if let Response::Break { name } = &payload {
                // An undeclared name never suspends and writes nothing.
                if !self.declared.contains(name) {
                    continue;
                }
                self.reached = self.reached.max(t);
                self.held = true;
                emit(context, &mut seen, t, payload);
                return Ok(StopReason::Blocked {
                    reached: at(self.reached),
                    deadline: None,
                });
            }
            self.reached = self.reached.max(t);
            emit(context, &mut seen, t, payload);
        }
        if let Some(exit) = exit {
            self.exited = true;
            self.reached = exit;
            seen.push(format!("exit {exit}"));
            return Ok(StopReason::Exited {
                reached: at(exit),
                reason: ExitReason::Shutdown,
            });
        }
        if self.busy {
            self.reached = end;
            return Ok(StopReason::BoundaryReached);
        }
        let deadline = self.sends.front().map(|(t, _)| *t).into_iter();
        Ok(StopReason::Blocked {
            reached: at(self.reached),
            deadline: deadline.chain(self.exit).min().map(at),
        })
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn emit(context: &mut RunContext<'_, ()>, seen: &mut Vec<String>, t: u64, payload: Response) {
    seen.push(format!("out {t} {payload:?}"));
    context.outbox.emit(Output { at: at(t), payload }).unwrap();
}

/// One machine of a scenario. Machine 0 is the oracle.
#[derive(Clone, Default)]
struct Spec {
    sends: Vec<(u64, Response)>,
    busy: bool,
    echo: Option<[u8; 6]>,
    acks: bool,
    exit: Option<u64>,
}
impl Spec {
    fn busy() -> Self {
        Self {
            busy: true,
            ..Self::default()
        }
    }
    fn idle() -> Self {
        Self::default()
    }
    fn echo(mut self, to: [u8; 6]) -> Self {
        self.echo = Some(to);
        self
    }
    /// Acknowledge every guest command one nanosecond after taking it.
    fn acks(mut self) -> Self {
        self.acks = true;
        self
    }
    /// A frame to `to` every `period` from `first` until `until`.
    fn every(mut self, first: u64, period: u64, until: u64, to: [u8; 6], tag: u8) -> Self {
        let mut t = first;
        while t < until {
            self.sends.push((
                t,
                Response::TransmitFrame {
                    bytes: frame(to, tag, &t.to_le_bytes()),
                },
            ));
            t += period;
        }
        self.sends.sort_by_key(|(t, _)| *t);
        self
    }
    fn emit(mut self, t: u64, payload: Response) -> Self {
        self.sends.push((t, payload));
        self.sends.sort_by_key(|(t, _)| *t);
        self
    }
    fn exit(mut self, t: u64) -> Self {
        self.exit = Some(t);
        self
    }
}

struct Scenario {
    machines: Vec<Spec>,
    network: NetworkSetup,
    inputs: Vec<(u64, &'static str, Action)>,
    end: u64,
    /// Run on an engine whose machines hold at breakpoints (PV).
    breaks: bool,
}

/// Everything a run produced that must not depend on where rounds ended.
#[derive(Debug, PartialEq, Eq)]
struct Results {
    outcome: String,
    events: Vec<String>,
    evidence: Vec<u8>,
    frames: Vec<String>,
    seen: Vec<Vec<String>>,
}

fn networked(index: usize) -> MachineDefinition {
    let mut definition = definition(format!("m{index}"));
    definition.network = Some(
        NetworkInterface::new(
            MacAddress::new(mac(index)).unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, index as u8 + 1),
            24,
        )
        .unwrap(),
    );
    definition
}

/// Run `scenario` to its end. Returns its results and how many barriers the
/// host saw on the way.
fn run(scenario: &Scenario, policy: Option<u64>) -> (Results, usize) {
    let seen: Vec<Seen> = scenario.machines.iter().map(|_| Seen::default()).collect();
    let mut machines: VecDeque<Script> = scenario
        .machines
        .iter()
        .zip(&seen)
        .map(|(spec, seen)| Script {
            sends: spec.sends.iter().cloned().collect(),
            busy: spec.busy,
            echo: spec.echo,
            acks: spec.acks,
            exit: spec.exit,
            exited: false,
            declared: Vec::new(),
            held: false,
            reached: 0,
            seen: seen.clone(),
        })
        .collect();
    let definitions: Vec<_> = (0..machines.len()).map(networked).collect();
    let mut driver = Driver::new_seeded(
        at(0),
        Duration::from_millis(1),
        2,
        7,
        || Ok(()),
        definitions[0].clone(),
        definitions[1..].to_vec(),
        Vec::new(),
        scenario.network.clone().with_frame_capacity(1 << 20),
        move |_| Ok(machines.pop_front().expect("one machine per definition")),
    )
    .unwrap();
    if scenario.breaks {
        driver.set_engine(Engine {
            backend: CpuBackend::Pv,
            architecture: Architecture::Aarch64,
        });
    }
    driver
        .with_barrier(|barrier| {
            for (t, name, action) in &scenario.inputs {
                barrier.schedule(at(*t), key(name), action.clone())?;
            }
            Ok(())
        })
        .unwrap();
    driver.set_stop_policy(policy.map(StopPolicy::seeded));
    let mut barriers = 0;
    let outcome = loop {
        let outcome = driver.step(at(scenario.end)).unwrap();
        barriers += 1;
        match outcome {
            StepOutcome::BarrierReached { at } if at.as_nanos() < scenario.end => {}
            other => break other,
        }
    };
    let drained = driver.drain_observations();
    let events = drained
        .events
        .iter()
        .map(|event| {
            format!(
                "#{} at {} {:?}",
                event.sequence,
                event.at.as_nanos(),
                event.kind
            )
        })
        .collect();
    let mut frames: Vec<String> = drained
        .frames
        .facts
        .iter()
        .map(|fact| {
            format!(
                "{:?}->{:?} sent {:?} due {:?} {} bytes {:?}",
                fact.from, fact.to, fact.sent, fact.due, fact.bytes, fact.fate
            )
        })
        .collect();
    assert_eq!(drained.frames.omitted, 0, "frame facts retained");
    frames.sort();
    let evidence = driver
        .with_barrier(|barrier| {
            let mut evidence = Vec::new();
            barrier
                .write_policy_evidence_without_stops(&mut evidence)
                .unwrap();
            Ok(evidence)
        })
        .unwrap();
    let results = Results {
        outcome: format!("{outcome:?}"),
        events,
        evidence,
        frames,
        seen: seen
            .iter()
            .map(|seen| seen.lock().unwrap().clone())
            .collect(),
    };
    (results, barriers)
}

/// The first difference between two lists, for a readable failure.
fn first_difference(what: &str, a: &[String], b: &[String]) -> Option<String> {
    let index = a.iter().zip(b).position(|(a, b)| a != b);
    match index {
        Some(i) => Some(format!(
            "{what} differs at {i}:\n  own stops: {}\n  policy:    {}",
            a[i], b[i]
        )),
        None if a.len() != b.len() => Some(format!(
            "{what} lengths differ: own stops {}, policy {}",
            a.len(),
            b.len()
        )),
        None => None,
    }
}

/// Every difference between `own` and `other`, empty when they agree.
fn differences(own: &Results, other: &Results) -> Vec<String> {
    let mut found = Vec::new();
    if own.outcome != other.outcome {
        found.push(format!(
            "outcome: own stops {}, policy {}",
            own.outcome, other.outcome
        ));
    }
    found.extend(first_difference("event stream", &own.events, &other.events));
    if own.evidence != other.evidence {
        found.push("policy evidence differs".into());
    }
    found.extend(first_difference("frame facts", &own.frames, &other.frames));
    for (index, (a, b)) in own.seen.iter().zip(&other.seen).enumerate() {
        found.extend(first_difference(&format!("machine {index}"), a, b));
    }
    found
}

/// Run `scenario` with the Driver's own stops and under every policy seed.
/// Returns the results under the Driver's own stops, and what differed from
/// them under each seed.
fn compare(scenario: &Scenario) -> (Results, Vec<(u64, Vec<String>)>) {
    let (own, own_barriers) = run(scenario, None);
    let mut found = Vec::new();
    for seed in SEEDS {
        let (other, barriers) = run(scenario, Some(seed));
        assert!(
            barriers > own_barriers,
            "seed {seed} added stops: {barriers} against {own_barriers}"
        );
        let differences = differences(&own, &other);
        if !differences.is_empty() {
            found.push((seed, differences));
        }
    }
    (own, found)
}

/// Assert that no stop policy changes the scenario's results, and that its
/// machines observed at least `at_least` things. Returns the results.
fn holds(scenario: Scenario, at_least: usize) -> Results {
    let (own, found) = compare(&scenario);
    assert!(found.is_empty(), "stop policy changed results: {found:#?}");
    let observed: usize = own.seen.iter().map(Vec::len).sum();
    assert!(
        observed >= at_least,
        "scenario exercised {observed} observations, wanted {at_least}"
    );
    own
}

/// How many frame facts have this fate.
fn fates(results: &Results, fate: &str) -> usize {
    results
        .frames
        .iter()
        .filter(|fact| fact.ends_with(fate))
        .count()
}

fn jittery() -> NetworkSetup {
    NetworkConfig::new(
        Duration::from_micros(100),
        Duration::from_micros(40),
        50_000,
    )
    .unwrap()
    .into()
}

#[test]
fn frames_timers_and_echo_chains_do_not_depend_on_stops() {
    let end = 20 * MS;
    let own = holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .echo(mac(1))
                    .every(13 * US, 397 * US, end, BROADCAST, 0),
                Spec::idle()
                    .echo(mac(2))
                    .every(101 * US + 7, 1_301 * US, end, mac(0), 10),
                Spec::idle()
                    .echo(mac(0))
                    .every(257 * US + 3, 709 * US, end, mac(1), 20),
            ],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        400,
    );
    assert!(fates(&own, "LossDropped") > 0, "loss exercised");
}

#[test]
fn partitions_shapes_and_frames_in_flight_do_not_depend_on_stops() {
    let end = 20 * MS;
    let links = |from: usize, to: usize| {
        (
            vec![Target::Name(format!("m{from}"))],
            vec![Target::Name(format!("m{to}"))],
        )
    };
    let (from, to) = links(1, 2);
    let shape = Action::Shape {
        from,
        to,
        policy: NetworkPolicy {
            latency_ns: 300 * US,
            jitter_ns: 50 * US,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        },
        network_faults: NetworkFaults {
            duplicate_ppm: 500_000,
            reorder_ppm: 300_000,
            reorder_delay_ns: 200 * US,
            latency_spike_ppm: 100_000,
            latency_spike_ns: 400 * US,
            ..NetworkFaults::default()
        },
    };
    let (left, right) = links(0, 1);
    let own = holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .echo(mac(2))
                    .every(11 * US, 211 * US, end, mac(1), 0),
                Spec::idle()
                    .echo(mac(2))
                    .every(53 * US + 1, 307 * US, end, mac(0), 10),
                Spec::busy()
                    .echo(mac(1))
                    .every(97 * US + 5, 401 * US, end, BROADCAST, 20),
            ],
            network: jittery(),
            inputs: vec![
                // Frames sent just before the cut are still in flight across it.
                (3 * MS, "cut", Action::Partition { left, right }),
                (5 * MS, "shape", shape),
                (
                    9 * MS,
                    "heal-cut",
                    Action::Heal(HealTarget::Input(key("cut"))),
                ),
                (
                    14 * MS,
                    "heal-shape",
                    Action::Heal(HealTarget::Input(key("shape"))),
                ),
            ],
            end,
            breaks: false,
        },
        300,
    );
    assert!(fates(&own, "PartitionDropped") > 0, "the cut exercised");
    let scheduled: Vec<_> = own
        .frames
        .iter()
        .filter(|fact| fact.ends_with("Scheduled"))
        .map(|fact| fact.split(" due ").next().unwrap())
        .collect();
    let duplicated = scheduled.windows(2).filter(|w| w[0] == w[1]).count();
    assert!(duplicated > 0, "duplicates exercised");
}

#[test]
fn pause_crash_and_reboot_do_not_depend_on_stops() {
    let end = 20 * MS;
    let m = |n: usize| Target::Name(format!("m{n}"));
    let own = holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .echo(mac(1))
                    .every(17 * US, 293 * US, end, BROADCAST, 0),
                Spec::idle()
                    .echo(mac(2))
                    .every(71 * US + 3, 503 * US, end, mac(0), 10),
                Spec::busy()
                    .echo(mac(0))
                    .every(131 * US + 9, 601 * US, end, mac(1), 20),
            ],
            network: jittery(),
            inputs: vec![
                // A paused receiver admits its frames at their arrival.
                (2 * MS, "pause", Action::Pause(m(1))),
                (6 * MS + 333, "resume", Action::Resume(m(1))),
                (8 * MS, "crash", Action::Crash(m(2))),
                (11 * MS + 17, "reboot", Action::Reboot(m(2))),
            ],
            end,
            breaks: false,
        },
        300,
    );
    assert!(
        fates(&own, "ReceiverDropped") > 0,
        "the crashed receiver exercised"
    );
}

/// A busy machine with a fast clock takes its frames and emits its output at
/// exact machine times inside its runs, and a blocked machine with a slow
/// clock wakes at the one simulation time whose machine time is its deadline.
#[test]
fn drifting_clocks_do_not_depend_on_stops() {
    let end = 20 * MS;
    let drift = |name: &str, ppb| Action::Drift {
        target: Target::Name(name.into()),
        rate: ClockRate::new(ppb).unwrap(),
    };
    holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .echo(mac(1))
                    .every(19 * US, 331 * US, end, BROADCAST, 0),
                Spec::busy()
                    .echo(mac(2))
                    .every(41 * US + 7, 389 * US, end, mac(0), 10),
                Spec::idle()
                    .echo(mac(0))
                    .every(83 * US + 1, 457 * US, end, mac(1), 20),
            ],
            network: jittery(),
            inputs: vec![
                (3 * MS, "fast", drift("m1", 100_000_000)),
                (3 * MS, "slow", drift("m2", -100_000_000)),
            ],
            end,
            breaks: false,
        },
        300,
    );
}

/// With a controller, a guest call is admitted one control latency after its
/// output and replied one more after that, whatever the commit.
#[test]
fn controller_calls_do_not_depend_on_stops() {
    let end = 20 * MS;
    let network = NetworkSetup::from(
        NetworkConfig::new(Duration::from_micros(100), Duration::from_micros(40), 0).unwrap(),
    )
    .with_controller(Some(MachineId::ORACLE), Duration::from_micros(250))
    .unwrap();
    let mut oracle = Spec::busy().every(23 * US, 449 * US, end, BROADCAST, 0);
    for (request, t) in (1..).zip((0..12).map(|n| 300 * US + 1_501 * US * n)) {
        let op = if request % 2 == 0 { "now" } else { "inspect" };
        oracle = oracle.emit(t, call(request, op));
    }
    let own = holds(
        Scenario {
            machines: vec![
                oracle,
                Spec::idle()
                    .echo(mac(0))
                    .every(61 * US + 3, 677 * US, end, mac(0), 10),
            ],
            network,
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        100,
    );
    let replies = own
        .events
        .iter()
        .filter(|event| event.contains("ReplyAdmitted"))
        .count();
    assert_eq!(replies, 12, "every call answered");
}

/// C1: without a controller, a guest call is admitted one floor after its
/// output, and answered there, whatever the commit that carried it.
#[test]
fn sdk_calls_do_not_depend_on_stops() {
    let end = 10 * MS;
    let mut oracle = Spec::busy();
    for (request, t) in (1..).zip((0..8).map(|n| 300 * US + 1_013 * US * n)) {
        oracle = oracle.emit(t, call(request, "now"));
    }
    holds(
        Scenario {
            machines: vec![oracle, Spec::busy()],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        1,
    );
}

/// The SDK replies in `results` to the oracle, with their machine times.
fn replies(results: &Results) -> Vec<&String> {
    results.seen[0]
        .iter()
        .filter(|seen| seen.contains("SimulationReply"))
        .collect()
}

/// Whether a settlement whose event contains `text` has its outcome at `at`.
fn settled(results: &Results, text: &str, at: u64) -> bool {
    let outcome = format!("outcome_at: Some(Timestamp({at}))");
    results.events.iter().any(|event| {
        event.contains("FaultSettled") && event.contains(text) && event.contains(&outcome)
    })
}

/// How many events contain `text`.
fn events_with(results: &Results, text: &str) -> usize {
    results
        .events
        .iter()
        .filter(|event| event.contains(text))
        .count()
}

/// C1: a guest command that waits for its machine's readiness is sent one
/// floor after the `Ready`, and a heal queued behind a realtime step one floor
/// after the step's acknowledgement. A refused step takes its queued heal
/// along at the refusal. Replies to the oracle's commands follow their
/// acknowledgements, or their refusal, by one floor.
#[test]
fn acknowledgements_and_released_commands_do_not_depend_on_stops() {
    let end = 20 * MS;
    let m1 = || Target::Name("m1".into());
    let step = |nanoseconds: i64| Action::StepRealtime {
        target: m1(),
        nanoseconds: std::num::NonZeroI64::new(nanoseconds).unwrap(),
    };
    let on = |op: &str, machine: &str| format!(r#""op":"{op}","target":{{"machine":"{machine}"}}"#);
    let oracle = Spec::busy()
        .every(29 * US, 613 * US, end, BROADCAST, 0)
        .emit(6 * MS + 3, call_with(1, &on("stop", "m1")))
        .emit(8 * MS + 11, call_with(2, &on("restart", "m1")))
        .emit(10 * MS + 7, call_with(3, &on("pause_process", "m1")))
        .emit(10 * MS + 501, call_with(4, &on("resume_process", "m1")))
        .emit(
            12 * MS + 13,
            call_with(
                5,
                r#""op":"clock_skew","target":{"machine":"m1"},"offset_ns":-3001"#,
            ),
        )
        // Before m2 is ready: sent one floor after its Ready.
        .emit(13 * MS + 1, call_with(6, &on("stop", "m2")));
    let own = holds(
        Scenario {
            machines: vec![
                oracle,
                Spec::idle()
                    .acks()
                    .echo(mac(0))
                    .every(71 * US + 3, 503 * US, end, mac(0), 10)
                    .emit(MS + 17, Response::Ready),
                Spec::busy()
                    .acks()
                    .every(131 * US + 9, 601 * US, end, mac(1), 20)
                    .emit(14 * MS + 5, Response::Ready),
            ],
            network: jittery(),
            inputs: vec![
                // Both wait for m1's Ready; the heal waits for the step too.
                (300 * US, "skew", step(5_000)),
                (
                    400 * US,
                    "heal-skew",
                    Action::Heal(HealTarget::Input(key("skew"))),
                ),
                // Refused by the guest, which takes the heal behind it along.
                (4 * MS, "refused", step(-7_001)),
                (
                    4 * MS,
                    "heal-refused",
                    Action::Heal(HealTarget::Input(key("refused"))),
                ),
                (
                    5 * MS + 9,
                    "pause-process",
                    Action::PauseProcess {
                        target: m1(),
                        deadline: Duration::from_millis(1),
                    },
                ),
            ],
            end,
            breaks: false,
        },
        300,
    );
    assert_eq!(replies(&own).len(), 6, "every call answered");
    assert_eq!(events_with(&own, "status: Rejected"), 2, "two refusals");
    assert_eq!(events_with(&own, "status: Unavailable"), 1, "queued heal");
    assert_eq!(events_with(&own, "status: Applied"), 8);
    // Each released command is sent one floor (100 µs) after what released
    // it and acknowledged one nanosecond later.
    let floor = 100 * US;
    assert!(settled(&own, "StepRealtime(5000)", MS + 17 + floor + 1));
    assert!(settled(&own, "action: Heal", MS + 17 + 2 * (floor + 1)));
    assert!(settled(
        &own,
        "machine: MachineId(2)",
        14 * MS + 5 + floor + 1
    ));
}

/// C1: a flush record is answered one floor after it, whatever the commit
/// that carried it.
#[test]
fn property_flushes_do_not_depend_on_stops() {
    let end = 20 * MS;
    let assert = r#"{"assert":{"id":"p","kind":"always","condition":true}}"#;
    let flush = r#"{"flush":{}}"#;
    let records = |spec: Spec, session: u32, first: u64, period: u64| {
        let mut spec = spec;
        let mut seq = 0;
        let mut t = first;
        while t < end {
            seq += 1;
            spec = spec.emit(t, property(session, seq, assert));
            seq += 1;
            spec = spec.emit(t + 1, property(session, seq, flush));
            t += period;
        }
        spec
    };
    let own = holds(
        Scenario {
            machines: vec![
                records(
                    Spec::busy().every(37 * US, 557 * US, end, BROADCAST, 0),
                    1,
                    211 * US + 5,
                    1_303 * US,
                ),
                records(
                    records(
                        Spec::idle()
                            .echo(mac(0))
                            .every(97 * US + 1, 709 * US, end, mac(0), 10),
                        1,
                        151 * US + 3,
                        1_709 * US,
                    ),
                    2,
                    677 * US + 1,
                    2_111 * US,
                ),
            ],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        100,
    );
    let answered: usize = own
        .seen
        .iter()
        .flatten()
        .filter(|seen| seen.contains("PropertyReply"))
        .count();
    assert!(answered >= 30, "flushes answered: {answered}");
}

/// C1: a hit is numbered when it is committed and answers a parked
/// `wait_break` one floor after the hit, whatever the commit that carried
/// it. A hit nobody waits for is queued for the next `wait_break`, which
/// returns it at its own admission.
#[test]
fn breakpoint_hits_and_wait_break_replies_do_not_depend_on_stops() {
    let end = 20 * MS;
    let declare = r#""op":"declare_breaks","names":["leader"]"#;
    let wait = |timeout: u64| format!(r#""op":"wait_break","timeout_ns":{timeout}"#);
    let resume = |id: u32| format!(r#""op":"resume_break","break_id":{id}"#);
    let hit = || Response::Break {
        name: "leader".into(),
    };
    let oracle = Spec::busy()
        .every(23 * US, 691 * US, end, BROADCAST, 0)
        .emit(100 * US + 3, call_with(1, declare))
        .emit(200 * US + 7, call_with(2, &wait(50 * MS)))
        .emit(3 * MS + 1, call_with(3, &resume(1)))
        .emit(6 * MS + 11, call_with(4, &wait(50 * MS)))
        .emit(7 * MS + 5, call_with(5, &resume(2)))
        // Times out: nothing is hit before it.
        .emit(8 * MS + 3, call_with(6, &wait(MS)))
        .emit(11 * MS + 9, call_with(7, &wait(50 * MS)))
        .emit(15 * MS + 2, call_with(8, &resume(3)));
    let own = holds(
        Scenario {
            machines: vec![
                oracle,
                Spec::busy()
                    .every(53 * US + 1, 401 * US, end, mac(2), 10)
                    .emit(MS + 7, hit())
                    .emit(5 * MS + 3, hit())
                    .emit(
                        9 * MS + 1,
                        Response::Break {
                            name: "follower".into(),
                        },
                    )
                    .emit(12 * MS + 1, hit()),
                // Its frames reach m1 while it is suspended, at their arrival.
                Spec::idle()
                    .echo(mac(1))
                    .every(83 * US + 1, 457 * US, end, mac(1), 20),
            ],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: true,
        },
        300,
    );
    assert_eq!(replies(&own).len(), 8, "every call answered");
    assert_eq!(events_with(&own, "BreakHit"), 3);
    assert_eq!(events_with(&own, "reason: Resumed"), 3);
}

/// C1: what follows from a machine's exit, the settlement of its pending
/// commands and the replies to their callers, is due one floor after the
/// exit, whatever the commit that saw it. A later crash and reboot of the
/// exited machine change nothing earlier.
#[test]
fn machine_exits_do_not_depend_on_stops() {
    let end = 20 * MS;
    let m1 = || Target::Name("m1".into());
    let oracle = Spec::busy()
        .every(31 * US, 523 * US, end, BROADCAST, 0)
        .emit(
            MS + 5,
            call_with(1, r#""op":"stop","target":{"machine":"m1"}"#),
        )
        .emit(
            9 * MS + 7,
            call_with(2, r#""op":"pause_process","target":{"machine":"m2"}"#),
        );
    let own = holds(
        Scenario {
            machines: vec![
                oracle,
                // Ready, but never acknowledges: its commands are pending
                // when it exits.
                Spec::idle()
                    .echo(mac(2))
                    .every(61 * US + 3, 487 * US, end, mac(2), 10)
                    .emit(10 * US, Response::Ready)
                    .exit(3 * MS + 5),
                Spec::busy()
                    .echo(mac(1))
                    .every(97 * US + 1, 331 * US, end, mac(1), 20)
                    .emit(10 * US, Response::Ready)
                    .exit(10 * MS + 3),
            ],
            network: jittery(),
            inputs: vec![
                // Queued behind the oracle's pending stop.
                (
                    2 * MS,
                    "restart",
                    Action::Restart {
                        target: m1(),
                        deadline: Duration::from_secs(1),
                    },
                ),
                (5 * MS, "crash", Action::Crash(m1())),
                (6 * MS + 1, "reboot", Action::Reboot(m1())),
            ],
            end,
            breaks: false,
        },
        300,
    );
    assert_eq!(replies(&own).len(), 2, "every call answered");
    assert_eq!(events_with(&own, "status: Unavailable"), 3);
    assert!(
        fates(&own, "ReceiverDropped") > 0,
        "frames for exited machines"
    );
}

/// C4: the run ends at the commit of the round in which the oracle exited.
/// It will end one floor after the exit.
#[test]
#[ignore = "C4: the run ends at the commit that saw the oracle's exit"]
fn the_end_of_the_run_does_not_depend_on_stops() {
    let end = 10 * MS;
    holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .every(13 * US, 397 * US, end, BROADCAST, 0)
                    .exit(4 * MS + 123),
                Spec::busy().echo(mac(0)),
            ],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        1,
    );
}

/// C2: outputs of two machines stamped in the same nanosecond are ordered by
/// the commit that carried them. A round that ends at such a nanosecond takes
/// the busy machine's output, while the blocked one emits in the next round.
/// They will be ordered by machine.
#[test]
#[ignore = "C2: same-nanosecond outputs of different machines are ordered by commit"]
fn same_nanosecond_outputs_do_not_depend_on_stops() {
    let end = 200 * US;
    let ties = |spec: Spec| {
        (0..end / 7).fold(spec, |spec, n| {
            spec.emit(
                7 * n + 3,
                Response::ApplicationOutput {
                    stream: Stream::Stdout,
                    bytes: n.to_le_bytes().to_vec(),
                },
            )
        })
    };
    holds(
        Scenario {
            machines: vec![ties(Spec::idle()), ties(Spec::busy())],
            network: jittery(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        1,
    );
}

/// Not predicted by the design. A blocked machine wakes at the first
/// simulation nanosecond whose machine time reaches its deadline. Under a fast
/// clock that nanosecond's machine time can be past the deadline, so the
/// machine adopts a later time than it would have reached had a run covered
/// the deadline. Whether a run covered it depends on where the previous round
/// ended. C3 (a synthetic stop adopts no time) needs a wake to start at the
/// deadline itself, which lies inside that simulation nanosecond.
#[test]
#[ignore = "C3: a timer under a fast clock wakes past its deadline only after a stop"]
fn a_timer_under_a_fast_clock_does_not_depend_on_stops() {
    let end = 20 * MS;
    holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .echo(mac(1))
                    .every(19 * US, 331 * US, end, BROADCAST, 0),
                Spec::idle()
                    .echo(mac(0))
                    .every(83 * US + 1, 457 * US, end, mac(0), 10),
            ],
            network: jittery(),
            inputs: vec![(
                3 * MS,
                "drift",
                Action::Drift {
                    target: Target::Name("m1".into()),
                    rate: ClockRate::new(100_000_000).unwrap(),
                },
            )],
            end,
            breaks: false,
        },
        1,
    );
}
