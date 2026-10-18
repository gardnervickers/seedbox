use super::raw_program_data::{
    counting_loop, Asm, GED_RESET, GED_SLEEP, PROGRAM, RBX, RIP, STUB, UNTIMED,
};
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
const QEMU: &str = "qemu-system-x86_64";
/// The time at which program instruction `n` completes at factor 1000.
fn after(n: u64) -> u64 {
    STUB + n
}
/// The stack page, below the stub's initial ESP of 0x8000.
const STACK_PAGE: u64 = 0x7000;
fn start(image: Vec<u8>, clock: Arc<TestClock>) -> TcgMachine {
    TcgMachine::new(
        Path::new(QEMU),
        Manifest {
            boot: Boot::Program(image.into()),
            memory_mib: 16,
            disk: None,
            network: None,
        },
        SeededRng::new(0),
        clock,
    )
    .unwrap()
}

#[test]
fn shutdown_latching_and_zero_budget_delivery_execute_nothing() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(counting_loop(), clock.clone());
    machine.run(&mut (), at(after(8))).unwrap();
    clock.0.store(after(8), Ordering::Relaxed);
    let registers = machine.process().unwrap().registers().unwrap();
    let ram = machine.process().unwrap().memory(STACK_PAGE, 4096).unwrap();
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
        machine.run(&mut (), at(after(8))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.process().unwrap().registers().unwrap(), registers);
    assert_eq!(
        machine.process().unwrap().memory(STACK_PAGE, 4096).unwrap(),
        ram
    );
    assert_eq!(clock.now(), after(8));
    assert_eq!(machine.rng.clone().next_u64(), rng);
    let delivered = machine.snapshot().unwrap();
    for snapshot in [&pending, &delivered] {
        let mut branch =
            TcgMachine::from_snapshot(Path::new(QEMU), snapshot, clock.clone()).unwrap();
        assert!(branch.shutdown_requested);
        branch.run(&mut (), at(after(16))).unwrap();
        // This program has no lifecycle receiver. The request cannot pretend
        // that cleanup occurred, and ordinary execution remains possible.
        assert_eq!(branch.state, State::Ready);
        assert_eq!(branch.process().unwrap().registers().unwrap()[RBX], 8);
    }
}

#[test]
fn unhandled_request_keeps_idle_state_and_lifecycle_resets_discard_it() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Stop the board timers, then HLT with interrupts disabled.
    let mut machine = start(Asm::default().untimed().hlt().image(), clock.clone());
    let idle = after(UNTIMED + 1);
    assert_eq!(
        machine.run(&mut (), at(idle + 9)).unwrap(),
        StopReason::Blocked {
            reached: at(idle),
            deadline: None
        }
    );
    clock.0.store(idle + 9, Ordering::Relaxed);
    machine.request(crate::pv::Request::Shutdown).unwrap();
    assert_eq!(
        machine.run(&mut (), at(idle + 9)).unwrap(),
        StopReason::Blocked {
            reached: at(idle + 9),
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

/// ACPI is how firmware powers off or resets the microvm board, and a triple
/// fault resets any x86 machine. Each guest request stops at its instruction
/// and is classified by its private cause; the reset never restarts the
/// machine in place.
#[test]
fn guest_power_off_and_reset_requests_stop_with_their_cause() {
    let request = |code: &mut Asm| code.spin().image();
    for (name, image, reason, rip) in [
        (
            "GED sleep S5: power-off",
            request(Asm::default().store_byte(GED_SLEEP, 0x34)),
            ExitReason::Shutdown,
            PROGRAM + 7,
        ),
        (
            "GED reset register",
            request(Asm::default().store_byte(GED_RESET, 0x42)),
            ExitReason::Crashed,
            PROGRAM + 7,
        ),
        // ud2 through an IDT without its gate: #UD, #NP, #DF, then reset.
        (
            "triple fault",
            request(Asm::default().ud2()),
            ExitReason::Crashed,
            PROGRAM,
        ),
    ] {
        let clock = Arc::new(TestClock(AtomicU64::new(0)));
        let mut machine = start(image, clock.clone());
        let stop = machine.run(&mut (), at(1000)).unwrap();
        let StopReason::Exited {
            reached,
            reason: got,
        } = stop
        else {
            panic!("{name}: unexpected {stop:?}");
        };
        assert_eq!(got, reason, "{name}");
        assert!(
            reached <= at(after(1)),
            "{name} stops at its request, not later"
        );
        // Nothing executes afterwards: EIP stays after the store, or at the
        // faulting instruction.
        assert_eq!(
            machine.process().unwrap().registers().unwrap()[RIP],
            rip,
            "{name}"
        );
        clock.0.store(reached.as_nanos(), Ordering::Relaxed);
        let saved = machine.snapshot().unwrap();
        let mut restored =
            TcgMachine::from_snapshot(Path::new(QEMU), &saved, clock.clone()).unwrap();
        assert_eq!(restored.run(&mut (), reached).unwrap(), stop);
        assert_eq!(machine.run(&mut (), reached).unwrap(), stop);
    }
}
