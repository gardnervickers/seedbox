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
fn lifecycle_private_entries_are_dispatched_before_public_cli_or_native_boot() {
    for (arguments, expected) in [
        (vec!["--seedbox-execution-monitor-v1"], "execution monitor:"),
        (
            vec!["__engine", "driver-world-v1"],
            "driver-world-v1 needs exactly",
        ),
        (
            vec!["--seedbox-execution-lease-v1", "--seedbox-pv-worker-v1"],
            "execution lease:",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_seedbox"))
            .args(arguments)
            .env_clear()
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains(expected), "{error}");
        assert!(
            !error.contains("Usage:"),
            "private entry reached public CLI: {error}"
        );
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

#[test]
fn monitor_join_removes_parent_owned_native_scratch_after_exit_failure_cancel_and_drop() {
    use seedbox_application::process_lifetime::{
        reclaim_barrier, LeasePaths, OwnedExecution, EXECUTION_LEASE_FILE,
    };
    use std::{ffi::OsString, fs};
    for mode in ["0", "7", "cancel", "drop"] {
        // A failed ownership assertion must preserve even the enclosing inputs.
        let root = tempfile::tempdir().unwrap().keep();
        eprintln!(
            "native scratch host regression: {} ({mode})",
            root.display()
        );
        let machines = root.join("machines");
        fs::create_dir(&machines).unwrap();
        let scratch = tempfile::Builder::new()
            .prefix("native-scratch")
            .rand_bytes(0)
            .tempdir_in(&machines)
            .unwrap();
        let scratch_path = scratch.path().to_owned();
        let ready = root.join("ready");
        let report = root.join("report");
        let script = "mkdir \"$1/seedbox-pv-fixture\"; printf kernel >\"$1/seedbox-pv-fixture/kernel\"; printf initrd >\"$1/seedbox-pv-fixture/initrd\"; printf ready >\"$3\"; case \"$2\" in cancel|drop) exec sleep 60;; *) exit \"$2\";; esac";
        let arguments: Vec<OsString> = vec![
            "-c".into(),
            script.into(),
            "native-scratch-fixture".into(),
            scratch_path.clone().into_os_string(),
            mode.into(),
            ready.clone().into_os_string(),
        ];
        let lease = root.join(EXECUTION_LEASE_FILE);
        let mut process = OwnedExecution::spawn(
            std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")),
            std::path::Path::new("/bin/sh"),
            &arguments,
            &[("PATH".into(), std::env::var_os("PATH").expect("test tools"))],
            fs::File::create(&report).unwrap(),
            fs::File::create(root.join("stderr")).unwrap(),
            LeasePaths {
                execution: &lease,
                session: None,
            },
        )
        .unwrap();
        // Same monitor-owned guard used by the Driver launch, not child Drop.
        process.retain_until_join(scratch);
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(
                Instant::now() < deadline,
                "fixture never reached scratch creation"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        if mode == "cancel" || mode == "drop" {
            assert!(scratch_path.join("seedbox-pv-fixture/initrd").exists());
        }
        if mode == "drop" {
            drop(process); // Ownership transfers to the existing background join.
            let deadline = Instant::now() + Duration::from_secs(5);
            while scratch_path.exists() {
                assert!(
                    Instant::now() < deadline,
                    "async monitor join retained scratch"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        } else {
            if mode == "cancel" {
                process.cancel();
            }
            assert!(process.finish().unwrap().cleanup_authorized());
            drop(process);
        }
        assert!(
            !scratch_path.exists(),
            "native scratch outlived joined group ({mode})"
        );
        assert!(
            report.exists(),
            "persistent execution artifacts must not be scratch"
        );
        let _barrier = reclaim_barrier(&lease, Duration::ZERO).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
