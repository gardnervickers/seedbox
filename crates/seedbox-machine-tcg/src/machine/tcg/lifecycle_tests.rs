use super::raw_program_data::{bios, counting_loop, storing_loop, RBX, SCRATCH, SHUTDOWN, STUB};
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
const QEMU: &str = "qemu-system-x86_64";
/// The time at which program instruction `n` completes at factor 1000.
fn after(n: u64) -> u64 {
    STUB + n
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

#[test]
fn reboot_resets_live_ram_without_executing_and_preserves_snapshot_entropy() {
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    // Store an incrementing counter at the RAM scratch word, then loop.
    let mut machine = start(storing_loop(), clock.clone());
    let rebooted = after(8);
    machine.run(&mut (), at(rebooted)).unwrap();
    let snapshot = machine.snapshot().unwrap();
    let saved_rng = machine.rng.clone();
    clock.0.store(rebooted, Ordering::Relaxed);
    machine
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
    assert_eq!(machine.reached, at(rebooted));
    assert_eq!(machine.pending_boots, 0);
    machine.run(&mut (), at(rebooted)).unwrap();
    assert_eq!(clock.now(), rebooted);
    assert_eq!(machine.reached, at(rebooted));
    assert_eq!(machine.state, State::Ready);
    assert_eq!(machine.process().unwrap().registers().unwrap()[RBX], 0);
    assert_eq!(
        machine
            .process()
            .unwrap()
            .memory(SCRATCH.into(), 8)
            .unwrap(),
        [0; 8]
    );
    let first_seed = machine.boot_seed;
    assert_eq!(first_seed, seed_after_boots(2));
    assert_eq!(
        machine.run(&mut (), at(rebooted)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.process().unwrap().registers().unwrap()[RBX], 0);
    // The new boot runs the stub again before eight program instructions.
    machine.run(&mut (), at(rebooted + after(8))).unwrap();
    assert_eq!(machine.process().unwrap().registers().unwrap()[RBX], 3);

    let branch_clock = Arc::new(TestClock(AtomicU64::new(rebooted)));
    let mut branch = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, branch_clock).unwrap();
    branch.rng = saved_rng;
    branch
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
    branch.run(&mut (), at(rebooted)).unwrap();
    assert_eq!(branch.boot_seed, first_seed);
    branch
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
    branch.run(&mut (), at(rebooted)).unwrap();
    assert_ne!(branch.boot_seed, first_seed);
    assert_eq!(branch.boot_seed, seed_after_boots(3));
}

#[test]
fn reboot_accepts_exit_and_crash_but_rejects_invalid_or_wrong_barrier() {
    // ACPI GED sleep control: power off with S5.
    let clock = Arc::new(TestClock(AtomicU64::new(0)));
    let mut machine = start(bios(&SHUTDOWN), clock.clone());
    assert!(matches!(
        machine.run(&mut (), at(100)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    clock.0.store(100, Ordering::Relaxed);
    machine
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
    machine
        .request(seedbox_machine::pv::Request::Crash)
        .unwrap();
    let snapshot = machine.snapshot().unwrap();
    let mut branch = TcgMachine::from_snapshot(Path::new(QEMU), &snapshot, clock.clone()).unwrap();
    branch
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
    assert!(branch.run(&mut (), at(99)).is_err());
    assert_eq!(
        branch.run(&mut (), at(100)).unwrap(),
        StopReason::BoundaryReached
    );
    branch.state = State::Invalid;
    assert!(branch
        .request(seedbox_machine::pv::Request::Reboot)
        .is_err());
    machine
        .request(seedbox_machine::pv::Request::Reboot)
        .unwrap();
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
    let executable = Path::new(QEMU);
    let machine = super::TcgMachine::new_raw(
        executable,
        Manifest {
            boot: Boot::Program(counting_loop().into()),
            memory_mib: 16,
            disk: None,
            network: None,
        },
    )
    .unwrap();
    crate::machine::test_environment::check_queued_boot_entropy(
        machine,
        |saved| super::TcgMachine::from_snapshot(executable, saved).unwrap(),
        |machine| machine.boot_seed,
    );
}

#[test]
fn standalone_fifo_consumes_exited_requests_and_zero_budget_lifecycle() {
    let machine = super::TcgMachine::new_raw(
        Path::new(QEMU),
        Manifest {
            boot: Boot::Program(counting_loop().into()),
            memory_mib: 16,
            disk: None,
            network: None,
        },
    )
    .unwrap();
    crate::machine::test_environment::check_lifecycle_fifo(machine);
}
