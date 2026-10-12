use super::*;
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
        clock,
    )
    .unwrap()
}
fn registers(machine: &mut TcgMachine) -> [u64; 33] {
    machine.process().unwrap().registers().unwrap()
}

#[test]
fn runnable_gap_and_restored_gap_do_not_execute_instructions_or_mutate_ram() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Increment/store a counter at RAM+0x1000, then loop.
    let mut machine = start(
        &[0x0000_1297, 0x0015_0513, 0x00a2_b023, 0xff9f_f06f],
        clock.clone(),
    );
    machine.run(&mut (), at(10)).unwrap();
    let before = registers(&mut machine);
    let ram = machine.process().unwrap().memory(0x8000_1000, 8).unwrap();
    let snapshot = machine.snapshot().unwrap();
    clock.0.store(30, Ordering::Relaxed);
    assert!(machine.run(&mut (), at(29)).is_err());
    assert_eq!(
        machine.run(&mut (), at(30)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine), before);
    assert_eq!(
        machine.process().unwrap().memory(0x8000_1000, 8).unwrap(),
        ram
    );
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock.clone())
            .unwrap();
    assert_eq!(
        branch.run(&mut (), at(30)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut branch), before);
    machine.run(&mut (), at(40)).unwrap();
    branch.run(&mut (), at(40)).unwrap();
    assert_eq!(registers(&mut branch), registers(&mut machine));
    assert_eq!(machine.reached, at(40));
    assert_eq!(clock.now(), 30);
    // Compare ten actual post-gap instructions with ten contiguous instructions.
    let old_clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut continuous =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, old_clock).unwrap();
    continuous.run(&mut (), at(20)).unwrap();
    assert_eq!(registers(&mut continuous), registers(&mut machine));
}

#[test]
fn overdue_timer_is_delivered_at_adopted_time_without_executing_cpu() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(
        &[
            0x0200_42b7, // CLINT mtimecmp at 0x02004000.
            0x00a0_0313, // Timer compare = 10 ticks (10 MHz).
            0x0062_b023, // Store schedules its deadline at 1003 ns.
            0x0800_0313, // MTIE bit; global MIE stays clear.
            0x3043_2073, // Enable timer wakeup from WFI without a trap.
            0x1050_0073, // WFI.
            0x0015_0513, // Increment once after timer wake.
            0x0000_006f, // Spin.
        ],
        clock.clone(),
    );
    assert_eq!(
        machine.run(&mut (), at(500)).unwrap(),
        StopReason::Blocked {
            reached: at(6),
            deadline: Some(at(1003))
        }
    );
    let before = registers(&mut machine);
    let snapshot = machine.snapshot().unwrap();
    clock.0.store(500, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(500)).unwrap(),
        StopReason::Blocked {
            reached: at(500),
            deadline: Some(at(1003)),
        }
    );
    assert_eq!(registers(&mut machine), before);
    clock.0.store(2000, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(2000)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine), before);
    let adopted = machine.snapshot().unwrap();
    let mut restored =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock.clone())
            .unwrap();
    assert_eq!(
        restored.run(&mut (), at(2000)).unwrap(),
        StopReason::BoundaryReached
    );
    machine.run(&mut (), at(2001)).unwrap();
    restored.run(&mut (), at(2001)).unwrap();
    assert_eq!(registers(&mut machine)[10], 1);
    assert_eq!(registers(&mut restored), registers(&mut machine));
    // A snapshot after zero-budget delivery retains the pending interrupt.
    let mut pending =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &adopted, clock).unwrap();
    pending.run(&mut (), at(2001)).unwrap();
    assert_eq!(registers(&mut pending), registers(&mut machine));
}

#[test]
fn zero_budget_elapsed_time_preserves_indefinite_blocking() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(&[0x1050_0073, 0x0015_0513], clock.clone());
    assert_eq!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::Blocked {
            reached: at(1),
            deadline: None
        }
    );
    let before = registers(&mut machine);
    clock.0.store(30, Ordering::Relaxed);
    let expected = StopReason::Blocked {
        reached: at(30),
        deadline: None,
    };
    assert_eq!(machine.run(&mut (), at(30)).unwrap(), expected);
    assert_eq!(machine.run(&mut (), at(40)).unwrap(), expected);
    assert_eq!(registers(&mut machine), before);
}

