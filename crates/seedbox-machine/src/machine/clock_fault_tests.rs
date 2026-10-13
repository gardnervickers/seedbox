//! Native clock faults through the Driver on TCG and PV with one shared guest
//! fixture (`tests/fixtures/clock-faults.c`). The machine is wrapped to record
//! its own-time-base runs, so timer conversion is checked exactly against an
//! independent reference mapping built from the published rate events.
use crate::{
    clock::ClockRate,
    driver::{
        Action, Barrier, Driver, Error, Event, EventKind, FaultAction, FaultOrigin, FaultRecord,
        HealTarget, InputKey, MachineId, PlaybackRate, SettlementStatus, StepOutcome, Target,
        Transition,
    },
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Response, Stream},
    rng::RngStream,
};
use std::{
    fmt::Debug,
    fs,
    num::NonZeroI64,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const QUANTUM: u64 = 5_000_000;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(key: &str) -> PathBuf {
    std::env::var_os(key).expect(key).into()
}
struct FixtureRng(u8);
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Run {
    now: u64,
    upto: u64,
    inputs: bool,
    result: StopReason,
}
/// Records runs in the machine's own time base; `snapshot` restores the
/// machine in place from its own capture, keeping every Driver-owned input.
struct Probe<M, F> {
    inner: M,
    restore: F,
    runs: Arc<Mutex<Vec<Run>>>,
}
impl<M, F> Machine for Probe<M, F>
where
    M: Machine<Context = ()>,
    F: Fn(&M::Snapshot) -> Result<M, M::Error>,
{
    type Context = ();
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = context.clock.now();
        let inputs = context.inbox.front().is_some();
        let result = self.inner.run(context, upto)?;
        self.runs.lock().unwrap().push(Run {
            now,
            upto: upto.as_nanos(),
            inputs,
            result,
        });
        Ok(result)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        let saved = self.inner.snapshot()?;
        self.inner = (self.restore)(&saved)?;
        Ok(saved)
    }
}

