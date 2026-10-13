use super::*;
use crate::clock::Clock;
use crate::rng::{RngStreamExt, SeededRng};
use std::sync::Arc;
type TcgMachine = crate::machine::test_environment::Environment<super::TcgMachine>;
use std::sync::atomic::{AtomicU64, Ordering};

fn seed_after_boots(boots: usize) -> u64 {
    let mut rng = SeededRng::new(0);
    (0..boots).map(|_| rng.next_u64()).last().unwrap()
}

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
fn reboot_resets_live_ram_without_executing_and_preserves_snapshot_entropy() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Store an incrementing counter at RAM+0x1000, then loop.
    let mut machine = start(
        &[0x0000_1297, 0x0015_0513, 0x00a2_b023, 0xff9f_f06f],
        clock.clone(),
    );
    machine.run(&mut (), at(8)).unwrap();
    let snapshot = machine.snapshot().unwrap();
    let saved_rng = machine.rng.clone();
    clock.0.store(8, Ordering::Relaxed);
    machine.request(crate::pv::Request::Reboot).unwrap();
    assert_eq!(machine.reached, at(8));
    assert_eq!(machine.pending_boots, 0);
    machine.run(&mut (), at(8)).unwrap();
    assert_eq!(clock.now(), 8);
    assert_eq!(machine.reached, at(8));
    assert_eq!(machine.state, State::Ready);
    assert_eq!(machine.process().unwrap().registers().unwrap()[10], 0);
    assert_eq!(
        machine.process().unwrap().memory(0x8000_1000, 8).unwrap(),
        [0; 8]
    );
    let first_seed = machine.boot_seed;
    assert_eq!(first_seed, seed_after_boots(2));
    assert_eq!(
        machine.run(&mut (), at(8)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.process().unwrap().registers().unwrap()[10], 0);
    machine.run(&mut (), at(16)).unwrap();
    assert_eq!(machine.process().unwrap().registers().unwrap()[10], 3);

    let branch_clock = Arc::new(TestClock(AtomicU64::new(8)));
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, branch_clock)
            .unwrap();
    branch.rng = saved_rng;
    branch.request(crate::pv::Request::Reboot).unwrap();
    branch.run(&mut (), at(8)).unwrap();
    assert_eq!(branch.boot_seed, first_seed);
    branch.request(crate::pv::Request::Reboot).unwrap();
    branch.run(&mut (), at(8)).unwrap();
    assert_ne!(branch.boot_seed, first_seed);
    assert_eq!(branch.boot_seed, seed_after_boots(3));
}

