use super::*;
use crate::clock::Clock;
use std::sync::Arc;
type PVMachine = crate::machine::test_environment::Environment<super::PVMachine>;
use crate::rng::{RngStreamExt, SeededRng};

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn reboot_clears_timers_at_current_time_and_preserves_entropy_across_snapshot() {
    let executable = std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    // PV handshake, five-nanosecond sleep, then power off.
    let kernel = super::test_image::program(&[
        super::test_image::Op::Ready,
        super::test_image::Op::TimerAfterNow(5),
        super::test_image::Op::Idle,
        super::test_image::Op::PowerOff,
    ]);
    let mut machine = PVMachine::new(
        Path::new(&executable),
        kernel,
        b"initrd".to_vec(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    assert_eq!(
        machine.run(&mut (), at(1)).unwrap(),
        StopReason::Blocked {
            reached: at(0),
            deadline: Some(at(5))
        }
    );
    let snapshot = machine.snapshot().unwrap();
    let saved_rng = machine.rng.clone();
    clock.set(3);
    machine.request(crate::pv::Request::Reboot).unwrap();
    assert_eq!(clock.now(), 3);
    assert_eq!(machine.reached, 3);
    assert_eq!(machine.pending_boots, 0);
    assert_eq!(
        machine.run(&mut (), at(3)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(
        machine.run(&mut (), at(4)).unwrap(),
        StopReason::Blocked {
            reached: at(3),
            deadline: Some(at(8))
        }
    );
    let seed = machine.seed;
    let branch_clock = Arc::new(crate::clock::SimulationClock::new(3));
    let mut branch =
        PVMachine::from_snapshot(Path::new(&executable), &snapshot, branch_clock).unwrap();
    branch.rng = saved_rng;
    branch.request(crate::pv::Request::Reboot).unwrap();
    branch.run(&mut (), at(3)).unwrap();
    assert_eq!(branch.seed, seed);
    branch.request(crate::pv::Request::Crash).unwrap();
    branch.request(crate::pv::Request::Reboot).unwrap();
    branch.run(&mut (), at(3)).unwrap();
    assert_ne!(branch.seed, seed);
    clock.set(8);
    assert!(matches!(
        machine.run(&mut (), at(9)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    clock.set(9);
    machine.request(crate::pv::Request::Reboot).unwrap();
    assert_eq!(
        machine.run(&mut (), at(9)).unwrap(),
        StopReason::BoundaryReached
    );
}

use super::guest;

fn finish(machine: &mut PVMachine, clock: &crate::clock::SimulationClock) {
    // test-time: pending(bd-dd9, lifecycle-tests-finish-now-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
    let started = std::time::Instant::now();
    loop {
        // test-time: pending(bd-dd9, lifecycle-tests-finish-elapsed-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "guest did not shut down: {}",
            String::from_utf8_lossy(&machine.console)
        );
        let upto = clock.now() + 1_000_000;
        match machine.run(&mut (), at(upto)).unwrap() {
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                ..
            } => break,
            StopReason::BoundaryReached => clock.set(upto),
            StopReason::Blocked {
                deadline: Some(deadline),
                ..
            } => clock.set(deadline.as_nanos()),
            stop => panic!(
                "shutdown stalled {stop:?}: {}",
                String::from_utf8_lossy(&machine.console)
            ),
        }
    }
    let output = String::from_utf8_lossy(&machine.console);
    for marker in [
        "PV_SHUTDOWN_REQUESTED",
        "PV_CHILD_CLEANED",
        "PV_SHUTDOWN_COMPLETE",
    ] {
        assert_eq!(output.matches(marker).count(), 1, "{output}");
    }
}

#[test]
#[ignore = "requires signed SEEDBOX_PV_WORKER, SEEDBOX_PV_KERNEL, SEEDBOX_PV_CLANG and SEEDBOX_PV_LD"]
fn zero_budget_shutdown_snapshots_preserve_request_once() {
    let executable = guest::input("SEEDBOX_PV_WORKER");
    let kernel = fs::read(guest::input("SEEDBOX_PV_KERNEL")).unwrap();
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let mut machine = PVMachine::new(
        &executable,
        kernel,
        guest::initrd(),
        SeededRng::new(7),
        clock.clone(),
    )
    .unwrap();
    let mut expected = SeededRng::new(7);
    let seed = expected.next_u64();
    assert_eq!(machine.seed, seed);
    assert_eq!(
        serde_json::to_vec(&machine.rng).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let rng = serde_json::to_vec(&machine.rng).unwrap();
    machine.request(crate::pv::Request::Shutdown).unwrap();
    machine.request(crate::pv::Request::Shutdown).unwrap();
    assert_eq!(machine.reached, 0);
    assert!(machine.console.is_empty());
    assert_eq!(serde_json::to_vec(&machine.rng).unwrap(), rng);
    let pending = machine.snapshot().unwrap();
    // Adoption and queuing with no instruction budget: no Linux boot or cleanup
    // has run, and the request is now also owned by stopped device state.
    clock.set(50);
    assert_eq!(
        machine.run(&mut (), at(50)).unwrap(),
        StopReason::BoundaryReached
    );
    assert!(machine.console.is_empty());
    let queued = machine.snapshot().unwrap();
    drop(machine);
    for snapshot in [pending, queued] {
        let branch_clock = Arc::new(crate::clock::SimulationClock::new(50));
        let mut branch =
            PVMachine::from_snapshot(&executable, &snapshot, branch_clock.clone()).unwrap();
        assert_eq!(branch.seed, seed);
        branch.rng = serde_json::from_slice(&rng).unwrap();
        branch.request(crate::pv::Request::Shutdown).unwrap();
        finish(&mut branch, &branch_clock);
        branch.request(crate::pv::Request::Shutdown).unwrap();
        // Hard lifecycle reset discards the old latch/device backlog. If it
        // leaked into this boot, Linux would power off instead of waiting.
        branch_clock.set(branch.reached);
        let output_start = branch.console.len();
        branch.request(crate::pv::Request::Reboot).unwrap();
        assert!(!branch.shutdown);
        // test-time: pending(bd-dd9, lifecycle-tests-zero-budget-shutdown-snapshots-preserve-request-once-now-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
        let reboot_started = std::time::Instant::now();
        loop {
            // test-time: pending(bd-dd9, lifecycle-tests-zero-budget-shutdown-snapshots-preserve-request-once-elapsed-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
            assert!(reboot_started.elapsed() < std::time::Duration::from_secs(60));
            let upto = branch_clock.now() + 1_000_000;
            match branch.run(&mut (), at(upto)).unwrap() {
                StopReason::BoundaryReached => branch_clock.set(upto),
                StopReason::Blocked {
                    reached,
                    deadline: None,
                } => {
                    assert!(reached.as_nanos() >= 50);
                    break;
                }
                StopReason::Blocked {
                    deadline: Some(deadline),
                    ..
                } => branch_clock.set(deadline.as_nanos()),
                other => panic!("reboot replayed shutdown: {other:?}"),
            }
            let output = String::from_utf8_lossy(&branch.console[output_start..]);
            assert!(!output.contains("PV_SHUTDOWN_REQUESTED"), "{output}");
            if output.contains("PV_LIFECYCLE_READY") {
                break;
            }
        }
        branch.request(crate::pv::Request::Crash).unwrap();
        assert!(!branch.shutdown);
        branch.request(crate::pv::Request::Shutdown).unwrap();
        assert!(!branch.shutdown);
    }
}

#[test]
#[ignore = "requires signed HVF worker and SEEDBOX_PV_KERNEL, SEEDBOX_PV_CLANG, SEEDBOX_PV_LD"]
fn live_linux_ring_dma_snapshot_preserves_shutdown_at_newer_clock() {
    let executable = guest::input("SEEDBOX_PV_WORKER");
    let kernel = fs::read(guest::input("SEEDBOX_PV_KERNEL")).unwrap();
    let clock = Arc::new(crate::clock::SimulationClock::new(0));
    let mut machine = PVMachine::new(
        &executable,
        kernel,
        guest::initrd(),
        SeededRng::new(11),
        clock.clone(),
    )
    .unwrap();
    // test-time: pending(bd-dd9, lifecycle-tests-live-linux-ring-dma-snapshot-preserves-shutdown-at-newer-clock-now-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
    let started = std::time::Instant::now();
    while !String::from_utf8_lossy(&machine.console).contains("PV_LIFECYCLE_READY") {
        // test-time: pending(bd-dd9, lifecycle-tests-live-linux-ring-dma-snapshot-preserves-shutdown-at-newer-clock-elapsed-1) -- Native phase clock/progress deadline awaiting GuestProgress migration in bd-dd9.
        assert!(started.elapsed() < std::time::Duration::from_secs(60));
        let upto = clock.now() + 1_000_000;
        match machine.run(&mut (), at(upto)).unwrap() {
            StopReason::BoundaryReached => clock.set(upto),
            StopReason::Blocked {
                deadline: Some(deadline),
                ..
            } => clock.set(deadline.as_nanos()),
            StopReason::Blocked {
                reached,
                deadline: None,
            } => {
                clock.set(reached.as_nanos());
                assert!(String::from_utf8_lossy(&machine.console).contains("PV_LIFECYCLE_READY"));
            }
            other => panic!("unexpected boot stop {other:?}"),
        }
    }
    machine.request(crate::pv::Request::Shutdown).unwrap();
    let pending = machine.snapshot().unwrap();
    let output = machine.console.clone();
    let now = clock.now();
    machine.run(&mut (), at(now)).unwrap();
    assert_eq!(machine.console, output);
    let delivered = machine.snapshot().unwrap();
    drop(machine);
    for snapshot in [pending, delivered] {
        let branch_clock = Arc::new(crate::clock::SimulationClock::new(now + 50_000_000));
        let mut branch =
            PVMachine::from_snapshot(&executable, &snapshot, branch_clock.clone()).unwrap();
        branch.request(crate::pv::Request::Shutdown).unwrap();
        finish(&mut branch, &branch_clock);
    }
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn queued_boots_use_caller_entropy_and_machine_only_snapshots() {
    let executable = guest::input("SEEDBOX_PV_WORKER");
    use super::test_image::{program, Op};
    let code = program(&[Op::Ready, Op::PowerOff]);
    let machine =
        super::PVMachine::from_boot_inputs(&executable, code, b"initrd".to_vec()).unwrap();
    crate::machine::test_environment::check_queued_boot_entropy(
        machine,
        |saved| super::PVMachine::from_snapshot(&executable, saved).unwrap(),
        |machine| machine.seed,
    );
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn standalone_fifo_consumes_exited_requests_and_zero_budget_lifecycle() {
    let executable = guest::input("SEEDBOX_PV_WORKER");
    use super::test_image::{program, Op};
    let code = program(&[Op::Ready, Op::Spin]);
    let machine =
        super::PVMachine::from_boot_inputs(&executable, code, b"initrd".to_vec()).unwrap();
    crate::machine::test_environment::check_lifecycle_fifo(machine);
}
