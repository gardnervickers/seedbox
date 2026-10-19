//! Public disarmed state controls and live-client normal-exit regressions.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{fs::File, path::PathBuf, process::Command, time::Duration};

pub(super) fn root(debug: bool) -> PathBuf {
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

pub(super) fn run(
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

#[test]
fn public_disarmed_timer_state_and_concurrent_old_value_chains() {
    for (debug, translated) in [(false, false), (false, true), (true, true)] {
        let root = root(debug);
        let (state, _) = run(&root, "unix-timer_state", &[], translated, debug, &[], None);
        assert_eq!(
            state
                .lines()
                .filter(|line| line.contains(" expected_errno="))
                .count(),
            30
        );
        assert_eq!(
            state
                .lines()
                .filter(|line| line.contains(" expected_interval="))
                .count(),
            30
        );
        assert_eq!(
            state
                .lines()
                .filter(|line| line.contains(" old_interval="))
                .count(),
            9
        );
        assert!(!state.contains("ok=0"), "{state}");
        assert!(state.ends_with("failures=0\n"), "{state}");
        let (concurrent, _) = run(
            &root,
            "unix-timer_concurrent",
            &[],
            translated,
            debug,
            &[],
            None,
        );
        let expected: String = (0..3)
            .map(|kind| {
                format!("kind={kind} writes=256 rejected=256 queries=385 old-value-chain=PASS\n")
            })
            .collect();
        assert_eq!(
            concurrent,
            format!("{expected}PASS concurrent timer transactions\n")
        );
    }
}

fn app_receipts(out: &str) {
    let receipt = out
        .trim()
        .strip_prefix("app-timer receipts=")
        .and_then(|value| value.strip_suffix(" mask=preserved"))
        .expect("positive application handler receipts and preserved SIGUSR1 mask");
    let counts: Vec<u32> = receipt
        .split(',')
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(counts.len(), 3, "{out}");
    assert!(counts.iter().all(|&count| count > 0), "{out}");
}

fn client_receipts(err: &str, expected_pre: u32, expected_post: u32) {
    let receipts: Vec<_> = err
        .lines()
        .filter_map(|line| line.strip_prefix("timer-client callbacks="))
        .collect();
    assert_eq!(receipts.len(), 1, "one public exit receipt: {err}");
    let (callbacks, fields) = receipts[0]
        .split_once(' ')
        .expect("callback and syscall receipts");
    let callbacks: Vec<u32> = callbacks
        .split(',')
        .map(|value| value.parse().unwrap())
        .collect();
    assert_eq!(callbacks.len(), 3, "{err}");
    assert!(callbacks.iter().all(|&count| count >= 2), "{err}");
    assert_eq!(
        fields,
        format!("pre={expected_pre} post={expected_post} ready=1 skipped=1 updates=3")
    );
    assert_eq!(expected_pre, expected_post + 1);
}

#[test]
fn public_live_client_timer_normal_exit_and_application_coexistence() {
    let (native, _) = run(
        &root(false),
        "unix-timer_app_timers",
        &[],
        false,
        false,
        &[],
        None,
    );
    app_receipts(&native);
    for debug in [false, true] {
        let root = root(debug);
        for options in [&[][..], &["-opt_speed", "-disable_traces"][..]] {
            let (out, err) = run(
                &root,
                "unix-timer_exit_marker",
                &[],
                true,
                debug,
                options,
                Some("libunix-timer-client.so"),
            );
            assert!(out.is_empty(), "{out}");
            client_receipts(&err, 101, 100);
        }
        let (out, err) = run(
            &root,
            "unix-timer_app_timers",
            &[],
            true,
            debug,
            &["-opt_speed", "-disable_traces"],
            Some("libunix-timer-client.so"),
        );
        app_receipts(&out);
        client_receipts(&err, 107, 106);
    }
}
