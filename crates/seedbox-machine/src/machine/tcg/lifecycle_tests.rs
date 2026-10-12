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
    clock.0.store(8, Ordering::Relaxed);
    machine.reboot(at(8)).unwrap();
    assert_eq!(clock.now(), 8);
    assert_eq!(machine.reached, at(8));
    assert_eq!(machine.state, State::Ready);
    assert_eq!(machine.process().unwrap().registers().unwrap()[10], 0);
    assert_eq!(
        machine.process().unwrap().memory(0x8000_1000, 8).unwrap(),
        [0; 8]
    );
    let first_seed = machine.boot_seed;
    assert_eq!(machine.boot_rng.counter(), 1);
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
    branch.reboot(at(8)).unwrap();
    assert_eq!(branch.boot_seed, first_seed);
    branch.reboot(at(8)).unwrap();
    assert_ne!(branch.boot_seed, first_seed);
    assert_eq!(branch.boot_rng.counter(), 2);
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
    machine.reboot(at(100)).unwrap();
    machine.crash().unwrap();
    let snapshot = machine.snapshot().unwrap();
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock.clone())
            .unwrap();
    branch.reboot(at(100)).unwrap();
    assert!(branch.reboot(at(99)).is_err());
    clock.0.store(99, Ordering::Relaxed);
    assert!(branch.reboot(at(99)).is_err());
    clock.0.store(100, Ordering::Relaxed);
    assert_eq!(
        branch.run(&mut (), at(100)).unwrap(),
        StopReason::BoundaryReached
    );
    branch.state = State::Invalid;
    assert!(branch.reboot(at(100)).is_err());
    machine.reboot(at(100)).unwrap();
    assert!(matches!(
        machine.run(&mut (), at(200)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
}
