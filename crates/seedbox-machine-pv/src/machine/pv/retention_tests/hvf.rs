//! Retained snapshots in workers that record guest writes. Every machine here
//! verifies: each snapshot against a whole capture taken with it, and all of
//! RAM after each rewind.
use super::*;
use seedbox_machine::{clock::SimulationClock, rng::SeededRng};
type Environment = seedbox_machine::test_support::test_environment::Environment<PVMachine>;

/// Bytes of one RAM page a worker tracks.
const PAGE: u64 = 16384;
const VERIFIED: Retention = Retention {
    limit_bytes: u64::MAX,
    verify: true,
};

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn worker() -> PathBuf {
    std::env::var_os("SEEDBOX_PV_WORKER")
        .expect("set SEEDBOX_PV_WORKER")
        .into()
}

/// Each run adds one to a word of the guest's own image and yields to the
/// bound. The run that counts to `limit` powers off instead, so the runs a
/// machine has left tell what its RAM held.
fn counter(limit: u32) -> Vec<u8> {
    assert!(limit < 4096);
    test_image::kernel(&[
        0xd2a1_4021,               // MOV X1,#0x0a010000 (PV registers).
        0xd280_0060,               // MOV X0,#3.
        0xf900_1020,               // STR X0,[X1,#32] (ready).
        0x1000_01a3,               // ADR X3,+52 (the count).
        0xf940_0064,               // LDR X4,[X3].
        0x9100_0484,               // ADD X4,X4,#1.
        0xf900_0064,               // STR X4,[X3].
        0xf100_009f | limit << 10, // CMP X4,#limit.
        0x5400_0060,               // B.EQ +12 (power off).
        0xf900_0c20,               // STR X0,[X1,#24] (yield).
        0x17ff_fffa,               // B -24 (count again).
        0xd280_0100,               // MOV X0,#8.
        0xf2b0_8000,               // MOVK X0,#0x8400,LSL #16 (PSCI SYSTEM_OFF).
        0xd400_0002,               // HVC #0.
        0x1400_0000,               // B .
        0,                         // Aligns the count.
        0,
        0, // The count.
    ])
}