fn definitions(
    backend: &str,
    oracle_role: &str,
) -> (
    crate::definition::MachineDefinition,
    crate::definition::MachineDefinition,
) {
    let application = fs::read(input(&format!("SEEDBOX_{backend}_CLOCK_APPLICATION"))).unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(application.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &application[..])
        .unwrap();
    let oracle = crate::definition::MachineDefinition {
        name: "oracle".into(),
        resources: crate::definition::Resources {
            memory_bytes: if backend == "PV" {
                128 << 20
            } else {
                256 << 20
            },
        },
        application: crate::definition::PreparedApplication {
            filesystem: crate::definition::FilesystemArchive::new(tar.into_inner().unwrap())
                .unwrap(),
            launch: crate::definition::Launch {
                argv: vec!["/application".into(), oracle_role.into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
    };
    let mut clock = oracle.clone();
    clock.name = "clock".into();
    clock.application.launch.argv = vec!["/application".into(), "clock".into()];
    (oracle, clock)
}

/// Published events, the clock machine's identity, its recorded runs and the
/// faults held before their effect.
type Outcome = (Vec<Event>, MachineId, Vec<Run>, Vec<FaultRecord>);
/// Host actions of a scenario, applied at the start, after every barrier and
/// on every held fault.
#[derive(Clone, Copy)]
enum Plan {
    /// Faults come from the guest controller (oracle role `controller`).
    Guest,
    /// Faults come from the public unified schedule (oracle role `idle`).
    Host,
    /// One host step below CLOCK_MONOTONIC (oracle role `idle`).
    Refusal,
}
impl Plan {
    fn oracle(self) -> &'static str {
        match self {
            Self::Guest => "controller",
            Self::Host | Self::Refusal => "idle",
        }
    }
    fn epoch(self) -> u32 {
        match self {
            Self::Refusal => REFUSAL_EPOCH_SECONDS,
            Self::Guest | Self::Host => crate::machine::TEST_EPOCH_SECONDS,
        }
    }
    fn apply<N>(
        self,
        b: &mut Barrier<'_, N>,
        now: Timestamp,
        held: Option<&FaultRecord>,
        live: &mut bool,
    ) where
        N: Machine + Send + 'static,
        N::Context: 'static,
        N::Error: Debug + Send + 'static,
    {
        match self {
            Self::Guest => {}
            Self::Host => host_plan(b, now, held, live),
            Self::Refusal => refusal_plan(b, now),
        }
    }
}

/// One run of the shared fixture: an oracle with `oracle_role` and the
/// recorded `clock` machine. `migrate` restores the clock machine in place
/// mid-drift, inside the same Driver.
fn scenario<M, F>(
    backend: &str,
    workers: usize,
    migrate: bool,
    plan: Plan,
    mut construct: impl FnMut(&crate::definition::MachineDefinition) -> M + Send + 'static,
    restore: F,
) -> Outcome
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
    F: Fn(&M::Snapshot) -> Result<M, M::Error> + Clone + Send + 'static,
{
    let (oracle, clock) = definitions(backend, plan.oracle());
    let mut live = false;
    let runs = Arc::new(Mutex::new(Vec::new()));
    let recorded = runs.clone();
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(3),
        || Ok(()),
        oracle,
        vec![clock],
        Vec::new(),
        move |definition| {
            Ok(Probe {
                inner: construct(definition),
                restore: restore.clone(),
                runs: if definition.name == "clock" {
                    recorded.clone()
                } else {
                    Arc::new(Mutex::new(Vec::new()))
                },
            })
        },
    )
    .unwrap();
    let clock = driver
        .with_barrier(|b| Ok(b.machines().nth(1).unwrap().0))
        .unwrap();
    let started = Instant::now();
    let mut events = driver.drain_events();
    let mut held = Vec::new();
    let mut migrated = !migrate;
    let mut now = at(0);
    driver
        .with_barrier(|b| {
            plan.apply(b, now, None, &mut live);
            Ok(())
        })
        .unwrap();
    loop {
        let upto = at(now.as_nanos() + QUANTUM);
        let result = driver.step(upto).unwrap();
        events.extend(driver.drain_events());
        match result {
            StepOutcome::Finished { reason, .. } => {
                assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
                break;
            }
            StepOutcome::BarrierReached { at } => {
                now = at;
                driver
                    .with_barrier(|b| {
                        plan.apply(b, now, None, &mut live);
                        Ok(())
                    })
                    .unwrap();
            }
            StepOutcome::BeforeFault { at, fault } => {
                now = at;
                driver
                    .with_barrier(|b| {
                        plan.apply(b, now, Some(&fault), &mut live);
                        Ok(())
                    })
                    .unwrap();
                held.push(fault);
            }
            other => panic!("{backend}: unexpected {other:?}"),
        };
        // Mid-drift, after the first step and before the crash: restore in place.
        if !migrated && upto >= at(3_000_000_000) {
            driver
                .with_barrier(|b| b.snapshot(clock).map(drop))
                .unwrap();
            migrated = true;
        }
        assert!(
            started.elapsed() < Duration::from_secs(900),
            "{backend} scenario too slow"
        );
    }
    let runs = runs.lock().unwrap().clone();
    (events, clock, runs, held)
}

/// Independent exact reference: G in 1e-9 ns from piecewise rates.
struct Reference(Vec<(u64, i64)>);
impl Reference {
    fn exact(&self, t: u64) -> i128 {
        let mut value = 0i128;
        let mut from = 0u64;
        let mut ppb = 0i64;
        for &(change, rate) in &self.0 {
            if change > t {
                break;
            }
            value += i128::from(change - from) * (1_000_000_000 + i128::from(ppb));
            from = change;
            ppb = rate;
        }
        value + i128::from(t - from) * (1_000_000_000 + i128::from(ppb))
    }
    fn floor(&self, t: u64) -> u64 {
        (self.exact(t) / 1_000_000_000) as u64
    }
    fn inverse(&self, machine: u64) -> u64 {
        let target = i128::from(machine) * 1_000_000_000;
        let (mut low, mut high) = (0u64, machine * 2 + 2);
        while low < high {
            let middle = low + (high - low) / 2;
            if self.exact(middle) >= target {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }
}

#[derive(Debug)]
struct Line {
    at: u64,
    boot: u64,
    tag: String,
    rt: i128,
    mono: i128,
    raw: i128,
    cnt: i128,
}
/// Clock-machine stdout lines; `rt` is relative to the realtime epoch.
fn lines(events: &[Event], clock: MachineId, epoch_seconds: u32) -> Vec<Line> {
    let epoch = i128::from(epoch_seconds) * 1_000_000_000;
    let mut lines = Vec::new();
    let mut partial = Vec::new();
    for event in events {
        let EventKind::Output(output) = &event.kind else {
            continue;
        };
        let Response::ApplicationOutput {
            stream: Stream::Stdout,
            bytes,
        } = &output.payload
        else {
            continue;
        };
        if output.machine != clock {
            continue;
        }
        for &byte in bytes {
            if byte != b'\n' {
                partial.push(byte);
                continue;
            }
            let text = String::from_utf8(std::mem::take(&mut partial)).unwrap();
            let mut fields = text.split(' ');
            let tag = fields.next().unwrap().to_string();
            let mut value = |name: &str| -> i128 {
                fields
                    .next()
                    .and_then(|f| f.strip_prefix(name))
                    .and_then(|f| f.strip_prefix('='))
                    .unwrap_or_else(|| panic!("{text}"))
                    .parse()
                    .unwrap()
            };
            lines.push(Line {
                at: output.at.as_nanos(),
                boot: output.boot,
                tag,
                rt: value("rt") - epoch,
                mono: value("mono"),
                raw: value("raw"),
                cnt: value("cnt"),
            });
        }
    }
    lines
}

/// Guest sampling and transport overhead, in machine nanoseconds: the counter
/// is read before formatting, the pipe write, PID 1 relay and transport. It is
/// not conversion error (see the exact timer checks). Measured 15-58 us on TCG
/// and 39-51 us on x86 PV; 1 ms leaves headroom without hiding a rate error.
const LATENCY: i128 = 1_000_000;

/// Rate changes of the clock machine, as published.
fn rates(events: &[Event], clock: MachineId) -> Reference {
    Reference(
        events
            .iter()
            .filter_map(|event| match event.kind {
                EventKind::Transition {
                    machine,
                    change: Transition::ClockRate(rate),
                    ..
                } if machine == clock => Some((event.at.as_nanos(), i64::from(rate.ppb()))),
                _ => None,
            })
            .collect(),
    )
}

/// Checks shared by every scenario: each reading follows the machine time
/// base, and every timer wake is never early, earliest, and re-converted when
/// it straddles a rate change.
fn check_time_base(backend: &str, lines: &[Line], runs: &[Run], reference: &Reference) {
    // The counter read just before the write is at most G(output time) and
    // close to it.
    for line in lines {
        let mapped = i128::from(reference.floor(line.at));
        assert!(
            line.cnt <= mapped && mapped - line.cnt < LATENCY,
            "{backend}: {line:?} G={mapped}"
        );
        assert!(line.raw <= line.cnt && line.mono <= line.cnt, "{line:?}");
    }
    // Exact conversion. The Driver runs a blocked machine without inputs only
    // when its converted deadline is due: rate changes, snapshots, holds and
    // other machines' work never run it, and these scenarios have no
    // pause/resume. Runs with inputs (requests, replies, lifecycle) are the only
    // legitimate earlier runs. So every input-free run after a Blocked stop is
    // that timer's wake: it must never be early, and it must happen at the
    // earliest simulation time whose mapped time reaches the deadline.
    let mut exact = 0;
    let mut straddled = 0;
    let mut reconverted = 0;
    for pair in runs.windows(2) {
        let (
            StopReason::Blocked {
                reached,
                deadline: Some(due),
            },
            next,
        ) = (pair[0].result, &pair[1])
        else {
            continue;
        };
        if next.inputs {
            continue;
        }
        let due = due.as_nanos();
        assert!(
            next.now >= due,
            "{backend}: early wake {} < {due}",
            next.now
        );
        let wake = reference.inverse(due);
        assert_eq!(next.now, reference.floor(wake), "{backend}: due {due}");
        exact += 1;
        // A pending timer straddling a rate change was re-converted: its wake
        // differs from the one its deadline had under the earlier rates.
        for (index, &(change, _)) in reference.0.iter().enumerate() {
            let boundary = reference.floor(change);
            if reached.as_nanos() < boundary && boundary < next.now {
                straddled += 1;
                // Rates here are never slow, so the mapping is injective and
                // the observed clock identifies the wake: a stale (unconverted)
                // wake would be observed differently.
                let earlier = Reference(reference.0[..index].to_vec());
                let stale = earlier.inverse(due);
                if stale != wake {
                    assert_ne!(reference.floor(stale), next.now, "{backend}: stale wake");
                    reconverted += 1;
                }
            }
        }
    }
    assert!(exact > 10, "{backend}: {exact} timer wakeups checked");
    if reference.0.len() > 1 {
        assert!(
            straddled >= 1 && reconverted >= 1,
            "{backend}: {straddled} straddling timers, {reconverted} re-converted"
        );
    }
}
fn find<'l>(lines: &'l [Line], boot: u64, tag: &str) -> Vec<&'l Line> {
    lines
        .iter()
        .filter(|l| l.boot == boot && l.tag == tag)
        .collect()
}
/// Every boot starts realtime at the factory epoch plus the counter.
fn check_boots(backend: &str, lines: &[Line], boots: u64) {
    for boot in 0..boots {
        let line = find(lines, boot, "BOOT")[0];
        assert!(
            line.cnt - line.rt >= 0 && line.cnt - line.rt < LATENCY,
            "{backend}: {line:?}"
        );
    }
}
/// A guest 1 s sleep under +10% drift: monotonic, raw and the counter agree,
/// and the sleep spans 1/1.1 of the simulation time.
fn check_sleep(backend: &str, lines: &[Line], reference: &Reference) {
    let slept = find(lines, 0, "SLEPT");
    let (first, second) = (slept[0], slept[1]);
    assert!(
        first.at > reference.0[0].0,
        "{backend}: drift precedes the sleeps"
    );
    let delta = |f: fn(&Line) -> i128| f(second) - f(first);
    assert!(delta(|l| l.mono) >= 1_000_000_000);
    assert!((delta(|l| l.cnt) - delta(|l| l.mono)).abs() < 1_000_000);
    assert!((delta(|l| l.raw) - delta(|l| l.cnt)).abs() < 1_000_000);
    let simulated = i128::from(second.at - first.at);
    assert!(
        (simulated * 11 / 10 - delta(|l| l.cnt)).abs() < LATENCY,
        "{backend}: rate"
    );
}
/// Heal keeps the offset accrued until then: G(t) = t + accrued afterwards.
fn check_heal(backend: &str, lines: &[Line], reference: &Reference, healed: u64) {
    let accrued = i128::from(reference.floor(healed)) - i128::from(healed);
    assert!(accrued > 400_000_000, "{backend}");
    let last = lines.last().unwrap();
    assert!(last.at > healed + 500_000_000);
    let ahead = last.cnt - i128::from(last.at);
    assert!(ahead <= accrued && ahead > accrued - LATENCY, "{last:?}");
}

/// Guest-controller scenario: drift, +2 s step, crash/reboot, heals.
fn check_guest(backend: &str, (events, clock, runs, held): &Outcome) {
    assert!(held.is_empty());
    let reference = rates(events, *clock);
    assert_eq!(reference.0.len(), 2, "{backend}: drift then heal");
    let healed = reference.0[1].0;
    assert_eq!((reference.0[0].1, reference.0[1].1), (100_000_000, 0));
    assert!(
        healed >= 5_000_000_000,
        "{backend}: heal after the reboot check"
    );
    for settlement in crate::driver::fixture::settlements(events) {
        assert_eq!(settlement.1, SettlementStatus::Applied, "{settlement:?}");
    }
    let lost = events.iter().any(|event| {
        matches!(&event.kind, EventKind::FaultSettled { record, detail, .. }
            if record.handle.as_ref().map(ToString::to_string).as_deref() == Some("p2")
                && detail == "realtime step ended with its machine incarnation")
    });
    assert!(lost, "{backend}: healing the step after reboot");
    let lines = lines(events, *clock, crate::machine::TEST_EPOCH_SECONDS);
    check_time_base(backend, &lines, runs, &reference);
    check_boots(backend, &lines, 2);
    check_sleep(backend, &lines, &reference);
    // Drift persists across reboot: the new boot already runs ahead.
    let reboot = find(&lines, 1, "BOOT")[0];
    assert!(reboot.at > 4_000_000_000 && reboot.cnt - i128::from(reboot.at) > 350_000_000);
    // The step moved realtime only, cancelled the cancel-on-set timer and fired
    // the absolute realtime timer well before its unstepped monotonic time.
    let boot = find(&lines, 0, "BOOT")[0];
    let canceled = find(&lines, 0, "CANCELED")[0];
    let stepped = canceled.rt - canceled.cnt;
    assert!(
        stepped > 2_000_000_000 - LATENCY && stepped <= 2_000_000_000,
        "{canceled:?}"
    );
    assert!(canceled.at >= 2_500_000_000);
    let fired = find(&lines, 0, "FIRED")[0];
    assert!(fired.rt >= boot.rt + 5_000_000_000);
    assert!(
        fired.mono - boot.mono < 3_500_000_000,
        "{backend}: {fired:?}"
    );
    assert!(find(&lines, 1, "CANCELED").is_empty() && find(&lines, 1, "FIRED").is_empty());
    check_heal(backend, &lines, &reference, healed);
}

fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
fn clock_target() -> Target {
    Target::Name("clock".into())
}
fn step(nanoseconds: i64) -> Action {
    Action::StepRealtime {
        target: clock_target(),
        nanoseconds: NonZeroI64::new(nanoseconds).unwrap(),
    }
}
/// Host-driven scenario through the public unified schedule: drift, a
/// negative step and its inverse heal, a step bound to a crashed boot,
/// reboot and drift heal, with holds before the drift and the step.
fn host_plan<N>(b: &mut Barrier<'_, N>, now: Timestamp, held: Option<&FaultRecord>, live: &mut bool)
where
    N: Machine + Send + 'static,
    N::Context: 'static,
    N::Error: Debug + Send + 'static,
{
    let result = (|| -> Result<(), Error<N::Error>> {
        if now == at(0) && held.is_none() {
            b.schedule(
                at(500_000_000),
                key("drift"),
                Action::Drift {
                    target: clock_target(),
                    rate: ClockRate::new(100_000_000).unwrap(),
                },
            )?;
            b.schedule(at(2_500_000_000), key("back"), step(-2_000_000_000))?;
            b.schedule(
                at(3_500_000_000),
                key("heal-back"),
                Action::Heal(HealTarget::Input(key("back"))),
            )?;
            b.schedule(
                at(4_000_000_000),
                key("crash"),
                Action::Crash(clock_target()),
            )?;
            b.schedule(at(4_000_000_000), key("late"), step(1_000_000_000))?;
            b.schedule(
                at(4_500_000_000),
                key("reboot"),
                Action::Reboot(clock_target()),
            )?;
            b.schedule(
                at(5_500_000_000),
                key("heal-drift"),
                Action::Heal(HealTarget::Input(key("drift"))),
            )?;
            return b.pause_before_next_fault(PlaybackRate::Unlimited);
        }
        match held.map(|record| record.action) {
            // Release the drift and hold again before the next effective fault.
            Some(FaultAction::Drift(_)) => b.pause_before_next_fault(PlaybackRate::Unlimited),
            Some(_) => b.set_rate(PlaybackRate::Unlimited),
            None => {
                if !*live && now >= at(3_000_000_000) {
                    // The applied step's handle keeps its key live.
                    assert!(matches!(
                        b.schedule(now, key("back"), step(1)),
                        Err(Error::LiveInputKey(_))
                    ));
                    *live = true;
                }
                Ok(())
            }
        }
    })();
    result.unwrap();
}
fn check_host(backend: &str, (events, clock, runs, held): &Outcome) {
    assert_eq!(
        held.iter().map(|record| record.action).collect::<Vec<_>>(),
        [
            FaultAction::Drift(ClockRate::new(100_000_000).unwrap()),
            FaultAction::StepRealtime(-2_000_000_000)
        ]
    );
    let admitted: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(record) => match &record.origin {
                FaultOrigin::Input { key } => Some((key.as_str().to_owned(), record.action)),
                _ => None,
            },
            _ => None,
        })
        .collect();
    assert_eq!(
        admitted,
        [
            (
                "drift".to_string(),
                FaultAction::Drift(ClockRate::new(100_000_000).unwrap())
            ),
            ("back".into(), FaultAction::StepRealtime(-2_000_000_000)),
            ("heal-back".into(), FaultAction::Heal),
            ("crash".into(), FaultAction::Crash),
            ("late".into(), FaultAction::StepRealtime(1_000_000_000)),
            ("reboot".into(), FaultAction::Reboot),
            ("heal-drift".into(), FaultAction::Heal),
        ]
    );
    for (record, status, ..) in crate::driver::fixture::settlements(events) {
        let expected = if record.action == FaultAction::StepRealtime(1_000_000_000) {
            // Bound to the crashed boot at admission.
            SettlementStatus::Unavailable
        } else {
            SettlementStatus::Applied
        };
        assert_eq!(status, expected, "{backend}: {record:?}");
    }
    let reference = rates(events, *clock);
    assert_eq!(reference.0.len(), 2, "{backend}: drift then heal");
    assert_eq!(reference.0[0], (500_000_000, 100_000_000));
    assert_eq!(reference.0[1], (5_500_000_000, 0));
    let lines = lines(events, *clock, crate::machine::TEST_EPOCH_SECONDS);
    check_time_base(backend, &lines, runs, &reference);
    check_boots(backend, &lines, 2);
    check_sleep(backend, &lines, &reference);
    // The negative step succeeded with a real epoch: realtime moved back 2 s,
    // the cancel-on-set timer was cancelled, and the counter did not move.
    let canceled = find(&lines, 0, "CANCELED")[0];
    let stepped = canceled.rt - canceled.cnt;
    assert!(
        stepped > -2_000_000_000 - LATENCY && stepped <= -2_000_000_000,
        "{backend}: {canceled:?}"
    );
    assert!(canceled.at >= 2_500_000_000 && canceled.at < 3_500_000_000);
    // The inverse heal stepped realtime forward again.
    let restored = find(&lines, 0, "TICK")
        .into_iter()
        .find(|line| line.at > 3_600_000_000)
        .unwrap();
    assert!(
        restored.cnt - restored.rt >= 0 && restored.cnt - restored.rt < LATENCY,
        "{backend}: {restored:?}"
    );
    // Drift persists across reboot.
    let reboot = find(&lines, 1, "BOOT")[0];
    assert!(reboot.at > 4_500_000_000 && reboot.cnt - i128::from(reboot.at) > 350_000_000);
    check_heal(backend, &lines, &reference, 5_500_000_000);
}

