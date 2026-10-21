//! Host-only scratch placement over public Snapshot/TcgMachine boundaries.
//! Opaque boot and migration bytes are never sent to QEMU in these cold tests.
use crate::*;
#[cfg(feature = "test-support")]
mod scratch_process_tests;
use std::{fs, path::PathBuf};

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
fn cold_snapshots_are_path_free_and_new_restores_own_only_new_session_children() {
    let source = tempfile::tempdir().unwrap();
    let root = long_root(source.path());
    let bytes = Fixture::default().disk(true).wire().0;
    let snapshot = decode(&bytes);
    let mut m =
        TcgMachine::from_snapshot_in(Path::new("/never-launched"), &snapshot, &root).unwrap();
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    let owned = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
    assert!(owned
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("seedbox-tcg-machine-"));
    let children: Vec<_> = fs::read_dir(&owned)
        .unwrap()
        .map(|p| p.unwrap().path())
        .collect();
    assert_eq!(children.len(), 1);
    assert_eq!(
        fs::read(children[0].join("base")).unwrap(),
        vec![0x9a; 2048]
    );
    let saved = m.snapshot().unwrap();
    assert_eq!(encode(&saved), encode(&snapshot));
    drop(m);
    assert!(!owned.exists());
    assert!(root.exists());
    source.close().unwrap();
    let target = tempfile::tempdir().unwrap();
    let root = long_root(target.path());
    fs::write(root.join("caller-marker"), b"keep").unwrap();
    for _ in 0..2 {
        let mut m =
            TcgMachine::from_snapshot_in(Path::new("/never-launched"), &saved, &root).unwrap();
        assert_eq!(encode(&m.snapshot().unwrap()), encode(&snapshot));
        drop(m);
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
        assert_eq!(fs::read(root.join("caller-marker")).unwrap(), b"keep");
    }
}

#[test]
fn explicit_root_errors_have_context_without_fallback_or_caller_deletion() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("caller-file");
    fs::write(&path, b"keep").unwrap();
    let snapshot = decode(&Fixture::default().wire().0);
    let error = TcgMachine::from_snapshot_in(Path::new("/never-launched"), &snapshot, &path)
        .err()
        .unwrap();
    assert!(
        error.to_string().contains(path.to_str().unwrap()),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"keep");
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
}

#[test]
fn spawn_failure_cleans_partial_boot_disk_and_machine_directories() {
    let root = tempfile::tempdir().unwrap();
    let root_path = long_root(root.path());
    fs::write(root_path.join("caller-marker"), b"keep").unwrap();
    let snapshot = decode(&Fixture::live().disk(true).wire().0);
    let error = TcgMachine::from_snapshot_in(
        Path::new("/no-such-scratch-fixture-executable"),
        &snapshot,
        &root_path,
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("start /no-such-scratch-fixture-executable"),
        "{error}"
    );
    assert_eq!(fs::read_dir(&root_path).unwrap().count(), 1);
    assert_eq!(fs::read(root_path.join("caller-marker")).unwrap(), b"keep");
}
