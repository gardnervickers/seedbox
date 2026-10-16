//! Process-only follow-ups for environment, async cancellation and restart.
#![cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
use seedbox_application::process_lifetime::{self, reclaim_barrier, OwnedExecution};
use std::{
    ffi::OsString,
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};
fn wait(path: &Path) {
    let start = Instant::now();
    while !path.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(7),
            "missing {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn worker(root: &Path, mode: &str, environment: &[(OsString, OsString)]) -> OwnedExecution {
    worker_with_session(root, mode, environment, None)
}
fn worker_with_session(
    root: &Path,
    mode: &str,
    environment: &[(OsString, OsString)],
    session: Option<&Path>,
) -> OwnedExecution {
    let executable = Path::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"));
    OwnedExecution::spawn(
        executable,
        executable,
        &[
            seedbox_machine::process_lease::ENTRY.into(),
            "--worker".into(),
            root.as_os_str().to_owned(),
            mode.into(),
        ],
        environment,
        fs::File::create(root.join("stdout")).unwrap(),
        fs::File::create(root.join("stderr")).unwrap(),
        process_lifetime::LeasePaths {
            execution: &root.join(process_lifetime::EXECUTION_LEASE_FILE),
            session,
        },
    )
    .unwrap()
}
#[test]
fn explicit_environment_survives_monitor_without_ambient_values() {
    use std::os::unix::ffi::OsStringExt;
    let root = tempfile::tempdir().unwrap();
    let value = OsString::from_vec(vec![b'x', 0xff, b'y']);
    let environment = vec![
        ("SEEDBOX_LIFETIME_ENV".into(), value),
        ("PATH".into(), "/explicit/only".into()),
    ];
    let mut execution = worker(root.path(), "environment", &environment);
    wait(&root.path().join("worker-environment"));
    let actual: Vec<(Vec<u8>, Vec<u8>)> =
        serde_json::from_slice(&fs::read(root.path().join("worker-environment")).unwrap()).unwrap();
    assert_eq!(
        actual,
        vec![
            (b"PATH".to_vec(), b"/explicit/only".to_vec()),
            (b"SEEDBOX_LIFETIME_ENV".to_vec(), vec![b'x', 0xff, b'y'])
        ]
    );
    fs::write(root.path().join("kill-worker"), []).unwrap();
    assert!(execution.finish().unwrap().cleanup_authorized());
}
#[tokio::test(flavor = "current_thread")]
async fn dropping_on_executor_hands_off_join_without_blocking_it() {
    let root = tempfile::tempdir().unwrap();
    let execution = worker(root.path(), "escaped", &[]);
    wait(&root.path().join("holder-ready"));
    let start = Instant::now();
    drop(execution);
    tokio::task::yield_now().await;
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "async drop blocked waiting for process cleanup"
    );
    // The escaped process is still alive, so a destructor must not pretend the
    // execution is cleaned merely because monitor joining has been detached.
    assert!(reclaim_barrier(
        &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
        Duration::ZERO
    )
    .is_err());
    fs::write(root.path().join("exit-holder"), []).unwrap();
    reclaim_barrier(
        &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(3),
    )
    .unwrap();
}
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn restart_during_cleanup_checks_session_then_remaining_execution_users() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("execution-data"),
        b"keep until all users stop",
    )
    .unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"))
            .arg("--server")
            .arg(root.path())
            .arg("escaped")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.path().join("server-stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(&root.path().join("server-ready"));
    let session = root.path().join(process_lifetime::SESSION_LEASE_FILE);
    assert!(reclaim_barrier(&session, Duration::ZERO).is_err());
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    // This actor starts while the monitor is terminating its group. It cannot
    // acquire session ownership until the monitor stops using session paths.
    let _session = reclaim_barrier(&session, process_lifetime::COMPLETION_TIMEOUT).unwrap();
    let execution = root.path().join(process_lifetime::EXECUTION_LEASE_FILE);
    assert!(reclaim_barrier(&execution, Duration::ZERO).is_err());
    assert!(root.path().join("execution-data").exists());
    fs::write(root.path().join("exit-holder"), []).unwrap();
    let _execution = reclaim_barrier(&execution, Duration::from_secs(3)).unwrap();
    fs::remove_file(root.path().join("execution-data")).unwrap();
}
#[test]
fn unproven_cleanup_retains_runtime_deletion_guard() {
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(tempfile::tempdir().unwrap());
    let runtime_path = runtime.path().to_owned();
    fs::write(runtime_path.join("runtime-input"), b"worker input").unwrap();
    let mut execution = worker(root.path(), "escaped", &[]);
    execution.retain_until_join(runtime.clone());
    drop(runtime);
    wait(&root.path().join("holder-ready"));
    fs::write(root.path().join("kill-worker"), []).unwrap();
    assert!(!execution.finish().unwrap().cleanup_authorized());
    drop(execution);
    assert!(runtime_path.join("runtime-input").exists());
    fs::write(root.path().join("exit-holder"), []).unwrap();
    reclaim_barrier(
        &root.path().join(process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(3),
    )
    .unwrap();
    // Test-owned input only, after the independent exit barrier succeeded.
    fs::remove_dir_all(runtime_path).unwrap();
}