/// A small epoch so a bounded negative step falls below CLOCK_MONOTONIC.
const REFUSAL_EPOCH_SECONDS: u32 = 3_600;
fn refusal_plan<N>(b: &mut Barrier<'_, N>, now: Timestamp)
where
    N: Machine + Send + 'static,
    N::Context: 'static,
    N::Error: Debug + Send + 'static,
{
    if now == at(0) {
        b.schedule(at(1_500_000_000), key("below"), step(-7_200_000_000_000))
            .unwrap();
    }
}
/// Linux refuses a step below CLOCK_MONOTONIC. This records the branch's
/// current, unapproved handling: the fault settles Rejected without failing
/// the run, and realtime is unchanged.
fn check_refusal(backend: &str, (events, clock, runs, _): &Outcome) {
    let settled = crate::driver::fixture::settlements(events);
    let refused: Vec<_> = settled
        .iter()
        .filter(|(record, ..)| record.action == FaultAction::StepRealtime(-7_200_000_000_000))
        .collect();
    assert_eq!(refused.len(), 1, "{backend}");
    assert_eq!(refused[0].1, SettlementStatus::Rejected);
    let detail = events.iter().find_map(|e| match &e.kind {
        EventKind::FaultSettled { record, detail, .. }
            if record.action == FaultAction::StepRealtime(-7_200_000_000_000) =>
        {
            Some(detail.clone())
        }
        _ => None,
    });
    assert_eq!(detail.as_deref(), Some("guest refused the realtime step"));
    let lines = lines(events, *clock, REFUSAL_EPOCH_SECONDS);
    let reference = rates(events, *clock);
    assert!(reference.0.is_empty());
    check_time_base(backend, &lines, runs, &Reference(vec![]));
    check_boots(backend, &lines, 1);
    // Realtime was below the requested target: its lead over monotonic is the
    // epoch plus the boot interval, less than the 2 h step.
    let after = lines.iter().find(|line| line.at > 2_000_000_000).unwrap();
    assert!(after.cnt - after.rt >= 0 && after.cnt - after.rt < LATENCY);
    assert!(
        i128::from(REFUSAL_EPOCH_SECONDS) * 1_000_000_000 + after.rt - after.mono
            < 7_200_000_000_000
    );
    assert!(find(&lines, 0, "CANCELED").is_empty());
}

fn tcg(workers: usize, migrate: bool, plan: Plan) -> Outcome {
    let qemu = input("SEEDBOX_TCG_QEMU");
    let mut factory =
        super::tcg::Factory::new(&qemu, &input("SEEDBOX_TCG_ASSETS"), plan.epoch()).unwrap();
    scenario(
        "TCG",
        workers,
        migrate,
        plan,
        move |definition| factory.create(definition).unwrap(),
        move |saved: &super::tcg::Snapshot| super::tcg::TcgMachine::from_snapshot(&qemu, saved),
    )
}
#[test]
#[ignore = "requires native TCG kernel/helper and clock-fault application"]
fn tcg_clock_faults_guest_drift_step_reboot_snapshot_and_workers() {
    let serial = tcg(1, false, Plan::Guest);
    check_guest("TCG", &serial);
    assert_eq!(serial, tcg(4, false, Plan::Guest), "workers");
    assert_eq!(serial, tcg(1, true, Plan::Guest), "restored mid-drift");
}
#[test]
#[ignore = "requires native TCG kernel/helper and clock-fault application"]
fn tcg_clock_faults_host_drift_negative_step_heal_holds_and_workers() {
    let serial = tcg(1, false, Plan::Host);
    check_host("TCG", &serial);
    assert_eq!(serial, tcg(4, false, Plan::Host), "workers");
}
#[test]
#[ignore = "requires native TCG kernel/helper and clock-fault application"]
fn tcg_clock_faults_step_below_monotonic_is_refused() {
    check_refusal("TCG", &tcg(1, false, Plan::Refusal));
}
#[cfg(seedbox_pv)]
fn pv(workers: usize, migrate: bool, plan: Plan) -> Outcome {
    let worker = input("SEEDBOX_PV_WORKER");
    let mut factory =
        super::pv::Factory::new(&worker, &input("SEEDBOX_PV_ASSETS"), plan.epoch()).unwrap();
    scenario(
        "PV",
        workers,
        migrate,
        plan,
        move |definition| factory.create(definition).unwrap(),
        move |saved: &super::pv::Snapshot| {
            super::pv::PVMachine::from_snapshot(&worker, saved).map_err(std::io::Error::other)
        },
    )
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and clock-fault application"]
fn pv_clock_faults_guest_drift_step_reboot_snapshot_and_workers() {
    let serial = pv(1, false, Plan::Guest);
    check_guest("PV", &serial);
    assert_eq!(serial, pv(4, false, Plan::Guest), "workers");
    assert_eq!(serial, pv(1, true, Plan::Guest), "restored mid-drift");
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and clock-fault application"]
fn pv_clock_faults_host_drift_negative_step_heal_holds_and_workers() {
    let serial = pv(1, false, Plan::Host);
    check_host("PV", &serial);
    assert_eq!(serial, pv(4, false, Plan::Host), "workers");
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and clock-fault application"]
fn pv_clock_faults_step_below_monotonic_is_refused() {
    check_refusal("PV", &pv(1, false, Plan::Refusal));
}
