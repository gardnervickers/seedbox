//! Process-only tests of the REAL monitor/anchor path. Not native guest proof.
#![cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
use seedbox_driver_execution::process_lifetime::{
    self, reclaim_barrier, LeasePaths, OwnedExecution, WorkerExit, WorkerFailureInjector,
};
use std::{fs, path::Path, sync::Arc, time::Duration};

fn launch(root: &Arc<tempfile::TempDir>) -> OwnedExecution {
    // This fixture's monitor entry calls process_lifetime::serve_monitor itself;
    // its worker/descendant are OS processes, not a substitute cleanup monitor.
    let executable = Path::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"));
    let mut execution = OwnedExecution::spawn(
        executable,
        executable,
        &[
            seedbox_machine::process_lease::ENTRY.into(),
            "--worker".into(),
            root.path().as_os_str().to_owned(),
            "ordinary".into(),
        ],
        &[],
        fs::File::create(root.path().join("stdout")).unwrap(),
        fs::File::create(root.path().join("stderr")).unwrap(),
        LeasePaths {
            execution: &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
            session: None,
        },
    )
    .unwrap();
    // Even a test panic must keep execution inputs until the real monitor joins.
    execution.retain_until_join(root.clone());
    seedbox_test_time::HangGuard::new(
        "positive worker/descendant readiness before requesting owned failure",
        Duration::from_secs(15),
    )
    .wait_for(|| root.path().join("holder-ready").exists().then_some(()))
    .unwrap();
    execution
}

#[test]
fn original_worker_failure_uses_real_monitor_receipt_and_releases_lease() {
    let root = Arc::new(tempfile::tempdir().unwrap());
    let injector = WorkerFailureInjector::default();
    let mut execution = launch(&root);
    execution.bind_failure_injector(&injector).unwrap();
    injector.fail_worker().unwrap();
    let completion = execution.finish().unwrap();
    assert_eq!(completion.worker, Some(WorkerExit::Signal(libc::SIGKILL)));
    assert!(completion.cleanup_authorized(), "{completion:?}");
    reclaim_barrier(
        &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(
        injector.fail_worker().unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
}

#[test]
fn keeping_failure_handle_alive_does_not_mask_owner_cancellation_eof() {
    let root = Arc::new(tempfile::tempdir().unwrap());
    let injector = WorkerFailureInjector::default();
    let mut execution = launch(&root);
    execution.bind_failure_injector(&injector).unwrap();
    execution.cancel();
    // The injector remains alive across finish: a strong endpoint clone would
    // mask EOF and prevent this ordinary monitor receipt/cleanup barrier.
    let completion = execution.finish().unwrap();
    assert!(completion.cleanup_authorized(), "{completion:?}");
    reclaim_barrier(
        &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
        Duration::ZERO,
    )
    .unwrap();
    assert_eq!(
        injector.fail_worker().unwrap_err().kind(),
        std::io::ErrorKind::NotConnected
    );
}
