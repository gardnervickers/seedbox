use super::*;
use crate::clock::Clock;
use crate::driver::{fixture::auto_key, Target};
use std::sync::Arc;
type PVMachine = crate::machine::test_environment::Environment<super::PVMachine>;
use crate::driver::{Action, StepOutcome};
use crate::rng::SeededRng;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

use super::test_image::{program, Op};

fn timed_guest() -> Vec<u8> {
    program(&[Op::Ready, Op::Timer(5), Op::Idle, Op::PowerOff])
}

#[test]
fn invalid_inputs_fail_before_spawning_worker() {
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let error = PVMachine::new(
        Path::new("/no-worker"),
        vec![],
        vec![],
        SeededRng::new(1),
        clock,
    )
    .err()
    .unwrap();
    assert!(!error.to_string().contains("machine worker"));
}

// Invoke explicitly with SEEDBOX_PV_WORKER set to a signed seedbox-pv-worker binary.
// Default portable tests do not open HVF/KVM.
#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_driver_isolates_two_guests_and_restores_independent_snapshots() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let mut driver = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            (0..2)
                .map(|_| {
                    PVMachine::configured(
                        executable,
                        timed_guest(),
                        b"initrd".to_vec(),
                        SeededRng::new(7),
                        clock.clone(),
                    )
                })
                .collect()
        },
    )
    .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    let snapshot = driver
        .with_barrier(|barrier| {
            let ids: Vec<_> = barrier.machines().map(|(id, _)| id).collect();
            for id in &ids {
                assert_eq!(
                    barrier.stopped(*id)?,
                    StopReason::Blocked {
                        reached: at(0),
                        deadline: Some(at(5))
                    }
                );
            }
            let snapshot = barrier.snapshot(ids[0])?;
            barrier.schedule(at(3), auto_key(), Action::Crash(Target::Machine(ids[1])))?;
            Ok(snapshot)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(3) }
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished {
            at: at(5),
            reason: ExitReason::Shutdown
        }
    );
    drop(driver);
    // The checkpoint survives the workers and temporary boot/checkpoint files.
    let mut restored = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                .map_err(std::io::Error::other)?])
        },
    )
    .unwrap();
    assert_eq!(
        restored.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    assert_eq!(
        restored.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        restored.step(at(10)).unwrap(),
        StepOutcome::Finished {
            at: at(5),
            reason: ExitReason::Shutdown
        }
    );
    let mut second = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                .map_err(std::io::Error::other)?])
        },
    )
    .unwrap();
    assert_eq!(
        second.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    assert_eq!(
        second.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        second.step(at(10)).unwrap(),
        StepOutcome::Finished {
            at: at(5),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_indefinite_idle_and_capture_failure_keep_valid_machine() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(37));
    let code = program(&[Op::Ready, Op::Idle]);
    let mut machine = PVMachine::new(
        Path::new(&executable),
        code,
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
    assert_eq!(clock.now(), 37);
    let existing = machine.directory.path().join("kernel");
    assert!(machine
        .worker()
        .unwrap()
        .call(Request::Capture(existing))
        .is_err());
    let snapshot = machine.snapshot().unwrap();
    assert_eq!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
    machine.request(crate::pv::Request::Crash).unwrap();
    assert!(
        matches!(machine.run(&mut (), at(100)).unwrap(), StopReason::Exited { reached, reason: ExitReason::Crashed } if reached == at(100))
    );
    assert!(machine.run(&mut (), at(101)).is_err());
    assert!(machine.snapshot().is_ok());
    drop(machine);
    let mut branch = PVMachine::from_snapshot(Path::new(&executable), &snapshot, clock).unwrap();
    assert_eq!(
        branch.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(37),
            deadline: None
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor, SEEDBOX_PV_WORKER and SEEDBOX_PV_KERNEL"]
fn native_linux_boot_and_program_poweroff_use_the_driver() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let kernel_path = std::env::var_os("SEEDBOX_PV_KERNEL").expect("set SEEDBOX_PV_KERNEL");
    let kernel = fs::read(kernel_path).unwrap();
    let mut driver = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            Ok(vec![PVMachine::configured(
                Path::new(&executable),
                kernel,
                super::test_image::linux_initrd(),
                SeededRng::new(7),
                clock,
            )?])
        },
    )
    .unwrap();
    for tick in 1..=10_000 {
        match driver.step(at(tick * 1_000_000)).unwrap() {
            StepOutcome::Finished { .. } => {
                driver
                    .with_barrier(|barrier| {
                        let (_, machine) = barrier.machines().next().unwrap();
                        let console = String::from_utf8_lossy(&machine.console);
                        assert!(console.contains("PV_LINUX_PASS"), "{console}");
                        Ok(())
                    })
                    .unwrap();
                return;
            }
            StepOutcome::Blocked { .. } => panic!("Linux blocked without a timer"),
            _ => {}
        }
    }
    panic!("Linux failed to power off within the simulation bound");
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_horizon_is_accepted_before_following_poweroff() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let code = program(&[Op::Ready, Op::Yield, Op::PowerOff]);
    let mut driver = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            Ok(vec![PVMachine::configured(
                executable,
                code,
                b"initrd".to_vec(),
                SeededRng::new(1),
                clock,
            )?])
        },
    )
    .unwrap();
    assert_eq!(
        driver.step(at(40)).unwrap(),
        StepOutcome::BarrierReached { at: at(40) }
    );
    let snapshot = driver
        .with_barrier(|barrier| {
            let id = barrier.machines().next().unwrap().0;
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            barrier.snapshot(id)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(50)).unwrap(),
        StepOutcome::Finished {
            at: at(40),
            reason: ExitReason::Shutdown
        }
    );
    drop(driver);
    let mut branch = crate::driver::fixture::driver(
        at(40),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(40)).as_nanos()));
            Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                .map_err(std::io::Error::other)?])
        },
    )
    .unwrap();
    assert_eq!(
        branch.step(at(50)).unwrap(),
        StepOutcome::Finished {
            at: at(40),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_runnable_gap_adopts_time_without_executing_guest_work() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let code = program(&[Op::Ready, Op::Yield, Op::PowerOff]);
    let mut machine = PVMachine::new(
        executable,
        code,
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::BoundaryReached
    );
    let snapshot = machine.snapshot().unwrap();
    clock.set(30);
    assert!(machine.run(&mut (), at(29)).is_err());
    assert_eq!(machine.reached, 10); // Invalid budget did not adopt time or execute.
    assert_eq!(
        machine.run(&mut (), at(30)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.reached, 30);
    assert_eq!(clock.now(), 30);
    // The instruction following the old boundary powers off. It must still be
    // pending after time adoption, and executes at 30 rather than at 10.
    assert_eq!(
        machine.run(&mut (), at(31)).unwrap(),
        StopReason::Exited {
            reached: at(30),
            reason: ExitReason::Shutdown,
        }
    );
    assert!(machine.run(&mut (), at(31)).is_err());
    let mut branch = PVMachine::from_snapshot(executable, &snapshot, clock).unwrap();
    assert_eq!(
        branch.run(&mut (), at(30)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(
        branch.run(&mut (), at(31)).unwrap(),
        StopReason::Exited {
            reached: at(30),
            reason: ExitReason::Shutdown,
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_overdue_timer_is_delivered_at_adopted_time_after_a_gap() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let executable = Path::new(&executable);
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let mut machine = PVMachine::new(
        executable,
        timed_guest(),
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(1)).unwrap(),
        StopReason::Blocked {
            reached: at(0),
            deadline: Some(at(5)),
        }
    );
    let snapshot = machine.snapshot().unwrap();
    // Adoption before the deadline must preserve the original timer.
    clock.set(3);
    assert_eq!(
        machine.run(&mut (), at(3)).unwrap(),
        StopReason::Blocked {
            reached: at(3),
            deadline: Some(at(5)),
        }
    );
    clock.set(30);
    assert_eq!(
        machine.run(&mut (), at(30)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(
        machine.run(&mut (), at(31)).unwrap(),
        StopReason::Exited {
            reached: at(30),
            reason: ExitReason::Shutdown,
        }
    );
    let mut branch = PVMachine::from_snapshot(executable, &snapshot, clock).unwrap();
    assert_eq!(
        branch.run(&mut (), at(31)).unwrap(),
        StopReason::Exited {
            reached: at(30),
            reason: ExitReason::Shutdown,
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_time_adoption_does_not_wake_indefinitely_blocked_work() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    // Idle with no timer armed; the power-off must remain unexecuted.
    let code = program(&[Op::Ready, Op::Idle, Op::PowerOff]);
    let mut machine = PVMachine::new(
        Path::new(&executable),
        code,
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(1)).unwrap(),
        StopReason::Blocked {
            reached: at(0),
            deadline: None,
        }
    );
    clock.set(30);
    assert_eq!(
        machine.run(&mut (), at(30)).unwrap(),
        StopReason::Blocked {
            reached: at(30),
            deadline: None,
        }
    );
    assert_eq!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Blocked {
            reached: at(30),
            deadline: None,
        }
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn native_repeated_run_keeps_execution_position_when_shared_clock_lags() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let code = program(&[Op::Ready, Op::Yield, Op::PowerOff]);
    let mut machine = PVMachine::new(
        Path::new(&executable),
        code,
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(clock.now(), 0);
    assert_eq!(
        machine.run(&mut (), at(20)).unwrap(),
        StopReason::Exited {
            reached: at(10),
            reason: ExitReason::Shutdown
        }
    );
    assert_eq!(clock.now(), 0);
}

#[test]
fn deferred_startup_failure_is_a_run_error_and_invalidates_the_machine() {
    let mut machine = super::PVMachine::from_boot_inputs(
        Path::new("/missing/pv-worker"),
        program(&[Op::Spin]),
        b"initrd".to_vec(),
    )
    .unwrap();
    let clock = crate::clock::SimulationClock::new(0);
    let mut rng = SeededRng::new(9);
    let mut expected = rng.clone();
    use crate::rng::RngStream;
    expected.fill_bytes(&mut [0; 8]);
    assert!(machine.snapshot().is_ok());
    let error = machine
        .run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: crate::pv::Inbox::new(
                    &mut Default::default(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                ),
                outbox: crate::pv::Outbox::new(
                    &mut Vec::new(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                    at(0),
                ),
            },
            at(0),
        )
        .unwrap_err();
    assert!(error.to_string().contains("/missing/pv-worker"));
    assert!(machine.snapshot().is_err());
    assert!(machine
        .run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: crate::pv::Inbox::new(
                    &mut Default::default(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock))
                ),
                outbox: crate::pv::Outbox::new(
                    &mut Vec::new(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                    at(1)
                ),
            },
            at(1)
        )
        .is_err());
    let mut got = [0; 8];
    let mut want = [0; 8];
    rng.fill_bytes(&mut got);
    expected.fill_bytes(&mut want);
    assert_eq!(got, want);
}

#[test]
fn complete_kernel_arguments_survive_uninitialized_snapshot() {
    let arguments = "rdinit=/custom caller=a caller=b";
    let mut machine = super::PVMachine::construct(
        Path::new("/not-spawned"),
        timed_guest(),
        b"initrd".to_vec(),
        arguments.into(),
    )
    .unwrap();
    let snapshot = machine.capture().unwrap();
    let restored = super::PVMachine::from_snapshot(Path::new("/not-spawned"), &snapshot).unwrap();
    assert_eq!(restored.command_line, arguments);
    assert!(super::PVMachine::construct(
        Path::new("/not-spawned"),
        timed_guest(),
        b"initrd".to_vec(),
        "bad\0argument".into()
    )
    .is_err());
}

#[test]
fn kernel_argument_byte_limit_prevents_silent_linux_truncation() {
    assert!(super::boot::arguments(&"x".repeat(2047)).is_ok());
    assert!(super::boot::arguments(&"x".repeat(2048)).is_err());
    assert!(super::boot::arguments(&"é".repeat(1024)).is_err());
}
