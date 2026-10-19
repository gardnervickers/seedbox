//! Derived public regression: positive nested signal and flush/observer receipts.
//! This finite control does not force every already-unlinked internal interleaving.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{fs::File, path::PathBuf, process::Command, time::Duration};

fn root(debug: bool) -> PathBuf {
    let variable = if debug {
        "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES"
    } else {
        "SEEDBOX_DYNAMORIO_FIXTURES"
    };
    PathBuf::from(
        std::env::var_os(variable)
            .unwrap_or_else(|| panic!("set {variable} to matching prepared fixtures")),
    )
    .canonicalize()
    .expect("prepared matching fixture root")
}

// The existing native supervisor owns cleanup, including on a timeout/panic.
struct Owned(std::process::Child);
impl Drop for Owned {
    fn drop(&mut self) {
        if matches!(self.0.try_wait(), Ok(None)) {
            unsafe {
                libc::kill(self.0.id() as libc::pid_t, libc::SIGTERM);
            }
        }
        let _ = self.0.wait();
    }
}

fn run(
    root: &std::path::Path,
    binary: &str,
    args: &[&str],
    translated: bool,
    debug: bool,
    options: &[&str],
    client: Option<&str>,
) -> (String, String) {
    let logs = tempfile::tempdir().unwrap();
    let stdout = logs.path().join("stdout");
    let stderr = logs.path().join("stderr");
    let mut command = Command::new(root.join("bin/unix-supervisor"));
    if translated {
        command.arg(root.join("runtime/bin64/drrun"));
        if debug {
            command.arg("-debug");
        }
        command
            .arg("-use_dll")
            .arg(root.join(if debug {
                "runtime/lib64/debug/libdynamorio.so"
            } else {
                "runtime/lib64/release/libdynamorio.so"
            }))
            .args(options);
        if let Some(client) = client {
            command.arg("-c").arg(root.join("lib").join(client));
        }
        command.arg("--");
    }
    command
        .arg(root.join("bin").join(binary))
        .args(args)
        .env_clear()
        .env("HOME", logs.path())
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(std::process::Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = Owned(
        command
            .guarded_spawn()
            .expect("start finite public control"),
    );
    let guard = HangGuard::new("finite public control completion", Duration::from_secs(60));
    let status = guard.wait_for(|| child.0.try_wait().transpose());
    drop(child);
    let out = std::fs::read_to_string(stdout).unwrap();
    let err = std::fs::read_to_string(stderr).unwrap();
    assert!(
        matches!(status, Ok(Ok(status)) if status.success()),
        "{binary} {args:?} debug={debug} translated={translated}: {status:?}\n{out}\n{err}"
    );
    eprintln!("{binary} {args:?} debug={debug} translated={translated}\n{out}{err}");
    (out, err)
}

const EXPECTED: &str = "handoff: threads=2 acks=128 nested=128 rounds=64\n";

#[test]
fn public_shared_signal_handoff_and_delayed_flush() {
    check("unix-handoff", "libunix-handoff-client.so", false);
}

#[test]
fn public_shared_signal_handoff_post_syscall_observer() {
    check("unix-handoff_syscall", "libtranslator_observe.so", true);
}

fn check(binary: &str, client: &str, observer: bool) {
    let (out, err) = run(&root(false), binary, &[], false, false, &[], None);
    assert_eq!(out, EXPECTED);
    assert!(err.is_empty(), "{err}");
    for debug in [false, true] {
        let (out, err) = run(&root(debug), binary, &[], true, debug, &[], Some(client));
        assert_eq!(out, EXPECTED);
        if observer {
            let receipts: Vec<_> = err
                .lines()
                .filter_map(|line| line.strip_prefix("path-cost: exit app="))
                .collect();
            assert_eq!(receipts.len(), 1, "one public observer exit receipt: {err}");
            let (app, vm) = receipts[0].split_once(" vm=").expect("observer counts");
            assert!(app.parse::<u64>().unwrap() > 0, "{err}");
            assert_eq!(vm, "0", "no VM module requested: {err}");
        } else {
            assert_eq!(
                err.lines()
                    .filter(|line| *line == "handoff-client: requested=64 completed=64")
                    .count(),
                1,
                "{err}"
            );
        }
    }
}
