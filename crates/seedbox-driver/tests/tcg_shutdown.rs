#![cfg(unix)]
mod support {
    pub mod epoch;
    #[allow(dead_code)]
    pub mod native_driver;
    pub mod prepared_application;
}
use support::epoch;
use support::native_driver;
use support::prepared_application as prepared;

use native_driver::auto_key;
use seedbox_driver::driver::{
    Action, Driver, EventKind, MachineId, MachineResponse, StepOutcome, Target,
};
use seedbox_machine::{
    machine::{ExitReason, Machine, StopReason, Timestamp},
    rng::RngStream,
};
use seedbox_machine_tcg::machine::tcg::TcgMachine;
use seedbox_test_time::{GuestProgress, HangGuard};

struct Harness {
    driver: Driver<TestMachine>,
    events: Vec<MachineResponse>,
}
impl std::ops::Deref for Harness {
    type Target = Driver<TestMachine>;
    fn deref(&self) -> &Self::Target {
        &self.driver
    }
}
impl std::ops::DerefMut for Harness {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.driver
    }
}

#[derive(Clone)]
struct TestRng;
impl RngStream for TestRng {
    fn fill_bytes(&mut self, buffer: &mut [u8]) {
        buffer.fill(0);
    }
}
type TestMachine = TcgMachine;
use std::{fs, path::PathBuf, time::Duration};

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .expect("run TCG checks inside nix develop")
        .into()
}
fn driver(restore_initial: bool) -> Harness {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let driver = Driver::new(
        at(0),
        Duration::from_millis(1),
        1,
        TestRng,
        || Ok(()),
        definition,
        Vec::new(),
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| {
            let mut machine = backend.create(definition)?;
            if restore_initial {
                let snapshot = machine.snapshot()?;
                drop(machine);
                TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &snapshot)
            } else {
                Ok(machine)
            }
        },
    )
    .unwrap();
    Harness {
        driver,
        events: Vec::new(),
    }
}
fn machine_id(driver: &mut Harness) -> MachineId {
    driver
        .with_barrier(|b| Ok(b.machines().next().unwrap().0))
        .unwrap()
}
fn output(driver: &mut Harness, id: MachineId) -> String {
    let batch = driver
        .drain_observations()
        .events
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Output(output) => Some(output),
            _ => None,
        })
        .collect::<Vec<_>>();
    driver.events.extend(batch);
    let bytes: Vec<_> = driver
        .events
        .iter()
        .filter(|event| event.machine == id)
        .filter_map(|event| match &event.payload {
            seedbox_machine::pv::Response::ApplicationOutput { bytes, .. } => {
                Some(bytes.as_slice())
            }
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
fn stopped(driver: &mut Harness, id: MachineId) -> StopReason {
    driver.with_barrier(|b| b.stopped(id)).unwrap()
}
fn step(driver: &mut Harness, upto: Timestamp) -> Timestamp {
    match driver.step(upto).unwrap() {
        StepOutcome::BarrierReached { at }
        | StepOutcome::Blocked { at }
        | StepOutcome::Finished { at, .. } => at,
        other => panic!("unexpected playback state: {other:?}"),
    }
}
fn advance(
    driver: &mut Harness,
    id: MachineId,
    now: Timestamp,
    progress: &mut GuestProgress,
) -> Timestamp {
    let upto = match stopped(driver, id) {
        StopReason::Blocked {
            deadline: Some(at), ..
        } => at,
        StopReason::Blocked { deadline: None, .. } => {
            panic!("guest has no wakeup: {}", output(driver, id))
        }
        _ => at(now.as_nanos() + 1_000_000),
    };
    let limit = progress
        .next(
            Duration::from_nanos(now.as_nanos()),
            Duration::from_nanos(
                upto.as_nanos()
                    .checked_sub(now.as_nanos())
                    .expect("backwards guest deadline")
                    .max(1),
            ),
        )
        .unwrap_or_else(|error| {
            panic!(
                "{}: {error}; now={now:?}; last_stop={:?}; outputs={}; pending=lifecycle/output",
                progress.phase(),
                stopped(driver, id),
                output(driver, id)
            )
        });
    // Keep a due wakeup at this instant; next() bounds any repeated rounds.
    step(
        driver,
        at(upto.as_nanos().min(limit.as_nanos().try_into().unwrap())),
    )
}
fn phase(name: &str, now: Timestamp, deadline: u64) -> GuestProgress {
    // At most lifecycle adoption and final exit harvest can be same-instant
    // rounds; queued pause/resume controls have already been explicitly stepped.
    GuestProgress::new(
        name,
        Duration::from_nanos(now.as_nanos()),
        Duration::from_nanos(deadline),
        2,
    )
    .unwrap()
}
fn finish(driver: &mut Harness, id: MachineId, mut now: Timestamp, deadline: u64) -> Timestamp {
    let guard = HangGuard::new(
        "TCG shutdown Driver worker transport",
        Duration::from_secs(300),
    );
    let mut progress = phase("shutdown cleanup", now, deadline);
    loop {
        if let StopReason::Exited { reached, reason } = stopped(driver, id) {
            assert_eq!(reason, ExitReason::Shutdown);
            return reached;
        }
        guard.remaining().unwrap();
        now = advance(driver, id, now, &mut progress);
    }
}
fn assert_clean_shutdown(output: &str) {
    assert_eq!(output.matches("cleaned\n").count(), 1, "{output:?}");
}
fn assert_no_natural_outcome(driver: &Harness) {
    assert!(!driver.events.iter().any(|event| matches!(
        event.payload,
        seedbox_machine::pv::Response::ApplicationOutcome { .. }
    )));
}

#[test]
#[ignore = "requires patched QEMU, TCG factory assets and static service fixture"]
fn early_queued_request_after_restore_runs_real_guest_cleanup() {
    let mut driver = driver(true);
    let id = machine_id(&mut driver);
    driver
        .with_barrier(|b| {
            b.schedule(at(0), auto_key(), Action::Shutdown(Target::Machine(id)))?;
            b.schedule(at(0), auto_key(), Action::Shutdown(Target::Machine(id)))
        })
        .unwrap();
    step(&mut driver, at(0));
    // guest_service_fixture.c sleeps one second after its output, and its
    // SIGTERM handler only writes "cleaned" and exits. Five seconds allow
    // Linux boot; one extra second allows supervisor cleanup/poweroff timers.
    let reached = finish(&mut driver, id, at(0), 7_000_000_000);
    assert!(reached.as_nanos() < 100_000_000_000);
    let observed = output(&mut driver, id);
    if observed.contains("app\0out\n") {
        assert_clean_shutdown(&observed);
    }
    assert_no_natural_outcome(&driver);
    assert_eq!(
        driver
            .events
            .iter()
            .filter(|event| matches!(event.payload, seedbox_machine::pv::Response::Ready))
            .count(),
        1
    );
    assert!(observed.matches("cleaned\n").count() <= 1);
    // An already-exited request is harmless and cannot reboot the guest.
    assert!(matches!(
        driver.with_barrier(|b| b.schedule(
            reached,
            auto_key(),
            Action::Shutdown(Target::Machine(id))
        )),
        Err(seedbox_driver::driver::Error::Completed)
    ));
    assert!(matches!(
        stopped(&mut driver, id),
        StopReason::Exited { .. }
    ));
}

#[test]
#[ignore = "requires patched QEMU, TCG factory assets and static service fixture"]
fn paused_shutdown_stays_pending_until_resume_and_restores_without_duplicate() {
    let mut driver = driver(false);
    let id = machine_id(&mut driver);
    let guard = HangGuard::new(
        "TCG readiness Driver worker transport",
        Duration::from_secs(300),
    );
    let mut now = at(0);
    let mut progress = phase("Linux boot and service output", now, 5_000_000_000);
    while !output(&mut driver, id).contains("app\0out\n") {
        guard.remaining().unwrap();
        now = advance(&mut driver, id, now, &mut progress);
    }
    let before = output(&mut driver, id);
    let resume = at(now.as_nanos() + 50_000_000);
    driver
        .with_barrier(|b| {
            b.schedule(now, auto_key(), Action::Pause(Target::Machine(id)))?;
            b.schedule(now, auto_key(), Action::Shutdown(Target::Machine(id)))?;
            b.schedule(resume, auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    step(&mut driver, now);
    let pending = driver
        .with_barrier(|b| {
            assert!(b.paused(id)?);
            b.snapshot(id)
        })
        .unwrap();
    assert_eq!(output(&mut driver, id), before);
    // step() may commit an intermediate quantum before its requested bound.
    // Observe every stopped barrier until the queued Resume actually admits.
    let mut committed = now;
    while committed < resume {
        let previous = committed;
        committed = step(&mut driver, resume);
        assert!(committed > previous && committed <= resume);
        assert_eq!(output(&mut driver, id), before);
        let paused = driver.with_barrier(|b| b.paused(id)).unwrap();
        assert_eq!(paused, committed < resume);
    }
    assert_eq!(committed, resume); // Zero-budget resume executes no instructions.
    let delivered = driver.with_barrier(|b| b.snapshot(id)).unwrap();
    // No application work is required after resume: SIGTERM writes the
    // marker and exits, then supervision reaps/syncs/powers off. Allow one
    // simulated second for this control-only phase, shared by every branch.
    let shutdown_deadline = resume.as_nanos() + 1_000_000_000;
    let reached = finish(&mut driver, id, resume, shutdown_deadline);
    let expected = output(&mut driver, id)[before.len()..].to_owned();
    assert_clean_shutdown(&expected);
    assert_no_natural_outcome(&driver);
    drop(driver);

    // Both zero-budget captures retain the accepted request in device state,
    // before guest instructions can perform cleanup.
    for snapshot in [pending, delivered] {
        let definition = prepared::definition(
            fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
            256 << 20,
        );
        let branch_driver = Driver::new(
            resume,
            Duration::from_millis(1),
            1,
            TestRng,
            || Ok(()),
            definition,
            Vec::new(),
            Vec::new(),
            seedbox_driver::driver::NetworkConfig::default(),
            move |_| TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &snapshot),
        )
        .unwrap();
        let mut branch = Harness {
            driver: branch_driver,
            events: Vec::new(),
        };
        let id = machine_id(&mut branch);
        assert_eq!(finish(&mut branch, id, resume, shutdown_deadline), reached);
        assert_eq!(output(&mut branch, id), expected);
        assert_no_natural_outcome(&branch);
    }
}
