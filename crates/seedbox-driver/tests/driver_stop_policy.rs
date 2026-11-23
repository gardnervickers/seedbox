//! What does not depend on where rounds end, on scripted machines.
//!
//! Each scenario runs once with the Driver's own round boundaries, once
//! under each of several seeded [`StopPolicy`]s, which end rounds early at
//! host-drawn times, and once with each of two other quanta, which move the
//! Driver's own boundaries. It also runs with machines whose runs stay in
//! flight on the Driver's thread, as a PV machine's do
//! ([`Machine::split_run`]), under the Driver's own stops and under each
//! policy. The runs must agree on:
//!
//! - the event stream, with each event's `position` removed (it counts commits);
//! - Driver policy evidence, as written, at a few times the host asks for a
//!   barrier at in every run and at the end;
//! - frame facts as a multiset, without positions (their interleaving follows
//!   commits);
//! - each machine's own record of what it observed: every input with the
//!   machine time it was offered, every draw from its RNG stream and every
//!   output it emitted. The bounds of its runs are not part of it.
//!
//! One scenario instead adds a single host barrier where a slow clock maps
//! two simulation nanoseconds to one machine nanosecond, which a random
//! policy would rarely hit.
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
    machine::{ExitReason, Machine, RunContext, SplitRun, StopReason, Timestamp},
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
/// The quantum of every run but those in [`QUANTA`].
const QUANTUM: Duration = Duration::from_millis(1);
/// Other quanta every scenario runs with, under the Driver's own stops. The
/// latency floor stays the network's, whatever the quantum.
const QUANTA: [Duration; 2] = [Duration::from_micros(250), Duration::from_millis(4)];
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
    /// Notices its exit only in a run that could go past it.
    late_exit: bool,
    exited: bool,
    declared: Vec<String>,
    held: bool,
    reached: u64,
    seen: Seen,
    /// Report [`Machine::split_run`]: the run executes in `start` and stays
    /// in flight until `finish`.
    split: bool,
    flight: Option<Result<StopReason, std::io::Error>>,
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
        let late = self.late_exit;
        let exit = self
            .exit
            .filter(|&exit| exit < end || !late && exit == end)
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

    fn split_run(&mut self) -> Option<&mut dyn SplitRun<(), Self::Error>> {
        self.split.then_some(self)
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl SplitRun<(), std::io::Error> for Script {
    fn start(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<Option<StopReason>, std::io::Error> {
        assert!(self.flight.is_none(), "one run in flight");
        self.flight = Some(self.run(context, upto));
        Ok(None)
    }

    fn ready(&mut self) -> bool {
        // Completion order is covered by the Driver's network tests; a run
        // that is not ready at a poll costs this suite an idle wait.
        true
    }

    fn finish(&mut self, _: &mut RunContext<'_, ()>) -> Result<StopReason, std::io::Error> {
        self.flight.take().expect("a run in flight")
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
    late_exit: bool,
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
    /// Exit at `t`, noticed only by a run that could go past `t`: a run
    /// that ends exactly at `t` reaches its bound, and the next one exits at
    /// its start. Its machine-time behaviour is the same either way.
    fn late_exit(mut self, t: u64) -> Self {
        self.exit = Some(t);
        self.late_exit = true;
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
    /// Policy evidence at each time the host looked, the last at the end.
    evidence: Vec<Vec<u8>>,
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
    run_with(scenario, policy, QUANTUM, &[])
}

/// [`run`] with `quantum`, the host first asking for a barrier at each of
/// `barriers`.
fn run_with(
    scenario: &Scenario,
    policy: Option<u64>,
    quantum: Duration,
    barriers: &[u64],
) -> (Results, usize) {
    run_as(scenario, policy, quantum, barriers, false)
}

/// [`run_with`], every machine's runs staying in flight on the Driver's
/// thread if `split`.
fn run_as(
    scenario: &Scenario,
    policy: Option<u64>,
    quantum: Duration,
    barriers: &[u64],
    split: bool,
) -> (Results, usize) {
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
            late_exit: spec.late_exit,
            exited: false,
            declared: Vec::new(),
            held: false,
            reached: 0,
            seen: seen.clone(),
            split,
            flight: None,
        })
        .collect();
    let definitions: Vec<_> = (0..machines.len()).map(networked).collect();
    let mut driver = Driver::new_seeded(
        at(0),
        quantum,
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
    for &barrier in barriers {
        while matches!(
            driver.step(at(barrier)).unwrap(),
            StepOutcome::BarrierReached { at: reached } if reached < at(barrier)
        ) {}
    }
    let evidence = |driver: &mut Driver<Script>| {
        driver
            .with_barrier(|barrier| {
                let mut evidence = Vec::new();
                barrier.write_policy_evidence(&mut evidence).unwrap();
                Ok(evidence)
            })
            .unwrap()
    };
    // The host looks at the world at a few times every run reaches, and at
    // the end.
    let end = scenario.end;
    let mut looks = Vec::new();
    let mut barriers = 0;
    let mut outcome = None;
    for until in [end / 3 + 7, end / 2 + 1, 2 * end / 3 + 13, end] {
        outcome = Some(loop {
            let outcome = driver.step(at(until)).unwrap();
            barriers += 1;
            match outcome {
                StepOutcome::BarrierReached { at } if at.as_nanos() < until => {}
                other => break other,
            }
        });
        looks.push(evidence(&mut driver));
    }
    let outcome = outcome.unwrap();
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
    let results = Results {
        outcome: format!("{outcome:?}"),
        events,
        evidence: looks,
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
            "{what} differs at {i}:\n  own stops: {}\n  other:     {}",
            a[i], b[i]
        )),
        None if a.len() != b.len() => Some(format!(
            "{what} lengths differ: own stops {}, other {}",
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
            "outcome: own stops {}, other {}",
            own.outcome, other.outcome
        ));
    }
    found.extend(first_difference("event stream", &own.events, &other.events));
    for (index, (a, b)) in own.evidence.iter().zip(&other.evidence).enumerate() {
        if a != b {
            found.push(format!("policy evidence differs at look {index}"));
        }
    }
    found.extend(first_difference("frame facts", &own.frames, &other.frames));
    for (index, (a, b)) in own.seen.iter().zip(&other.seen).enumerate() {
        found.extend(first_difference(&format!("machine {index}"), a, b));
    }
    found
}

/// Run `scenario` with the Driver's own stops, under every policy seed and
/// with every other quantum, and with split machines under the Driver's own
/// stops and every policy seed. Returns the results under the Driver's own
/// stops, and what differed from them in each other run.
fn compare(scenario: &Scenario) -> (Results, Vec<(String, Vec<String>)>) {
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
            found.push((format!("seed {seed}"), differences));
        }
    }
    for quantum in QUANTA {
        let (other, _) = run_with(scenario, None, quantum, &[]);
        let differences = differences(&own, &other);
        if !differences.is_empty() {
            found.push((format!("quantum {quantum:?}"), differences));
        }
    }
    for policy in std::iter::once(None).chain(SEEDS.map(Some)) {
        let (other, _) = run_as(scenario, policy, QUANTUM, &[], true);
        let differences = differences(&own, &other);
        if !differences.is_empty() {
            found.push((format!("split, policy {policy:?}"), differences));
        }
    }
    (own, found)
}

