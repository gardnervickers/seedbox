use super::raw_program_data::{storing_loop, Asm, Reg, RBX, SCRATCH, STUB, UNTIMED};
use super::*;
use crate::clock::Clock;
use crate::rng::SeededRng;
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
/// Arm the LAPIC deadline at program instruction 1003, halt with interrupts
/// enabled, and after the EOI handler (two instructions) increment EBX once.
fn timer_then_increment() -> Vec<u8> {
    Asm::default()
        .lapic_timer(998)
        .sti()
        .hlt()
        .inc(Reg::Ebx)
        .spin()
        .image()
}
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
fn registers(machine: &mut TcgMachine) -> [u64; 17] {
    machine.process().unwrap().registers().unwrap()
}

#[test]
fn runnable_gap_and_restored_gap_do_not_execute_instructions_or_mutate_ram() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Increment/store a counter at the RAM scratch word, then loop.
    let mut machine = start(storing_loop(), clock.clone());
    machine.run(&mut (), at(after(10))).unwrap();
    let before = registers(&mut machine);
    let ram = machine
        .process()
        .unwrap()
        .memory(SCRATCH.into(), 8)
        .unwrap();
    let snapshot = machine.snapshot().unwrap();
    clock.0.store(after(30), Ordering::Relaxed);
    assert!(machine.run(&mut (), at(after(29))).is_err());
    assert_eq!(
        machine.run(&mut (), at(after(30))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine), before);
    assert_eq!(
        machine
            .process()
            .unwrap()
            .memory(SCRATCH.into(), 8)
            .unwrap(),
        ram
    );
    let mut branch = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock.clone()).unwrap();
    assert_eq!(
        branch.run(&mut (), at(after(30))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut branch), before);
    machine.run(&mut (), at(after(40))).unwrap();
    branch.run(&mut (), at(after(40))).unwrap();
    assert_eq!(registers(&mut branch), registers(&mut machine));
    assert_eq!(machine.reached, at(after(40)));
    assert_eq!(clock.now(), after(30));
    // Compare ten actual post-gap instructions with ten contiguous instructions.
    let old_clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut continuous = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, old_clock).unwrap();
    continuous.run(&mut (), at(after(20))).unwrap();
    assert_eq!(registers(&mut continuous), registers(&mut machine));
}

#[test]
fn overdue_timer_is_delivered_at_adopted_time_without_executing_cpu() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(timer_then_increment(), clock.clone());
    assert_eq!(
        machine.run(&mut (), at(after(500))).unwrap(),
        StopReason::Blocked {
            reached: at(after(6)),
            deadline: Some(at(after(1003)))
        }
    );
    let before = registers(&mut machine);
    let snapshot = machine.snapshot().unwrap();
    clock.0.store(after(500), Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(after(500))).unwrap(),
        StopReason::Blocked {
            reached: at(after(500)),
            deadline: Some(at(after(1003))),
        }
    );
    assert_eq!(registers(&mut machine), before);
    clock.0.store(after(2000), Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(after(2000))).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine), before);
    let adopted = machine.snapshot().unwrap();
    let mut restored =
        TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock.clone()).unwrap();
    assert_eq!(
        restored.run(&mut (), at(after(2000))).unwrap(),
        StopReason::BoundaryReached
    );
    // The EOI handler's two instructions precede the increment.
    let woke = after(2000) + 3;
    machine.run(&mut (), at(woke)).unwrap();
    restored.run(&mut (), at(woke)).unwrap();
    assert_eq!(registers(&mut machine)[RBX], 1);
    assert_eq!(registers(&mut restored), registers(&mut machine));
    // A snapshot after zero-budget delivery retains the pending interrupt.
    let mut pending = TcgMachine::from_snapshot(Path::new(QEMU), &adopted, clock).unwrap();
    pending.run(&mut (), at(woke)).unwrap();
    assert_eq!(registers(&mut pending), registers(&mut machine));
}

#[test]
fn zero_budget_elapsed_time_preserves_indefinite_blocking() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Stop the board timers, halt with interrupts disabled, then increment.
    let image = Asm::default().untimed().hlt().inc(Reg::Ebx).image();
    let mut machine = start(image, clock.clone());
    let idle = after(UNTIMED + 1);
    assert_eq!(
        machine.run(&mut (), at(idle + 9)).unwrap(),
        StopReason::Blocked {
            reached: at(idle),
            deadline: None
        }
    );
    let before = registers(&mut machine);
    clock.0.store(idle + 29, Ordering::Relaxed);
    let expected = StopReason::Blocked {
        reached: at(idle + 29),
        deadline: None,
    };
    assert_eq!(machine.run(&mut (), at(idle + 29)).unwrap(), expected);
    assert_eq!(machine.run(&mut (), at(idle + 39)).unwrap(), expected);
    assert_eq!(registers(&mut machine), before);
}

#[test]
fn repeated_capture_preserves_stopped_reason_timer_and_next_execution() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // LAPIC deadline at program instruction 1003; increment after wake.
    let mut machine = start(timer_then_increment(), clock.clone());
    let blocked = StopReason::Blocked {
        reached: at(after(6)),
        deadline: Some(at(after(1003))),
    };
    assert_eq!(machine.run(&mut (), at(after(500))).unwrap(), blocked);
    let registers_before = registers(&mut machine);
    let stop_before = machine.process().unwrap().stopped_result().unwrap();
    let first = machine.snapshot().unwrap();
    let _second = machine.snapshot().unwrap();
    let _third = machine.snapshot().unwrap();
    assert_eq!(
        machine.process().unwrap().stopped_result().unwrap(),
        stop_before
    );
    assert_eq!(registers(&mut machine), registers_before);
    assert_eq!(machine.run(&mut (), at(after(500))).unwrap(), blocked);
    clock.0.store(after(1003), Ordering::Relaxed);
    let mut branch = TcgMachine::from_snapshot(Path::new(QEMU), &first, clock).unwrap();
    // The EOI handler's two instructions precede the increment.
    machine.run(&mut (), at(after(1003) + 3)).unwrap();
    branch.run(&mut (), at(after(1003) + 3)).unwrap();
    assert_eq!(registers(&mut machine)[RBX], 1);
    assert_eq!(registers(&mut machine), registers(&mut branch));
}

#[test]
fn halted_timer_run_replays_the_same_native_interaction_witness() {
    let initial = "00".repeat(32);
    let mut witnesses = Vec::new();
    for _ in 0..2 {
        let clock = Arc::new(TestClock(AtomicU64::new(0)));
        let mut machine = start(timer_then_increment(), clock.clone());
        machine
            .process()
            .unwrap()
            .start_native_witness(&initial, 1, false)
            .unwrap();
        machine
            .process()
            .unwrap()
            .input(b"same-replay-input\n")
            .unwrap();
        assert!(matches!(
            machine.run(&mut (), at(after(100))).unwrap(),
            StopReason::Blocked {
                deadline: Some(_),
                ..
            }
        ));
        // Same adopted time and instruction budget on the fresh replay. This
        // services the expired LAPIC timer before executing its IRQ handler.
        clock.0.store(after(2000), Ordering::Relaxed);
        assert_eq!(
            machine.run(&mut (), at(after(2006))).unwrap(),
            StopReason::BoundaryReached
        );
        let witness = machine.process().unwrap().native_witness(false).unwrap();
        assert_ne!(
            witness.0, initial,
            "real LAPIC interactions must be witnessed"
        );
        witnesses.push(witness);
    }
    assert_eq!(
        witnesses[0], witnesses[1],
        "same-input native witness replay"
    );
}
