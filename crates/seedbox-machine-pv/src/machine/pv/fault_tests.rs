//! Device faults across the client's boot boundaries, with raw images.
use super::*;
use seedbox_machine::{
    machine::{ExitReason, Machine, StopReason, Timestamp},
    pv::{DeviceFault, DiskOp, FaultCounts, Request},
    rng::SeededRng,
};
use std::sync::Arc;
type Environment = seedbox_machine::test_support::test_environment::Environment<super::PVMachine>;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn faults_and_holds_outlive_a_guest_power_off_and_heal_with_their_counts() {
    let executable =
        PathBuf::from(std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER"));
    let clock = Arc::new(seedbox_machine::clock::SimulationClock::new(0));
    let kernel = super::test_image::program(&[
        super::test_image::Op::Ready,
        super::test_image::Op::PowerOff,
    ]);
    let raw = super::PVMachine::construct(
        &executable,
        kernel,
        b"initrd".to_vec(),
        TEST_COMMAND_LINE.into(),
        Some(DiskImage::zeroed(1 << 20).unwrap()),
        None,
    )
    .unwrap();
    let mut machine = Environment::configured(raw, SeededRng::new(7), clock.clone());
    let latency = DeviceFault::DiskLatency {
        op: DiskOp::Any,
        latency_ns: 1_000,
        jitter_ns: 0,
    };
    machine
        .request(Request::InstallFault {
            id: 3,
            seed: 11,
            fault: latency,
        })
        .unwrap();
    machine
        .request(Request::HoldBlockRequest {
            trigger: 1,
            op: DiskOp::Flush,
            nth: 2,
            until: None,
        })
        .unwrap();
    // The guest powers off without touching its disk; retiring the ended
    // boot reads its counts from the stopped worker.
    assert!(matches!(
        machine.run(&mut (), at(10)).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    let next = machine.aux.faults.boot(1);
    assert_eq!(next.installed.len(), 1);
    assert_eq!((next.holds[0].trigger, next.holds[0].seen), (1, 0));
    // Without a worker the snapshot carries the mirror.
    let snapshot = machine.snapshot().unwrap();
    machine.request(Request::HealFault { id: 3 }).unwrap();
    assert_eq!(
        machine.take_fault_counts(3).unwrap(),
        Some(FaultCounts::default())
    );
    assert_eq!(machine.take_fault_counts(3).unwrap(), None);
    let restored = super::PVMachine::from_snapshot(&executable, &snapshot).unwrap();
    let mut branch = Environment::configured(restored, SeededRng::new(7), clock.clone());
    assert_eq!(branch.aux.faults.boot(1).installed.len(), 1);
    // The next boot starts with the fault and the hold still counting.
    branch.request(Request::Reboot).unwrap();
    branch.run(&mut (), at(10)).unwrap();
    branch.request(Request::HealFault { id: 3 }).unwrap();
    assert_eq!(
        branch.take_fault_counts(3).unwrap(),
        Some(FaultCounts::default())
    );
    assert!(branch.aux.faults.boot(2).holds.len() == 1);
}
