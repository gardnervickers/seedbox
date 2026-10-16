//! Real OS processes exercise ownership and file-deletion barriers. These are
//! protocol/cleanup tests, not native VM or real-QEMU acceptance.
#![cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
use seedbox_application::process_lifetime::{reclaim_barrier, Completion, ExitBarrier, WorkerExit};
use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
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
fn start(root: &Path, mode: &str) -> Server {
    fs::write(root.join("execution-data"), b"must survive uncertainty").unwrap();
    let server = Server(
        Command::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"))
            .arg("--server")
            .arg(root)
            .arg(mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.join("server-stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(&root.join("server-ready"));
    server
}
fn finish(server: &mut Server) {
    let start = Instant::now();
    loop {
        if let Some(exit) = server.0.try_wait().unwrap() {
            assert!(exit.success());
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "server did not finish"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn worker_death_terminates_descendants_before_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let mut server = start(root.path(), "ordinary");
    fs::write(root.path().join("kill-worker"), []).unwrap();
    finish(&mut server);
    let completion: Completion =
        serde_json::from_slice(&fs::read(root.path().join("completion")).unwrap()).unwrap();
    assert_eq!(completion.worker, Some(WorkerExit::Signal(libc::SIGKILL)));
    assert!(completion.cleanup_authorized(), "{completion:?}");
    assert!(!root.path().join("execution-data").exists());
    reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::ZERO,
    )
    .unwrap();
}
#[test]
fn server_death_terminates_worker_and_descendants() {
    let root = tempfile::tempdir().unwrap();
    let mut server = start(root.path(), "ordinary");
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(7),
    )
    .unwrap();
    // A dead server cannot perform cleanup. Restart has the deletion authority.
    assert!(root.path().join("execution-data").exists());
}
#[test]
fn stopped_worker_dies_when_server_dies() {
    let root = tempfile::tempdir().unwrap();
    let mut server = start(root.path(), "stopped");
    wait(&root.path().join("worker-ready"));
    let pid: i32 = fs::read_to_string(root.path().join("worker-pid"))
        .unwrap()
        .parse()
        .unwrap();
    let started = Instant::now();
    while !stopped(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "worker never reached SIGSTOP"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(7),
    )
    .unwrap();
}
#[test]
fn simultaneous_worker_and_server_death_releases_restart_barrier() {
    let root = tempfile::tempdir().unwrap();
    let mut server = start(root.path(), "ordinary");
    fs::write(root.path().join("kill-worker"), []).unwrap();
    server.0.kill().unwrap();
    server.0.wait().unwrap();
    reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(7),
    )
    .unwrap();
}
#[test]
fn escaped_holder_times_out_and_preserves_files() {
    let root = tempfile::tempdir().unwrap();
    let mut server = start(root.path(), "escaped");
    fs::write(root.path().join("kill-worker"), []).unwrap();
    finish(&mut server);
    let completion: Completion =
        serde_json::from_slice(&fs::read(root.path().join("completion")).unwrap()).unwrap();
    assert!(!completion.cleanup_authorized());
    match completion.barrier {
        ExitBarrier::Failed(message) => assert!(
            message.contains("execution lease still held")
                && message.contains("execution files must be kept"),
            "{message}"
        ),
        other => panic!("unexpected barrier {other:?}"),
    }
    assert!(root.path().join("execution-data").exists());
    assert!(reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::ZERO
    )
    .is_err());
    fs::write(root.path().join("exit-holder"), []).unwrap();
    reclaim_barrier(
        &root
            .path()
            .join(seedbox_application::process_lifetime::EXECUTION_LEASE_FILE),
        Duration::from_secs(2),
    )
    .unwrap();
}
#[test]
fn concurrent_executions_do_not_inherit_another_executions_owner_writer() {
    let root = tempfile::tempdir().unwrap();
    let mut server = Server(
        Command::new(env!("CARGO_BIN_EXE_seedbox-lifetime-fixture"))
            .arg("--siblings")
            .arg(root.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(root.path().join("server-stderr")).unwrap())
            .spawn()
            .unwrap(),
    );
    wait(&root.path().join("server-ready"));
    fs::write(root.path().join("cancel-first"), []).unwrap();
    wait(&root.path().join("a/completion"));
    let completion: Completion =
        serde_json::from_slice(&fs::read(root.path().join("a/completion")).unwrap()).unwrap();
    assert!(completion.cleanup_authorized(), "{completion:?}");
    reclaim_barrier(&root.path().join("a/execution.lease"), Duration::ZERO).unwrap();
    assert!(reclaim_barrier(&root.path().join("b/execution.lease"), Duration::ZERO).is_err());
    fs::write(root.path().join("b/kill-worker"), []).unwrap();
    finish(&mut server);
}

#[cfg(target_os = "linux")]
fn stopped(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .any(|line| line.starts_with("State:") && line.split_whitespace().nth(1) == Some("T"))
}
#[cfg(target_os = "macos")]
fn stopped(pid: i32) -> bool {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let length = std::mem::size_of_val(&info) as i32;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            length,
        )
    };
    read == length && info.pbi_status == 4 // Darwin SSTOP.
}