/// Assert that no stop policy changes the scenario's results, and that its
/// machines observed at least `at_least` things. Returns the results.
fn holds(scenario: Scenario, at_least: usize) -> Results {
    let (own, found) = compare(&scenario);
    assert!(
        found.is_empty(),
        "stops or quantum changed results: {found:#?}"
    );
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

/// C4: the run ends one floor after the oracle's exit, whatever the commit
/// that saw it. Every machine runs to the end, and output up to it is published.
#[test]
fn the_end_of_the_run_does_not_depend_on_stops() {
    let end = 10 * MS;
    let own = holds(
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
    assert_eq!(
        own.outcome,
        "Finished { at: Timestamp(4100123), reason: Shutdown }"
    );
}

/// C4: after the oracle's outcome, what is due before the end still happens:
/// a call it made just before is answered, a peer's flush is answered, and a
/// host input takes effect. Nothing due at the end or later does, and a
/// machine suspended at a breakpoint stays suspended until the end.
#[test]
fn what_is_due_before_the_end_happens_whatever_the_stops() {
    let end = 10 * MS;
    let x = 4 * MS + 123;
    let f = x + 100 * US;
    let declare = r#""op":"declare_breaks","names":["leader"]"#;
    let own = holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .every(13 * US, 397 * US, end, BROADCAST, 0)
                    .emit(100 * US + 3, call_with(1, declare))
                    .emit(x - 50 * US, call(2, "now"))
                    .emit(x, Response::ApplicationOutcome { code: 3 })
                    // Admitted at the end: never answered.
                    .emit(f - 100 * US, call(3, "now")),
                Spec::busy()
                    .echo(mac(0))
                    .emit(x - 99 * US, property(1, 1, r#"{"flush":{}}"#))
                    .emit(
                        x + 1,
                        Response::Break {
                            name: "leader".into(),
                        },
                    ),
                Spec::idle().every(97 * US + 1, 331 * US, end, mac(1), 20),
            ],
            network: jittery(),
            inputs: vec![
                (f - 1, "pause", Action::Pause(Target::Name("m2".into()))),
                (f, "late", Action::Pause(Target::Name("m1".into()))),
            ],
            end,
            breaks: true,
        },
        1,
    );
    assert_eq!(
        own.outcome,
        format!("Finished {{ at: Timestamp({f}), reason: ProgramExit {{ code: 3 }} }}")
    );
    assert_eq!(
        replies(&own).len(),
        2,
        "the call before the outcome answered"
    );
    assert!(own.seen[1]
        .iter()
        .any(|seen| seen.contains("PropertyReply")));
    assert_eq!(
        events_with(&own, "status: Applied"),
        1,
        "only the first pause"
    );
    assert_eq!(events_with(&own, "BreakHit"), 1);
    assert_eq!(events_with(&own, "reason: RunEnded"), 1);
    let last = own.events.iter().filter(|event| event.contains("Output"));
    assert!(last
        .map(|event| event.split(' ').nth(2).unwrap().parse::<u64>().unwrap())
        .all(|at| at <= f));
}

/// C2: outputs of two machines stamped in the same nanosecond are ordered by
/// machine, whatever the commits. A round that ends at such a nanosecond
/// holds the busy machine's output there until the blocked one, which emits
/// at it in the next round, has passed it.
#[test]
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

/// An Ethernet/IPv4/TCP SYN from machine `from`'s address to `to`'s.
fn syn(from: usize, to: usize, port: u16) -> Response {
    let mut b = vec![0; 60];
    b[..6].copy_from_slice(&mac(to));
    b[6..12].copy_from_slice(&mac(from));
    b[12..14].copy_from_slice(&[8, 0]);
    b[14] = 0x45;
    b[16..18].copy_from_slice(&40u16.to_be_bytes());
    b[22] = 64;
    b[23] = 6;
    b[26..30].copy_from_slice(&[10, 0, 0, from as u8 + 1]);
    b[30..34].copy_from_slice(&[10, 0, 0, to as u8 + 1]);
    b[34..36].copy_from_slice(&port.to_be_bytes());
    b[36..38].copy_from_slice(&80u16.to_be_bytes());
    b[46] = 0x50;
    b[47] = 0x02;
    Response::TransmitFrame { bytes: b }
}

/// C2: two machines open connections to a third in the same nanoseconds.
/// Their incarnation numbers follow routing order, which is (machine,
/// emission) order whatever the commits: the busy machine's SYN at a round's
/// end is held there until the blocked one, whose SYN comes in the next
/// round, has passed that nanosecond.
#[test]
fn connection_incarnations_do_not_depend_on_stops() {
    let end = 40 * US;
    let opens = |spec: Spec, from: usize, first_port: u16| {
        (0..end / 11).fold(spec, |spec, n| {
            spec.emit(11 * n + 5, syn(from, 0, first_port + n as u16))
        })
    };
    let connections = |request: u32, from: &str| {
        call_with(
            request,
            &format!(r#""op":"connections","from":{{"machine":"{from}"}},"to":{{"machine":"m0"}}"#),
        )
    };
    let own = holds(
        Scenario {
            machines: vec![
                Spec::busy()
                    .emit(end - 2 * US, connections(1, "m1"))
                    .emit(end - 2 * US, connections(2, "m2")),
                opens(Spec::idle(), 1, 10_000),
                opens(Spec::busy(), 2, 30_000),
            ],
            network: NetworkConfig::new(Duration::from_nanos(700), Duration::ZERO, 0)
                .unwrap()
                .into(),
            inputs: Vec::new(),
            end,
            breaks: false,
        },
        1,
    );
    let listed = replies(&own);
    assert_eq!(listed.len(), 2, "both listings answered");
    for reply in listed {
        let bytes = reply.split("payload: [").nth(1).unwrap();
        let bytes: Vec<u8> = bytes
            .trim_end_matches(&[']', ' ', '}'][..])
            .split(", ")
            .map(|byte| byte.parse().unwrap())
            .collect();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains(r#""incarnation":"#), "{text}");
    }
    assert!(fates(&own, "Scheduled") >= 2 * (end as usize / 11 - 1));
}

/// C2: a machine held at a breakpoint takes the frames that arrive for it in
/// its lane, without a budget, whether or not a commit has carried its hit
/// yet. Were such a frame a barrier only once the hit is committed, an
/// arrival soon after a hit would cut every machine there under some stops
/// and not under others, and the outputs of other machines stamped in the
/// arrival's nanosecond would be ordered by that cut: the busy machine's,
/// emitted before it, ahead of the lower-numbered idle one's, emitted after.
#[test]
fn frames_for_a_machine_held_at_a_breakpoint_do_not_depend_on_stops() {
    const LATENCY: u64 = 700;
    let pairs = 24_u64;
    let end = pairs * 10 * US + 5 * US;
    let said = |n: u64| Response::ApplicationOutput {
        stream: Stream::Stdout,
        bytes: n.to_le_bytes().to_vec(),
    };
    let mut oracle = Spec::busy().emit(
        100,
        call_with(1, r#""op":"declare_breaks","names":["leader"]"#),
    );
    let (mut idle, mut busy, mut held) = (Spec::idle(), Spec::busy(), Spec::busy());
    for n in 0..pairs {
        // A hit, then a frame that reaches the held machine 40 to 707 ns
        // later, inside or just past the round the hit was written in. Two
        // other machines write in the arrival's nanosecond.
        let hit = 2 * US + n * 10 * US + 37 * n;
        let arrival = hit + 40 + 29 * n;
        held = held.emit(
            hit,
            Response::Break {
                name: "leader".into(),
            },
        );
        busy = busy
            .emit(
                arrival - LATENCY,
                Response::TransmitFrame {
                    bytes: frame(mac(3), ECHO_LIMIT, &n.to_le_bytes()),
                },
            )
            .emit(arrival, said(n));
        idle = idle.emit(arrival, said(n));
        let resume = format!(r#""op":"resume_break","break_id":{}"#, n + 1);
        oracle = oracle.emit(hit + 4 * US, call_with(n as u32 + 2, &resume));
    }
    let own = holds(
        Scenario {
            machines: vec![oracle, idle, busy, held],
            network: NetworkConfig::new(Duration::from_nanos(LATENCY), Duration::ZERO, 0)
                .unwrap()
                .into(),
            inputs: Vec::new(),
            end,
            breaks: true,
        },
        1,
    );
    assert_eq!(events_with(&own, "BreakHit"), pairs as usize);
    assert_eq!(events_with(&own, "reason: Resumed"), pairs as usize);
    // Every frame reached the machine while it was held.
    let received = own.seen[3]
        .iter()
        .filter(|line| line.contains("ReceiveFrame"))
        .count();
    assert_eq!(received, pairs as usize);
}

/// A slow clock maps two simulation nanoseconds to one machine nanosecond. A
/// machine that notices its exit only by trying to run past it exits at the
/// start of its next run when a barrier falls on the second of the two; its
/// exit still maps to the first, as when no barrier falls there.
#[test]
fn a_slow_clock_exit_does_not_depend_on_a_barrier_in_its_nanosecond() {
    let end = 10 * MS;
    let scenario = Scenario {
        machines: vec![
            Spec::busy()
                .every(13 * US, 397 * US, end, BROADCAST, 0)
                .late_exit(1_800 * US),
            Spec::busy().echo(mac(0)),
        ],
        network: jittery(),
        inputs: vec![(
            0,
            "slow",
            Action::Drift {
                target: Target::Name("m0".into()),
                rate: ClockRate::new(-100_000_000).unwrap(),
            },
        )],
        end,
        breaks: false,
    };
    // At nine tenths of simulation time, machine time 1.8 ms is reached at
    // 2 ms and still read at 2 ms + 1 ns.
    let (own, _) = run(&scenario, None);
    let (cut, _) = run_with(&scenario, None, QUANTUM, &[2 * MS + 1]);
    assert_eq!(differences(&own, &cut), Vec::<String>::new());
    assert_eq!(
        own.outcome,
        format!(
            "Finished {{ at: Timestamp({}), reason: Shutdown }}",
            2 * MS + 100 * US
        )
    );
}

/// A blocked machine wakes at the first simulation nanosecond whose machine
/// time reaches its deadline. Under a fast clock that nanosecond's machine
/// time can be past the deadline; the wake starts at the deadline itself,
/// inside that nanosecond, as a run that covered the deadline would take it
/// (C3), whatever the previous round's end.
#[test]
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