#[test]
fn reboot_accepts_exit_and_crash_but_rejects_invalid_or_wrong_barrier() {
    // RISC-V virt finisher: write FINISHER_PASS (0x5555) to 0x100000.
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(
        &[0x0010_02b7, 0x0000_5337, 0x5553_0313, 0x0062_a023],
        clock.clone(),
    );
    assert!(matches!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    clock.0.store(100, Ordering::Relaxed);
    machine.request(crate::pv::Request::Reboot).unwrap();
    machine.request(crate::pv::Request::Crash).unwrap();
    let snapshot = machine.snapshot().unwrap();
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock.clone())
            .unwrap();
    branch.request(crate::pv::Request::Reboot).unwrap();
    assert!(branch.run(&mut (), at(99)).is_err());
    assert_eq!(
        branch.run(&mut (), at(100)).unwrap(),
        StopReason::BoundaryReached
    );
    branch.state = State::Invalid;
    assert!(branch.request(crate::pv::Request::Reboot).is_err());
    machine.request(crate::pv::Request::Reboot).unwrap();
    assert!(matches!(
        machine.run(&mut (), at(200)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
}

#[test]
fn queued_boots_use_caller_entropy_and_machine_only_snapshots() {
    use crate::machine::test_environment::run_requests;
    use crate::pv::Request;
    #[derive(Clone)]
    struct Bytes(u8);
    impl crate::rng::RngStream for Bytes {
        fn fill_bytes(&mut self, bytes: &mut [u8]) {
            for byte in bytes {
                *byte = self.0;
                self.0 += 1;
            }
        }
    }
    let executable = Path::new("qemu-system-riscv64");
    let mut machine = super::TcgMachine::new_raw(
        executable,
        Manifest {
            boot: Boot::Program(
                [0x0015_0513u32, 0xffdf_f06f]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect(),
            ),
            memory_mib: 16,
        },
    )
    .unwrap();
    let clock = crate::clock::SimulationClock::new(0);
    let mut rng = Bytes(0);
    machine.snapshot().unwrap();
    assert_eq!(rng.0, 0, "snapshot must not draw caller entropy");
    run_requests(&mut machine, &clock, &mut rng, [], at(0)).unwrap();
    assert_eq!(rng.0, 8);
    let saved = machine.snapshot().unwrap();
    let mut branch_rng = rng.clone();
    run_requests(
        &mut machine,
        &clock,
        &mut rng,
        [Request::Reboot, Request::Reboot, Request::Shutdown],
        at(0),
    )
    .unwrap();
    assert_eq!(
        rng.0, 24,
        "each ordered reboot draws one seed, without instructions"
    );
    assert_eq!(
        machine.boot_seed,
        u64::from_le_bytes([16, 17, 18, 19, 20, 21, 22, 23])
    );
    let mut branch = super::TcgMachine::from_snapshot(executable, &saved).unwrap();
    run_requests(
        &mut branch,
        &clock,
        &mut branch_rng,
        [Request::Reboot, Request::Reboot, Request::Shutdown],
        at(0),
    )
    .unwrap();
    assert_eq!(branch.boot_seed, machine.boot_seed);
    assert_eq!(branch_rng.0, rng.0);
    let before_crash = branch_rng.0;
    assert!(matches!(
        run_requests(
            &mut branch,
            &clock,
            &mut branch_rng,
            [Request::Crash],
            at(0)
        )
        .unwrap(),
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    assert_eq!(
        branch_rng.0, before_crash,
        "crash accepts at zero budget without a seed draw"
    );
    branch.snapshot().unwrap();
    run_requests(
        &mut branch,
        &clock,
        &mut branch_rng,
        [Request::Reboot],
        at(0),
    )
    .unwrap();
    assert_eq!(branch_rng.0, 32, "reboot revives an exited machine");
}

#[test]
fn driver_reboots_live_and_crashed_guest_with_fresh_state_and_retained_actions() {
    use crate::driver::{Action, Driver, StepOutcome};
    use std::time::Duration;

    let words = [0x0015_0513u32, 0xffdf_f06f]; // Increment a0; loop.
    let mut driver = crate::driver::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            TcgMachine::configured(
                Path::new("qemu-system-riscv64"),
                Manifest {
                    boot: Boot::Program(words.iter().flat_map(|word| word.to_le_bytes()).collect()),
                    memory_mib: 16,
                },
                SeededRng::new(0),
                clock,
            )
            .map(|guest| vec![guest])
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap();
    assert_eq!(
        driver.step(at(5)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    let before = driver
        .with_barrier(|barrier| {
            let saved = barrier.snapshot(id)?;
            barrier.schedule(at(5), Action::Pause(id))?;
            barrier.schedule(at(5), Action::Reboot(id))?;
            Ok(saved)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(5)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    let after = driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machines().next().unwrap().0, id);
            assert!(!barrier.paused(id)?);
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            assert_eq!(barrier.machine(id)?.pending_boots, 0);
            barrier.snapshot(id)
        })
        .unwrap();
    let mut old = TcgMachine::from_snapshot(
        Path::new("qemu-system-riscv64"),
        &before,
        Arc::new(TestClock(AtomicU64::new(5))),
    )
    .unwrap();
    let mut fresh = TcgMachine::from_snapshot(
        Path::new("qemu-system-riscv64"),
        &after,
        Arc::new(TestClock(AtomicU64::new(5))),
    )
    .unwrap();
    assert!(old.process().unwrap().registers().unwrap()[10] > 0);
    fresh.run(&mut (), at(5)).unwrap();
    assert_eq!(fresh.process().unwrap().registers().unwrap()[10], 0);

    driver
        .with_barrier(|barrier| {
            for (time, action) in [
                (5, Action::Crash(id)),
                (8, Action::Reboot(id)),
                (8, Action::Pause(id)),
                (8, Action::Wake(id)),
                (9, Action::Resume(id)),
                (12, Action::Crash(id)),
            ] {
                barrier.schedule(at(time), action)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(8) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machines().next().unwrap().0, id);
            assert!(barrier.paused(id)?);
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            assert_eq!(barrier.machine(id)?.pending_boots, 0);
            assert_eq!(barrier.machine(id)?.pending_at, None);
            assert_eq!(barrier.machine(id)?.state, State::Ready);
            Ok(())
        })
        .unwrap();
    for end in [9, 10] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(12),
            reason: ExitReason::Shutdown
        }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(id)?,
                StopReason::Exited {
                    reached: at(12),
                    reason: ExitReason::Crashed
                }
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn standalone_fifo_consumes_exited_requests_and_zero_budget_lifecycle() {
    let machine = super::TcgMachine::new_raw(
        Path::new("qemu-system-riscv64"),
        Manifest {
            boot: Boot::Program(
                [0x0015_0513u32, 0xffdf_f06f]
                    .into_iter()
                    .flat_map(u32::to_le_bytes)
                    .collect(),
            ),
            memory_mib: 16,
        },
    )
    .unwrap();
    crate::machine::test_environment::check_lifecycle_fifo(machine);
}
