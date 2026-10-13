//! Native clock faults through the Driver on TCG and PV with one shared guest
//! fixture (`tests/fixtures/clock-faults.c`). The machine is wrapped to record
//! its own-time-base runs, so timer conversion is checked exactly against an
//! independent reference mapping built from the published rate events.
use crate::{
    driver::{Driver, Event, EventKind, MachineId, SettlementStatus, StepOutcome, Transition},
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Response, Stream},
    rng::RngStream,
};
use std::{
    fmt::Debug,
    fs,
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
    let controller = crate::definition::MachineDefinition {
        name: "controller".into(),
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
                argv: vec!["/application".into(), "controller".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
    };
    let mut clock = controller.clone();
    clock.name = "clock".into();
    clock.application.launch.argv = vec!["/application".into(), "clock".into()];
    (controller, clock)
}

/// Published events, the clock machine's identity and its recorded runs.
type Outcome = (Vec<Event>, MachineId, Vec<Run>);
fn scenario<M, F>(
    backend: &str,
    workers: usize,
    migrate: bool,
    mut construct: impl FnMut(&crate::definition::MachineDefinition) -> M + Send + 'static,
    restore: F,
) -> Outcome
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
    F: Fn(&M::Snapshot) -> Result<M, M::Error> + Clone + Send + 'static,
{
    let (controller, clock) = definitions(backend);
    let runs = Arc::new(Mutex::new(Vec::new()));
    let recorded = runs.clone();
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(3),
        || Ok(()),
        controller,
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
    let mut migrated = !migrate;
    let mut now = at(0);
    loop {
        let upto = at(now.as_nanos() + QUANTUM);
        let result = driver.step(upto).unwrap();
        events.extend(driver.drain_events());
        now = match result {
            StepOutcome::Finished { reason, .. } => {
                assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
                break;
            }
            StepOutcome::BarrierReached { at } => at,
            other => panic!("{backend}: unexpected {other:?}"),
        };
        // Mid-drift, after the step and before the crash: restore in place.
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
    (events, clock, runs)
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
fn lines(events: &[Event], clock: MachineId) -> Vec<Line> {
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
                rt: value("rt"),
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
fn check(backend: &str, (events, clock, runs): &Outcome) {
    let rates: Vec<_> = events
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::Transition {
                machine,
                change: Transition::ClockRate(rate),
                ..
            } if machine == *clock => Some((event.at.as_nanos(), i64::from(rate.ppb()))),
            _ => None,
        })
        .collect();
    assert_eq!(rates.len(), 2, "{backend}: drift then heal");
    let healed = rates[1].0;
    assert_eq!((rates[0].1, rates[1].1), (100_000_000, 0));
    assert!(
        healed >= 5_000_000_000,
        "{backend}: heal after the reboot check"
    );
    let reference = Reference(rates);
    for settlement in crate::driver::fixture::settlements(events) {
        assert_eq!(settlement.1, SettlementStatus::Applied, "{settlement:?}");
    }
    let lost = events.iter().any(|event| {
        matches!(&event.kind, EventKind::FaultSettled { record, detail, .. }
            if record.handle.as_ref().map(ToString::to_string).as_deref() == Some("p2")
                && detail == "realtime step ended with its machine incarnation")
    });
    assert!(lost, "{backend}: healing the step after reboot");
    let lines = lines(events, *clock);
    // Every reading follows the machine time base: the counter read just
    // before the write is at most G(output time) and close to it.
    for line in &lines {
        let mapped = i128::from(reference.floor(line.at));
        assert!(
            line.cnt <= mapped && mapped - line.cnt < LATENCY,
            "{backend}: {line:?} G={mapped}"
        );
        assert!(line.raw <= line.cnt && line.mono <= line.cnt, "{line:?}");
    }
    let find = |boot, tag: &str| {
        lines
            .iter()
            .filter(|l| l.boot == boot && l.tag == tag)
            .collect::<Vec<_>>()
    };
    // Each boot starts realtime at epoch (zero here) plus the counter.
    for boot in [0, 1] {
        let line = find(boot, "BOOT")[0];
        assert!(
            line.cnt - line.rt >= 0 && line.cnt - line.rt < LATENCY,
            "{backend}: {line:?}"
        );
    }
    // Drift persists across reboot: the new boot already runs ahead of
    // simulation time by the accrued ten percent.
    let reboot = find(1, "BOOT")[0];
    assert!(reboot.at > 4_000_000_000 && reboot.cnt - i128::from(reboot.at) > 350_000_000);
    // A guest 1 s sleep under drift: monotonic, raw and the counter agree.
    let slept = find(0, "SLEPT");
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
    // The step moved realtime only, cancelled the cancel-on-set timer and fired
    // the absolute realtime timer well before its unstepped monotonic time.
    let boot = find(0, "BOOT")[0];
    let canceled = find(0, "CANCELED")[0];
    let stepped = canceled.rt - canceled.cnt;
    assert!(
        stepped > 2_000_000_000 - LATENCY && stepped <= 2_000_000_000,
        "{canceled:?}"
    );
    assert!(canceled.at >= 2_500_000_000);
    let fired = find(0, "FIRED")[0];
    assert!(fired.rt >= boot.rt + 5_000_000_000);
    assert!(
        fired.mono - boot.mono < 3_500_000_000,
        "{backend}: {fired:?}"
    );
    assert!(find(1, "CANCELED").is_empty() && find(1, "FIRED").is_empty());
    // Heal keeps the offset accrued until then: G(t) = t + accrued afterwards.
    let accrued = i128::from(reference.floor(healed)) - i128::from(healed);
    assert!(accrued > 400_000_000);
    let last = lines.last().unwrap();
    assert!(last.at > healed + 500_000_000);
    let ahead = last.cnt - i128::from(last.at);
    assert!(ahead <= accrued && ahead > accrued - LATENCY, "{last:?}");
    // Exact conversion. The Driver runs a blocked machine without inputs only
    // when its converted deadline is due: rate changes, snapshots and other
    // machines' work never run it, and this scenario has no pause/resume or
    // wake actions. Runs with inputs (requests, replies, lifecycle) are the only
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
                // The deadline converted under the earlier rates alone gives
                // another simulation wake. Rates here are never slow, so the
                // mapping is injective and the observed clock identifies the
                // wake: a stale (unconverted) wake would be observed differently.
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
    assert!(
        straddled >= 1 && reconverted >= 1,
        "{backend}: {straddled} straddling timers, {reconverted} re-converted"
    );
}

fn tcg(workers: usize, migrate: bool) -> Outcome {
    let qemu = input("SEEDBOX_TCG_QEMU");
    let mut factory = super::tcg::Factory::new(&qemu, &input("SEEDBOX_TCG_ASSETS")).unwrap();
    let restore_qemu = qemu.clone();
    scenario(
        "TCG",
        workers,
        migrate,
        move |definition| factory.create(definition).unwrap(),
        move |saved: &super::tcg::Snapshot| {
            super::tcg::TcgMachine::from_snapshot(&restore_qemu, saved)
        },
    )
}
#[test]
#[ignore = "requires native TCG kernel/helper and clock-fault application"]
fn tcg_clock_faults_drift_step_reboot_snapshot_and_workers() {
    let serial = tcg(1, false);
    check("TCG", &serial);
    assert_eq!(serial, tcg(4, false), "workers");
    assert_eq!(serial, tcg(1, true), "restored mid-drift");
}
#[cfg(seedbox_pv)]
fn pv(workers: usize, migrate: bool) -> Outcome {
    let worker = input("SEEDBOX_PV_WORKER");
    let mut factory = super::pv::Factory::new(&worker, &input("SEEDBOX_PV_ASSETS")).unwrap();
    let restore_worker = worker.clone();
    scenario(
        "PV",
        workers,
        migrate,
        move |definition| factory.create(definition).unwrap(),
        move |saved: &super::pv::Snapshot| {
            super::pv::PVMachine::from_snapshot(&restore_worker, saved)
                .map_err(std::io::Error::other)
        },
    )
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor/kernel/helper and clock-fault application"]
fn pv_clock_faults_drift_step_reboot_snapshot_and_workers() {
    let serial = pv(1, false);
    check("PV", &serial);
    assert_eq!(serial, pv(4, false), "workers");
    assert_eq!(serial, pv(1, true), "restored mid-drift");
}
