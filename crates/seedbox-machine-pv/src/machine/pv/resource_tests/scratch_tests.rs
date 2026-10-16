//! Host-only scratch placement; no worker/VM/guest is started here.
use super::*;

fn rooted(root: &Path) -> PVMachine {
    PVMachine::construct_with_memory_in(
        Path::new("/never-launched"),
        kernel(),
        b"initrd".to_vec(),
        "rdinit=/init".into(),
        Some(DiskImage::from_bytes(vec![7; 2048]).unwrap()),
        None,
        64 << 20,
        Some(root),
    )
    .unwrap()
}
fn long_root(parent: &Path) -> PathBuf {
    let root = parent
        .join("session,offset=8192")
        .join("long-".repeat(25))
        .join("deep-".repeat(25));
    fs::create_dir_all(&root).unwrap();
    assert!(root.as_os_str().len() > 108);
    root
}

#[test]
fn cold_capture_restores_only_under_new_caller_root_after_source_deletion() {
    let source = tempfile::tempdir().unwrap();
    let source_root = long_root(source.path());
    let mut m = rooted(&source_root);
    let owned = m.directory.path().to_owned();
    assert!(owned.starts_with(&source_root));
    for name in ["kernel", "initrd", "disk-base"] {
        assert!(owned.join(name).is_file());
    }
    let snapshot = m.capture().unwrap();
    assert!(!snapshot
        .bytes
        .windows(owned.as_os_str().len())
        .any(|s| s == owned.to_str().unwrap().as_bytes()));
    drop(m);
    assert!(!owned.exists());
    assert!(
        source_root.exists(),
        "machine never owns caller root deletion"
    );
    source.close().unwrap();
    let target = tempfile::tempdir().unwrap();
    let root = long_root(target.path());
    fs::write(root.join("caller-marker"), b"keep").unwrap();
    for _ in 0..2 {
        let mut m =
            PVMachine::from_snapshot_in(Path::new("/never-launched"), &snapshot, &root).unwrap();
        let dir = m.directory.path().to_owned();
        assert!(dir.starts_with(&root));
        assert_eq!(m.capture().unwrap().bytes, snapshot.bytes);
        // Deferred reboots retain this machine's scratch authority, not a path
        // from the captured source. They do not start a worker until run().
        m.request_reboot(Timestamp::from_nanos(0)).unwrap();
        assert_eq!(m.directory.path(), dir);
        assert!(m.worker.is_none());
        drop(m);
        assert!(!dir.exists());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(fs::read(root.join("caller-marker")).unwrap(), b"keep");
    }
}

#[test]
fn invalid_root_has_context_and_never_falls_back_to_global_temp() {
    let root = tempfile::tempdir().unwrap();
    let not_dir = root.path().join("not-a-directory");
    fs::write(&not_dir, b"caller file").unwrap();
    let error = PVMachine::construct_with_memory_in(
        Path::new("/never-launched"),
        kernel(),
        b"initrd".to_vec(),
        "rdinit=/init".into(),
        None,
        None,
        64 << 20,
        Some(&not_dir),
    )
    .err()
    .unwrap();
    assert!(error.contains(not_dir.to_str().unwrap()), "{error}");
    assert_eq!(fs::read(not_dir).unwrap(), b"caller file");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn restore_overlay_error_removes_partial_target_but_not_caller_root() {
    let source = tempfile::tempdir().unwrap();
    let mut m = rooted(source.path());
    let mut snapshot = m.capture().unwrap();
    let bytes = std::sync::Arc::make_mut(&mut snapshot.bytes);
    *bytes.last_mut().unwrap() = 255; // invalid last overlay-map count; framing unchanged
    drop(m);
    let target = tempfile::tempdir().unwrap();
    fs::write(target.path().join("marker"), b"keep").unwrap();
    assert!(
        PVMachine::from_snapshot_in(Path::new("/never-launched"), &snapshot, target.path())
            .is_err()
    );
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 1);
    assert_eq!(fs::read(target.path().join("marker")).unwrap(), b"keep");
}

#[test]
fn all_unwound_operation_files_remain_reclaimable_under_session() {
    let session = tempfile::tempdir().unwrap();
    let m = rooted(session.path());
    let machine = m.directory.path().to_owned();
    let path = {
        let scratch = restore_image(&machine, b"opaque owned native image").unwrap();
        let path = scratch.path().to_owned();
        assert!(path.starts_with(session.path()));
        path
    };
    assert!(!path.exists(), "failed restore unwinds its named image");
    // Model abandoned named files without pretending to execute a native guest:
    // retain this cold fixture's directory, then let the caller reclaim its tree.
    // Remaining cold fields still drop normally; no RAM or child is leaked.
    let retained = m.directory.keep();
    assert_eq!(retained, machine);
    assert!(machine.exists());
    fs::remove_dir_all(session.path()).unwrap();
    assert!(!machine.exists());
}
