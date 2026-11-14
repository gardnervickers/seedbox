//! Retained snapshots through the client. What needs no hypervisor is here;
//! what needs a worker that retains is beside it, for the backend that can.
use super::*;
use seedbox_machine::snapshot::DecodeLimits;

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
mod hvf;

/// Limits that admit a snapshot of `bytes` encoded bytes.
fn limits(bytes: u64) -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: bytes,
        allocation_bytes: bytes * 2,
        collection_items: 50000,
        total_items: 200000,
        string_bytes: 1 << 20,
        blob_bytes: bytes,
    }
}

#[test]
fn files_have_the_same_contents_only_byte_for_byte() {
    let directory = tempfile::tempdir().unwrap();
    let file = |name: &str, bytes: &[u8]| {
        let path = directory.path().join(name);
        fs::write(&path, bytes).unwrap();
        path
    };
    // Longer than one read of the comparison, and differing only at the end.
    let mut bytes = vec![7; (1 << 20) + 5];
    let one = file("one", &bytes);
    assert!(same_contents(&one, &file("same", &bytes)).unwrap());
    assert!(!same_contents(&one, &file("shorter", &bytes[..bytes.len() - 1])).unwrap());
    *bytes.last_mut().unwrap() = 8;
    assert!(!same_contents(&one, &file("changed", &bytes)).unwrap());
    assert!(same_contents(&file("empty", &[]), &file("also empty", &[])).unwrap());
    assert!(same_contents(&one, &directory.path().join("absent")).is_err());
}

/// Retention keeps live boots. A machine between boots has no worker to keep
/// one in, so its snapshot is the one it takes without retention, and neither
/// that snapshot nor its encoding starts a worker when restored.
#[test]
fn a_machine_between_boots_retains_nothing() {
    let executable = Path::new("/never-launched");
    let mut machine = PVMachine::from_boot_inputs(
        executable,
        test_image::program(&[test_image::Op::Ready, test_image::Op::Idle]),
        b"initrd".to_vec(),
    )
    .unwrap();
    let plain = machine.snapshot().unwrap();
    machine.retain_snapshots(Retention {
        limit_bytes: 0,
        verify: true,
    });
    let snapshot = machine.snapshot().unwrap();
    assert!(matches!(snapshot.body, Body::Image(_)));
    assert_eq!(snapshot.copied_pages(), None);
    assert_eq!(snapshot.bytes(), plain.bytes());
    assert_eq!(machine.restored(), None);

    let mut encoded = Vec::new();
    snapshot.write_to(&mut encoded).unwrap();
    let decoded = Snapshot::read_from(&mut &encoded[..], &limits(1 << 20)).unwrap();
    assert!(matches!(decoded.body, Body::Image(_)));
    for snapshot in [&snapshot, &decoded] {
        let restored = PVMachine::from_snapshot(executable, snapshot).unwrap();
        assert_eq!(restored.restored(), Some(Restored::Deferred));
        assert!(restored.worker.is_none());
    }
}

/// A Linux guest with a command accepted and its reply pending, every
/// snapshot retained and verified. Each restore finds its snapshot's worker
/// parked, so where the backend retains, each is a rewind, and of a small
/// part of RAM.
#[test]
#[ignore = "requires a native PV worker with snapshots, kernel and static guest service initramfs"]
fn a_linux_guest_continues_exactly_once_from_retained_snapshots() {
    use seedbox_machine::test_support::{
        guest_service_snapshot_tests::check, prepared_application as prepared, prepared_fixture,
    };
    const MEMORY: u64 = 128 << 20;
    // Tracked pages are 16 KiB where a backend retains.
    const PAGES: u64 = MEMORY / 16384;
    let input =
        |name: &str| PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")));
    let retention = Retention {
        limit_bytes: u64::MAX,
        verify: true,
    };
    let executable = input("SEEDBOX_PV_WORKER");
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        MEMORY,
    );
    let mut machine =
        PVMachine::new(&executable, &prepared_fixture::bootstrap("PV"), definition).unwrap();
    machine.retain_snapshots(retention);
    let restored = std::cell::RefCell::new(Vec::new());
    check(machine, |snapshot| {
        let mut machine = PVMachine::from_snapshot(&executable, snapshot).unwrap();
        restored.borrow_mut().push(machine.restored().unwrap());
        machine.retain_snapshots(retention);
        machine
    });
    let restored = restored.into_inner();
    if RETAINS {
        assert_eq!(restored.len(), 3);
        for restored in restored {
            assert!(
                matches!(restored, Restored::Rewound { pages } if pages < PAGES / 8),
                "{restored:?}"
            );
        }
    } else {
        assert_eq!(restored, [Restored::Loaded; 3]);
    }
}
