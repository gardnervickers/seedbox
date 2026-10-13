#![cfg(unix)]

use seedbox_machine as machine_crate;
use seedbox_machine::{
    driver::{Action, Driver, MachineId, MachineResponse, StepOutcome},
    machine::{tcg::TcgMachine, ExitReason, Machine, StopReason, Timestamp},
    rng::RngStream,
};
#[path = "support/prepared_application.rs"]
mod prepared;

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
use std::{
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

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
    let mut backend = machine_crate::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
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
        move |definition| {
            let mut machine = backend.create(&definition)?;
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
    let batch = driver.with_barrier(|b| Ok(b.drain_responses())).unwrap();
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
fn advance(driver: &mut Harness, id: MachineId, now: Timestamp) -> Timestamp {
    let upto = match stopped(driver, id) {
        StopReason::Blocked {
            deadline: Some(at), ..
        } => at,
        StopReason::Blocked { deadline: None, .. } => {
            panic!("guest has no wakeup: {}", output(driver, id))
        }
        _ => at(now.as_nanos() + 1_000_000),
    };
    step(driver, upto)
}
fn finish(driver: &mut Harness, id: MachineId, mut now: Timestamp) -> Timestamp {
    let started = Instant::now();
    loop {
        if let StopReason::Exited { reached, reason } = stopped(driver, id) {
            assert_eq!(reason, ExitReason::Shutdown);
            return reached;
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "shutdown timed out: {}",
            output(driver, id)
        );
        now = advance(driver, id, now);
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
            b.schedule(at(0), Action::Shutdown(id))?;
            b.schedule(at(0), Action::Shutdown(id))
        })
        .unwrap();
    step(&mut driver, at(0));
    let reached = finish(&mut driver, id, at(0));
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
        driver.with_barrier(|b| b.schedule(reached, Action::Shutdown(id))),
        Err(seedbox_machine::driver::Error::Completed)
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
    let started = Instant::now();
    let mut now = at(0);
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "Linux boot timed out: {}",
            output(&mut driver, id)
        );
        now = advance(&mut driver, id, now);
        if output(&mut driver, id).contains("app\0out\n") {
            break;
        }
    }
    let before = output(&mut driver, id);
    let resume = at(now.as_nanos() + 50_000_000);
    driver
        .with_barrier(|b| {
            b.schedule(now, Action::Pause(id))?;
            b.schedule(now, Action::Shutdown(id))?;
            b.schedule(resume, Action::Resume(id))
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
    let reached = finish(&mut driver, id, resume);
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
            move |_| TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &snapshot),
        )
        .unwrap();
        let mut branch = Harness {
            driver: branch_driver,
            events: Vec::new(),
        };
        let id = machine_id(&mut branch);
        assert_eq!(finish(&mut branch, id, resume), reached);
        assert_eq!(output(&mut branch, id), expected);
        assert_no_natural_outcome(&branch);
    }
}
