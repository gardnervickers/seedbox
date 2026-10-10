//! Owner lifetime is enforced even while a worker is blocked before any guest
//! exists. This exercises the actual retained-worker executable without KVM/HVF.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

use std::{
    ffi::CString,
    os::unix::{
        ffi::OsStrExt,
        process::{CommandExt, ExitStatusExt},
    },
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Cleanup(Child);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn eof_on_owner_pipe_stops_a_blocked_native_worker() {
    let directory = tempfile::tempdir().unwrap();
    let fifo = directory.path().join("blocked-config");
    let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: path is a valid terminated string and mode is an ordinary mode_t.
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
    let mut command = Command::new(env!("CARGO_BIN_EXE_seedbox"));
    command.arg("__engine");
    #[cfg(target_os = "macos")]
    command
        .args(["arm64-world-v1", "--owner-pipe", "--seed", "1", "--config"])
        .arg(&fifo)
        .arg("--out")
        .arg(directory.path().join("output"));
    #[cfg(target_os = "linux")]
    command
        .args(["seedbox-internal-worker-v1", "--owner-pipe", "--request"])
        .arg(&fifo);
    let mut child = Cleanup(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "worker should be blocked reading configuration"
    );
    drop(child.0.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.signal(), Some(libc::SIGKILL));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "worker survived owner disappearance"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
