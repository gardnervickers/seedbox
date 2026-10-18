use super::*;
use crate::clock::Clock;
use std::sync::Arc;
type PVMachine = crate::machine::test_environment::Environment<super::PVMachine>;
use crate::rng::SeededRng;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

use super::test_image::{program, timed_guest, Op};

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
    let machine = super::PVMachine::from_boot_inputs(
        Path::new("/missing/pv-worker"),
        program(&[Op::Spin]),
        b"initrd".to_vec(),
    )
    .unwrap();
    crate::machine::test_environment::check_deferred_startup_failure(machine, "/missing/pv-worker");
}

#[test]
fn complete_kernel_arguments_survive_uninitialized_snapshot() {
    let arguments = "rdinit=/custom caller=a caller=b";
    let mut machine = super::PVMachine::construct(
        Path::new("/not-spawned"),
        timed_guest(),
        b"initrd".to_vec(),
        arguments.into(),
        None,
        None,
    )
    .unwrap();
    let snapshot = machine.capture().unwrap();
    let restored = super::PVMachine::from_snapshot(Path::new("/not-spawned"), &snapshot).unwrap();
    assert_eq!(restored.command_line, arguments);
    assert!(super::PVMachine::construct(
        Path::new("/not-spawned"),
        timed_guest(),
        b"initrd".to_vec(),
        "bad\0argument".into(),
        None,
        None
    )
    .is_err());
}

#[test]
fn network_identity_survives_uninitialized_snapshot_without_repeating_arguments() {
    let interface = crate::definition::NetworkInterface::new(
        crate::definition::MacAddress::new([2, 0, 0, 0, 0, 9]).unwrap(),
        std::net::Ipv4Addr::new(192, 168, 4, 9),
        0,
    )
    .unwrap();
    let (arguments, network) = super::command_line("rdinit=/init", Some(&interface));
    assert_eq!(arguments, "rdinit=/init seedbox.ipv4=192.168.4.9/0");
    let mut machine = super::PVMachine::construct(
        Path::new("/not-spawned"),
        timed_guest(),
        b"initrd".to_vec(),
        arguments.clone(),
        None,
        network,
    )
    .unwrap();
    let snapshot = machine.capture().unwrap();
    let restored = super::PVMachine::from_snapshot(Path::new("/not-spawned"), &snapshot).unwrap();
    assert_eq!(restored.command_line, arguments);
    assert_eq!(restored.network, Some([2, 0, 0, 0, 0, 9]));
    assert_eq!(super::command_line("a", None), ("a".into(), None));
}

#[test]
fn kernel_argument_byte_limit_prevents_silent_linux_truncation() {
    assert!(super::boot::arguments(&"x".repeat(2047)).is_ok());
    assert!(super::boot::arguments(&"x".repeat(2048)).is_err());
    assert!(super::boot::arguments(&"é".repeat(1024)).is_err());
}