#[test]
fn repeated_capture_preserves_stopped_reason_timer_and_next_execution() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(
        &[
            0x0200_42b7,
            0x00a0_0313,
            0x0062_b023, // CLINT deadline at 1003 ns.
            0x0800_0313,
            0x3043_2073,
            0x1050_0073, // Enable timer; WFI.
            0x0015_0513,
            0x0000_006f, // Increment after wake; spin.
        ],
        clock.clone(),
    );
    let blocked = StopReason::Blocked {
        reached: at(6),
        deadline: Some(at(1003)),
    };
    assert_eq!(machine.run(&mut (), at(500)).unwrap(), blocked);
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
    assert_eq!(machine.run(&mut (), at(500)).unwrap(), blocked);
    clock.0.store(1003, Ordering::Relaxed);
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &first, clock).unwrap();
    machine.run(&mut (), at(1004)).unwrap();
    branch.run(&mut (), at(1004)).unwrap();
    assert_eq!(registers(&mut machine)[10], 1);
    assert_eq!(registers(&mut machine), registers(&mut branch));
}

#[test]
fn driver_resumes_a_runnable_guest_without_replaying_paused_instructions() {
    use crate::driver::{Action, Driver, StepOutcome};
    let mut driver = Driver::new(
        at(0),
        std::time::Duration::from_nanos(10_000),
        2,
        || Ok(()),
        |clock| {
            (0..2)
                .map(|_| {
                    TcgMachine::new(
                        Path::new("qemu-system-riscv64"),
                        Manifest {
                            boot: Boot::Program(
                                [0x0015_0513u32, 0xffdf_f06f]
                                    .iter()
                                    .flat_map(|w| w.to_le_bytes())
                                    .collect(),
                            ),
                            memory_mib: 16,
                        },
                        clock.clone(),
                    )
                })
                .collect()
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| {
            let id = barrier.machines().next().unwrap().0;
            barrier.schedule(at(3), Action::Pause(id))?;
            barrier.schedule(at(20), Action::Resume(id))?;
            Ok(id)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(3)).unwrap(),
        StepOutcome::BarrierReached { at: at(3) }
    );
    assert!(driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
    let paused = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
    assert_eq!(
        driver.step(at(20)).unwrap(),
        StepOutcome::BarrierReached { at: at(20) }
    );
    assert!(!driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
    let resumed = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
    assert_eq!(resumed.reached, at(20));
    let clock = Arc::new(TestClock(AtomicU64::new(20)));
    let mut paused_copy =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &paused, clock.clone())
            .unwrap();
    let mut resumed_copy =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &resumed, clock).unwrap();
    assert_eq!(registers(&mut paused_copy), registers(&mut resumed_copy));
    assert_eq!(registers(&mut resumed_copy)[10], 2);
    assert_eq!(
        driver.step(at(24)).unwrap(),
        StepOutcome::BarrierReached { at: at(24) }
    );
    let after = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
    let mut copy = TcgMachine::from_snapshot(
        Path::new("qemu-system-riscv64"),
        &after,
        Arc::new(TestClock(AtomicU64::new(24))),
    )
    .unwrap();
    assert_eq!(registers(&mut copy)[10], 4);
}

#[test]
fn driver_resume_delivers_overdue_timer_before_post_resume_instructions() {
    use crate::driver::{Action, Driver, StepOutcome};
    // First machine sleeps on CLINT's 1003 ns deadline, then increments a0.
    let programs = [
        vec![
            0x0200_42b7u32,
            0x00a0_0313,
            0x0062_b023,
            0x0800_0313,
            0x3043_2073,
            0x1050_0073,
            0x0015_0513,
            0x0000_006f,
        ],
        vec![0x0000_006f], // Second machine remains runnable during the pause.
    ];
    let mut driver = Driver::new(
        at(0),
        std::time::Duration::from_nanos(10_000),
        2,
        || Ok(()),
        |clock| {
            programs
                .iter()
                .map(|words| {
                    TcgMachine::new(
                        Path::new("qemu-system-riscv64"),
                        Manifest {
                            boot: Boot::Program(
                                words.iter().flat_map(|w| w.to_le_bytes()).collect(),
                            ),
                            memory_mib: 16,
                        },
                        clock.clone(),
                    )
                })
                .collect()
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| {
            let id = barrier.machines().next().unwrap().0;
            barrier.schedule(at(7), Action::Pause(id))?;
            barrier.schedule(at(2000), Action::Resume(id))?;
            Ok(id)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(7)).unwrap(),
        StepOutcome::BarrierReached { at: at(7) }
    );
    assert!(driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
    assert_eq!(
        driver.step(at(2000)).unwrap(),
        StepOutcome::BarrierReached { at: at(2000) }
    );
    let resumed = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
    assert_eq!(resumed.reached, at(2000));
    assert_eq!(resumed.state, State::Ready);
    let clock = Arc::new(TestClock(AtomicU64::new(2000)));
    let mut copy =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &resumed, clock.clone())
            .unwrap();
    assert_eq!(registers(&mut copy)[10], 0);
    assert_eq!(
        driver.step(at(2001)).unwrap(),
        StepOutcome::BarrierReached { at: at(2001) }
    );
    let after = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
    let mut copy =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &after, clock).unwrap();
    assert_eq!(registers(&mut copy)[10], 1);
}
