//! Explicit session ownership supplied at the public process boundary.
//! These are OS-process tests, not QEMU/native-machine acceptance.
#![cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
use seedbox_driver_execution::process_lifetime::{
    self, reclaim_barrier, LeasePaths, OwnedExecution,
};
use std::{fs, os::fd::AsRawFd, path::Path, time::Duration};
fn launch(root: &Path, session: Option<&Path>) -> std::io::Result<OwnedExecution> {
    let executable = Path::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"));
    OwnedExecution::spawn(
        executable,
        executable,
        &[
            seedbox_machine::process_lease::ENTRY.into(),
            "--worker".into(),
            root.as_os_str().to_owned(),
            "ordinary".into(),
        ],
        &[],
        fs::File::create(root.join("stdout"))?,
        fs::File::create(root.join("stderr"))?,
        LeasePaths {
            execution: &root.join(process_lifetime::EXECUTION_LEASE_FILE),
            session,
        },
    )
}
fn ready(root: &Path) {
    seedbox_test_time::HangGuard::new(
        "positive lifetime holder readiness; retain 7s for contended process startup",
        Duration::from_secs(7),
    )
    .wait_for(|| root.join("holder-ready").exists().then_some(()))
    .unwrap();
}
#[test]
fn explicit_none_does_not_discover_or_acquire_an_ancestor_session_lease() {
    let parent = tempfile::tempdir().unwrap();
    let ancestor_lease = parent.path().join(process_lifetime::SESSION_LEASE_FILE);
    fs::File::create(&ancestor_lease).unwrap();
    let _exclusive = reclaim_barrier(&ancestor_lease, Duration::ZERO).unwrap();
    let root = parent.path().join("standalone");
    fs::create_dir(&root).unwrap();
    // This execution genuinely has no server session. A similarly named file
    // in its ancestors must neither become authority nor prevent the launch.
    let mut execution = launch(&root, None).unwrap();
    ready(&root);
    fs::write(root.join("kill-worker"), []).unwrap();
    assert!(execution.finish().unwrap().cleanup_authorized());
}
#[test]
fn supplied_nonancestor_session_lease_is_retained_until_monitor_stops_using_inputs() {
    let root = tempfile::tempdir().unwrap();
    let owner = tempfile::tempdir().unwrap();
    let lease_path = owner.path().join("explicit-owner.lease");
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lease_path)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lease.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) },
        0
    );
    let mut execution = launch(root.path(), Some(&lease_path)).unwrap();
    ready(root.path());
    drop(lease);
    assert!(
        reclaim_barrier(&lease_path, Duration::ZERO).is_err(),
        "monitor must hold the supplied session lease"
    );
    fs::write(root.path().join("kill-worker"), []).unwrap();
    assert!(execution.finish().unwrap().cleanup_authorized());
    reclaim_barrier(&lease_path, Duration::ZERO).unwrap();
}
#[test]
fn supplied_missing_session_lease_is_an_error_and_is_not_created() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("missing-session.lease");
    let error = match launch(root.path(), Some(&missing)) {
        Err(error) => error,
        Ok(_) => panic!("missing session ownership must not be fabricated"),
    };
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    assert!(error.to_string().contains(&missing.display().to_string()));
    assert!(!missing.exists());
    assert!(!root.path().join("worker-pid").exists());
}
