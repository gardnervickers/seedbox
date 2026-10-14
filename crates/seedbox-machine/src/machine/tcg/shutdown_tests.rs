use super::*;
use crate::clock::Clock;
use crate::rng::{RngStreamExt, SeededRng};
use std::sync::Arc;
type TcgMachine = crate::machine::test_environment::Environment<super::TcgMachine>;
use std::sync::atomic::{AtomicU64, Ordering};

struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn start(words: &[u32], clock: Arc<TestClock>) -> TcgMachine {
    TcgMachine::new(
        Path::new("qemu-system-riscv64"),
        Manifest {
            boot: Boot::Program(words.iter().flat_map(|w| w.to_le_bytes()).collect()),
            memory_mib: 16,
        },
        SeededRng::new(0),
        clock,
    )
    .unwrap()
}

#[test]
fn shutdown_latching_and_zero_budget_delivery_execute_nothing() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(&[0x0015_0513, 0xffdf_f06f], clock.clone());
    machine.run(&mut (), at(8)).unwrap();
    clock.0.store(8, Ordering::Relaxed);
    let registers = machine.process().unwrap().registers().unwrap();
    let ram = machine
        .process()
        .unwrap()
        .memory(0x8000_0000, 4096)
        .unwrap();
    let stopped = machine.process().unwrap().stopped_result().unwrap();
    let rng = machine.rng.clone().next_u64();
    machine.request(crate::pv::Request::Shutdown).unwrap();
    machine.request(crate::pv::Request::Shutdown).unwrap();
    assert!(machine.shutdown_requested);
    assert_eq!(
        machine.process().unwrap().stopped_result().unwrap(),
        stopped
    );
    let pending = machine.snapshot().unwrap();
    assert_eq!(
        machine.run(&mut (), at(8)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.process().unwrap().registers().unwrap(), registers);
    assert_eq!(
        machine
            .process()
            .unwrap()
            .memory(0x8000_0000, 4096)
            .unwrap(),
        ram
    );
    assert_eq!(clock.now(), 8);
    assert_eq!(machine.rng.clone().next_u64(), rng);
    let delivered = machine.snapshot().unwrap();
    for snapshot in [&pending, &delivered] {
        let mut branch =
            TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), snapshot, clock.clone())
                .unwrap();
        assert!(branch.shutdown_requested);
        branch.run(&mut (), at(16)).unwrap();
        // This program has no lifecycle receiver. The request cannot pretend
        // that cleanup occurred, and ordinary execution remains possible.
        assert_eq!(branch.state, State::Ready);
        assert_eq!(branch.process().unwrap().registers().unwrap()[10], 8);
    }
}

#[test]
fn unhandled_request_keeps_idle_state_and_lifecycle_resets_discard_it() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(&[0x1050_0073], clock.clone()); // WFI, no enabled interrupt.
    assert_eq!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::Blocked {
            reached: at(1),
            deadline: None
        }
    );
    clock.0.store(10, Ordering::Relaxed);
    machine.request(crate::pv::Request::Shutdown).unwrap();
    assert_eq!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::Blocked {
            reached: at(10),
            deadline: None
        }
    );
    machine.request(crate::pv::Request::Crash).unwrap();
    machine.request(crate::pv::Request::Shutdown).unwrap(); // Already crashed is harmless.
    assert!(!machine.shutdown_requested);
    machine.request(crate::pv::Request::Reboot).unwrap();
    machine.request(crate::pv::Request::Shutdown).unwrap();
    machine.request(crate::pv::Request::Reboot).unwrap();
    assert!(!machine.shutdown_requested);
    machine.state = State::Invalid;
    assert!(machine.request(crate::pv::Request::Shutdown).is_err());
}

/// The board's test finisher is how firmware powers off or resets. Each guest
/// request stops at its instruction and is classified by its private cause;
/// the reset never restarts the machine in place.
#[test]
fn guest_finisher_requests_stop_with_their_cause() {
    // t0 = 0x100000 (finisher); t1 = request; sw t1, 0(t0); j .
    let program = |upper: u32, low: u32| {
        [
            0x0010_02b7,
            (upper << 12) | 0x337,
            (low << 20) | 0x0003_0313,
            0x0062_a023,
            0x0000_006f,
        ]
    };
    for (upper, low, reason) in [
        (0x5, 0x555, ExitReason::Shutdown), // FINISHER_PASS: power-off
        (0x7, 0x777, ExitReason::Crashed),  // FINISHER_RESET: reset request
        (0x3, 0x333, ExitReason::Crashed),  // FINISHER_FAIL: guest-reported panic
    ] {
        let clock = Arc::new(TestClock(AtomicU64::new(0)));
        let mut machine = start(&program(upper, low), clock.clone());
        let stop = machine.run(&mut (), at(1000)).unwrap();
        let StopReason::Exited {
            reached,
            reason: got,
        } = stop
        else {
            panic!("{low:#x}: unexpected {stop:?}");
        };
        assert_eq!(got, reason, "{low:#x}");
        assert!(reached <= at(4), "{low:#x} stops at its store, not later");
        // Nothing executes afterwards: the PC stays at the store's successor.
        assert_eq!(
            machine.process().unwrap().registers().unwrap()[32],
            0x8000_0010
        );
        clock.0.store(reached.as_nanos(), Ordering::Relaxed);
        let saved = machine.snapshot().unwrap();
        let mut restored =
            TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &saved, clock.clone())
                .unwrap();
        assert_eq!(restored.run(&mut (), reached).unwrap(), stop);
        assert_eq!(machine.run(&mut (), reached).unwrap(), stop);
    }
}