/// A retaining machine that counts to `limit` and has counted once.
fn counting(limit: u32, disk: Option<DiskImage>) -> Environment {
    counting_in(limit, disk, None)
}
/// As [`counting`], with its scratch beneath `root`.
fn counting_in(limit: u32, disk: Option<DiskImage>, root: Option<&Path>) -> Environment {
    let raw = PVMachine::construct_with_memory_in(
        &worker(),
        counter(limit),
        b"initrd".to_vec(),
        TEST_COMMAND_LINE.into(),
        disk,
        None,
        boot::RAM_BYTES as u64,
        root,
    )
    .unwrap();
    let clock = Arc::new(SimulationClock::new(0));
    let mut machine = Environment::configured(raw, SeededRng::new(7), clock);
    machine.retain_snapshots(VERIFIED);
    step(&mut machine, 10);
    machine
}
fn restore(snapshot: &Snapshot) -> Environment {
    let raw = PVMachine::from_snapshot(&worker(), snapshot).unwrap();
    let clock = Arc::new(SimulationClock::new(0));
    Environment::configured(raw, SeededRng::new(0), clock)
}
/// Count once more, short of the limit.
fn step(machine: &mut Environment, upto: u64) {
    assert_eq!(
        machine.run(&mut (), at(upto)).unwrap(),
        StopReason::BoundaryReached
    );
}
/// Run on from `from`, ten at a time, until the guest powers off: every stop.
fn to_power_off(machine: &mut Environment, from: u64) -> Vec<StopReason> {
    let mut stops = Vec::new();
    for run in 1..=64 {
        let stop = machine.run(&mut (), at(from + run * 10)).unwrap();
        stops.push(stop);
        match stop {
            StopReason::BoundaryReached => {}
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                ..
            } => return stops,
            other => panic!("the counting guest stopped with {other:?}"),
        }
    }
    panic!("the guest counted past its limit");
}
fn process(machine: &Environment) -> u32 {
    machine.worker.as_ref().expect("a live boot").process()
}
/// Whether the process exists; a worker that was terminated was also reaped.
fn exists(process: u32) -> bool {
    unsafe { libc::kill(process as i32, 0) == 0 }
}
/// What a refused snapshot reports.
fn refusal(error: &std::io::Error) -> SnapshotStoreFull {
    assert_eq!(error.kind(), std::io::ErrorKind::QuotaExceeded);
    *error
        .get_ref()
        .and_then(|inner| inner.downcast_ref())
        .expect("a full snapshot store")
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_parked_worker_goes_back_to_its_snapshot_in_place() {
    let mut machine = counting(5, None);
    step(&mut machine, 20);
    let snapshot = machine.snapshot().unwrap();
    assert!(matches!(snapshot.body, Body::Retained(_)));
    let worker = process(&machine);
    // Three more counts to five.
    let stops = to_power_off(&mut machine, 20);
    assert_eq!(stops.len(), 3);
    assert!(machine.worker.is_none());
    for _ in 0..2 {
        let mut branch = restore(&snapshot);
        // Only the page holding the count was written since.
        assert_eq!(branch.restored(), Some(Restored::Rewound { pages: 1 }));
        assert_eq!(process(&branch), worker);
        // Retention is the caller's to give, not the snapshot's.
        let plain = branch.snapshot().unwrap();
        assert!(matches!(plain.body, Body::Image(_)));
        assert_eq!(plain.copied_pages(), None);
        // A count the rewind left in RAM would take the guest past five.
        assert_eq!(to_power_off(&mut branch, 20), stops);
    }
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn snapshots_of_a_rewound_machine_branch_from_it_in_the_same_worker() {
    let mut machine = counting(6, None);
    step(&mut machine, 20);
    let two = machine.snapshot().unwrap();
    let worker = process(&machine);
    assert_eq!(to_power_off(&mut machine, 20).len(), 4);

    let mut branch = restore(&two);
    branch.retain_snapshots(VERIFIED);
    step(&mut branch, 30);
    step(&mut branch, 40);
    let four = branch.snapshot().unwrap();
    assert_eq!(four.copied_pages(), Some(1));
    assert_eq!(to_power_off(&mut branch, 40).len(), 2);

    // Either can be gone back to, in any order, however far the last ran.
    for (snapshot, from, left) in [(&four, 40, 2), (&two, 20, 4), (&two, 20, 4), (&four, 40, 2)] {
        let mut branch = restore(snapshot);
        assert_eq!(branch.restored(), Some(Restored::Rewound { pages: 1 }));
        assert_eq!(process(&branch), worker);
        assert_eq!(to_power_off(&mut branch, from).len(), left);
    }
    // A machine dropped mid-boot parks its worker as one that exits does.
    let mut branch = restore(&four);
    step(&mut branch, 50);
    drop(branch);
    let mut branch = restore(&two);
    assert_eq!(branch.restored(), Some(Restored::Rewound { pages: 1 }));
    assert_eq!(to_power_off(&mut branch, 20).len(), 4);
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_snapshot_whose_worker_is_occupied_is_copied_out_into_a_new_one() {
    let mut machine = counting(5, None);
    step(&mut machine, 20);
    let snapshot = machine.snapshot().unwrap();
    let mut branch = restore(&snapshot);
    assert_eq!(branch.restored(), Some(Restored::Exported));
    assert_ne!(process(&branch), process(&machine));
    // The two run on from the same count, neither seeing the other's.
    step(&mut machine, 30);
    step(&mut machine, 40);
    step(&mut branch, 30);
    let stops = to_power_off(&mut machine, 40);
    assert_eq!(stops.len(), 1);
    step(&mut branch, 40);
    assert_eq!(to_power_off(&mut branch, 40), stops);
    // Its machine has left, so the snapshot's worker is now parked.
    let mut again = restore(&snapshot);
    assert_eq!(again.restored(), Some(Restored::Rewound { pages: 1 }));
    assert_eq!(to_power_off(&mut again, 20).len(), 3);
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_worker_lives_until_its_machine_and_its_last_snapshot_are_gone() {
    // No snapshot: the worker exits with its machine.
    let machine = counting(5, None);
    let worker = process(&machine);
    assert!(exists(worker));
    drop(machine);
    assert!(!exists(worker));

    let mut machine = counting(5, None);
    let snapshot = machine.snapshot().unwrap();
    let worker = process(&machine);
    let scratch = machine.directory.path().to_owned();
    let copy = snapshot.clone();
    drop(machine);
    assert!(exists(worker) && scratch.exists());
    drop(copy);
    assert!(exists(worker) && scratch.exists());
    // The snapshot is dropped while a machine runs in its worker, which then
    // only forgets it. Its scratch goes with it: the machine running there
    // has its own.
    let mut branch = restore(&snapshot);
    assert_eq!(process(&branch), worker);
    drop(snapshot);
    assert!(exists(worker) && !scratch.exists());
    assert_eq!(to_power_off(&mut branch, 10).len(), 4);
    assert!(!exists(worker) && !scratch.exists());
    drop(branch);

    // The other order: the snapshot outlives every machine.
    let mut machine = counting(5, None);
    let snapshot = machine.snapshot().unwrap();
    let worker = process(&machine);
    let scratch = machine.directory.path().to_owned();
    drop(machine);
    assert!(exists(worker) && scratch.exists());
    drop(snapshot);
    assert!(!exists(worker) && !scratch.exists());
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_branch_keeps_the_snapshots_it_retains_beneath_its_own_scratch_root() {
    let (source_root, branch_root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut machine = counting_in(5, None, Some(source_root.path()));
    let snapshot = machine.snapshot().unwrap();
    let worker = process(&machine);
    let source = machine.directory.path().to_owned();
    drop(machine);

    let raw = PVMachine::from_snapshot_in(&self::worker(), &snapshot, branch_root.path()).unwrap();
    let clock = Arc::new(SimulationClock::new(0));
    let mut branch = Environment::configured(raw, SeededRng::new(0), clock);
    assert_eq!(process(&branch), worker);
    branch.retain_snapshots(VERIFIED);
    step(&mut branch, 20);
    let later = branch.snapshot().unwrap();
    let Body::Retained(retained) = &later.body else {
        panic!("a retained snapshot");
    };
    assert_eq!(retained.version.scratch(), branch.directory.path());
    assert!(branch.directory.path().starts_with(branch_root.path()));
    step(&mut branch, 30);
    drop(branch);

    // Nothing of the branch's needs the source's scratch, or its root.
    drop(snapshot);
    assert!(exists(worker) && !source.exists());
    source_root.close().unwrap();
    later.write_to(&mut Vec::new()).unwrap();
    let mut again = restore(&later);
    assert_eq!(again.restored(), Some(Restored::Rewound { pages: 1 }));
    assert_eq!(process(&again), worker);
    assert_eq!(to_power_off(&mut again, 20).len(), 3);
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_retained_snapshot_is_the_whole_capture_of_its_moment() {
    let mut machine = counting(5, None);
    step(&mut machine, 20);
    let whole = machine.capture().unwrap();
    assert!(matches!(whole.body, Body::Image(_)));
    let retained = machine.snapshot().unwrap();
    assert!(retained.bytes() == whole.bytes());
    // It does not follow its machine.
    step(&mut machine, 30);
    let later = machine.capture().unwrap().bytes();
    assert!(later != whole.bytes());
    assert!(retained.bytes() == whole.bytes());

    let mut encoded = Vec::new();
    retained.write_to(&mut encoded).unwrap();
    let mut expected = Vec::new();
    whole.write_to(&mut expected).unwrap();
    assert!(encoded == expected);
    drop(expected);
    let decoded = Snapshot::read_from(&mut &encoded[..], &limits(1 << 30)).unwrap();
    drop(encoded);
    assert!(matches!(decoded.body, Body::Image(_)));
    assert!(decoded.bytes() == whole.bytes());
    drop(whole);

    // However it is restored it is the same machine: one count on, each
    // captures what the machine it was taken from did.
    let mut loaded = restore(&decoded);
    assert_eq!(loaded.restored(), Some(Restored::Loaded));
    let mut exported = restore(&retained);
    assert_eq!(exported.restored(), Some(Restored::Exported));
    assert_eq!(to_power_off(&mut machine, 30).len(), 2);
    let mut rewound = restore(&retained);
    assert_eq!(rewound.restored(), Some(Restored::Rewound { pages: 1 }));
    for branch in [&mut loaded, &mut exported, &mut rewound] {
        step(branch, 30);
        assert!(branch.capture().unwrap().bytes() == later);
        assert_eq!(to_power_off(branch, 30).len(), 2);
    }
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_full_store_refuses_a_snapshot_and_loses_nothing() {
    let mut machine = counting(8, None);
    let retain = |machine: &mut Environment, limit_bytes| {
        machine.retain_snapshots(Retention {
            limit_bytes,
            verify: true,
        });
        machine.snapshot()
    };
    let refused = |machine: &mut Environment, limit_bytes| {
        refusal(&retain(machine, limit_bytes).err().unwrap())
    };
    let error = retain(&mut machine, 0).err().unwrap();
    assert_eq!(
        refusal(&error),
        SnapshotStoreFull {
            held_bytes: 0,
            limit_bytes: 0
        }
    );
    assert_eq!(
        error.to_string(),
        "PV snapshot store is full: 0 bytes held, limit 0"
    );
    // The first snapshot copies the pages of RAM that are not zero.
    let first = retain(&mut machine, u64::MAX).unwrap();
    let held = first.copied_pages().unwrap() * PAGE;
    assert!(held > 0 && held < machine.memory_bytes);
    // An unchanged machine costs nothing more.
    let same = retain(&mut machine, held).unwrap();
    assert_eq!(same.copied_pages(), Some(0));
    // A count on, the page holding it does not fit.
    step(&mut machine, 20);
    let full = |held_bytes, limit_bytes| SnapshotStoreFull {
        held_bytes,
        limit_bytes,
    };
    assert_eq!(refused(&mut machine, held), full(held, held));
    // The refusal lost nothing: the next snapshot still copies that page.
    let second = retain(&mut machine, held + PAGE).unwrap();
    assert_eq!(second.copied_pages(), Some(1));
    step(&mut machine, 30);
    assert_eq!(
        refused(&mut machine, held + PAGE),
        full(held + PAGE, held + PAGE)
    );
    // Dropping the snapshots that alone hold a page makes room for one.
    drop(same);
    assert_eq!(
        refused(&mut machine, held + PAGE),
        full(held + PAGE, held + PAGE)
    );
    drop(first);
    let third = retain(&mut machine, held + PAGE).unwrap();
    assert_eq!(third.copied_pages(), Some(1));
    // But never below a copy of RAM: the worker keeps its last snapshot as
    // the base of its next.
    drop(second);
    step(&mut machine, 40);
    assert_eq!(refused(&mut machine, held), full(held, held));
    drop(third);
    assert_eq!(refused(&mut machine, held), full(held, held));
    let fourth = retain(&mut machine, held + PAGE).unwrap();
    assert_eq!(fourth.copied_pages(), Some(1));
    // Refused or kept, no snapshot changed the machine.
    assert_eq!(to_power_off(&mut machine, 40).len(), 4);
    let mut branch = restore(&fourth);
    assert_eq!(branch.restored(), Some(Restored::Rewound { pages: 1 }));
    assert_eq!(to_power_off(&mut branch, 40).len(), 4);
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_snapshot_whose_worker_died_fails_and_drops_cleanly() {
    let kill = |worker: u32| assert_eq!(unsafe { libc::kill(worker as i32, libc::SIGKILL) }, 0);
    let restore = |snapshot: &Snapshot| PVMachine::from_snapshot(&worker(), snapshot).map(drop);
    // Under its machine.
    let mut machine = counting(5, None);
    let snapshot = machine.snapshot().unwrap();
    kill(process(&machine));
    assert!(machine.run(&mut (), at(20)).is_err());
    assert!(machine.worker.is_none());
    assert_eq!(restore(&snapshot).unwrap_err(), "PV worker is gone");
    assert!(snapshot.write_to(&mut Vec::new()).is_err());
    drop(snapshot);
    drop(machine);

    // Parked: the first use finds out.
    let mut machine = counting(5, None);
    let snapshot = machine.snapshot().unwrap();
    let parked = process(&machine);
    drop(machine);
    kill(parked);
    assert!(restore(&snapshot).is_err());
    assert_eq!(restore(&snapshot).unwrap_err(), "PV worker is gone");
    assert!(snapshot.write_to(&mut Vec::new()).is_err());
    drop(snapshot);
    assert!(!exists(parked));

    assert_eq!(to_power_off(&mut counting(3, None), 10).len(), 2);
}

#[test]
#[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
fn a_retained_snapshot_keeps_the_disk_of_its_boot() {
    let mut machine = counting(5, Some(DiskImage::zeroed(1 << 20).unwrap()));
    // The worker owns the disk during a boot.
    assert!(machine.disk_store().is_none());
    let snapshot = machine.snapshot().unwrap();
    assert!(snapshot.bytes() == machine.capture().unwrap().bytes());
    let mut exported = restore(&snapshot);
    assert_eq!(exported.restored(), Some(Restored::Exported));
    assert!(exported.disk_store().is_none());
    assert_eq!(to_power_off(&mut machine, 10).len(), 4);
    assert!(machine.disk_store().is_some());

    let mut rewound = restore(&snapshot);
    assert_eq!(rewound.restored(), Some(Restored::Rewound { pages: 1 }));
    assert!(rewound.disk_store().is_none());
    for branch in [&mut rewound, &mut exported] {
        assert_eq!(to_power_off(branch, 10).len(), 4);
        // The boot's end hands the disk back, from whichever worker held it.
        assert!(branch.disk_store().is_some());
    }
}
