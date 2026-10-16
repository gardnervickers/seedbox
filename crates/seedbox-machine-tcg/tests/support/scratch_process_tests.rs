//! Real host children, fake QMP/RSP only: never QEMU or native guest execution.
#![allow(
    clippy::disallowed_methods,
    reason = "test code: spawns here never race a Seedbox process writing an executable"
)]
use super::*;
use seedbox_machine::{
    execution::ExecutionConfig,
    machine::{RunContext, Timestamp},
    pv::{Inbox, Input, Outbox, Request},
    rng::RngStream,
    test_support::FixedClock,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    os::unix::fs::PermissionsExt,
    process::{Command, Stdio},
};
const CHILD: &str = env!("CARGO_BIN_EXE_seedbox-tcg-scratch-fixture");
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
fn wrapper(root: &Path, mode: &str) -> (PathBuf, PathBuf) {
    let record = root.join(format!("{mode}-records.ndjson"));
    let executable = root.join(format!("{mode}-exec-wrapper"));
    fs::write(
        &executable,
        format!(
            "#!/bin/sh\nexec {} --scratch-fixture --scratch-mode {} --scratch-record {} \"$@\"\n",
            quote(CHILD),
            quote(mode),
            quote(record.to_str().unwrap())
        ),
    )
    .unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    (executable, record)
}
fn events(record: &Path) -> Vec<Value> {
    fs::read_to_string(record)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
fn starts(record: &Path) -> Vec<Value> {
    events(record)
        .into_iter()
        .filter(|v| v["kind"] == "start")
        .collect()
}
fn reaped(record: &Path) {
    // This Linux proof checks the particular fixture PIDs, not unrelated host
    // processes. Other platforms still exercise kill/wait and file cleanup.
    #[cfg(target_os = "linux")]
    for start in starts(record) {
        assert!(
            !Path::new("/proc")
                .join(start["pid"].as_u64().unwrap().to_string())
                .exists(),
            "fixture child was not reaped"
        );
    }
}
fn owned_dirs(root: &Path) -> Vec<PathBuf> {
    fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("seedbox-tcg-machine-")
        })
        .collect()
}
fn transients(root: &Path) -> usize {
    fs::read_dir(root)
        .unwrap()
        .map(|p| p.unwrap().path())
        .map(|p| {
            usize::from(
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("capture-")
                    || p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("incoming-")
                    || p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("disk-export-"),
            ) + if p.is_dir() { transients(&p) } else { 0 }
        })
        .sum()
}
#[derive(Default)]
struct Entropy(Vec<usize>);
impl RngStream for Entropy {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        self.0.push(bytes.len());
        bytes.fill(self.0.len() as u8);
    }
}
fn reboot(m: &mut TcgMachine, rng: &mut Entropy, count: usize) {
    let now = Timestamp::from_nanos(123);
    let mut inputs: VecDeque<_> = (0..count)
        .map(|_| Input {
            at: now,
            payload: Request::Reboot,
        })
        .collect();
    let mut outputs = Vec::new();
    let mut context = RunContext {
        worker: &mut (),
        clock: &FixedClock(123),
        rng,
        execution: ExecutionConfig::default(),
        inbox: Inbox::new(&mut inputs, now),
        outbox: Outbox::new(&mut outputs, now, now),
    };
    m.run(&mut context, now).unwrap();
    assert!(inputs.is_empty());
    assert!(outputs.is_empty());
}

#[test]
fn abrupt_cold_owner_death_leaves_only_session_reclaimable_files() {
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let target = tempfile::tempdir().unwrap();
    let root = long_root(target.path());
    let artifact = root.join("caller-snapshot");
    fs::write(&artifact, Fixture::default().disk(true).wire().0).unwrap();
    let marker = root.join("caller-ready");
    let mut owner = Owned(
        Command::new(CHILD)
            .args(["--scratch-fixture", "--scratch-owner-probe"])
            .arg("--scratch-root")
            .arg(&root)
            .arg("--scratch-snapshot")
            .arg(artifact)
            .arg("--scratch-ready")
            .arg(&marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !marker.exists() {
        assert!(
            owner.0.try_wait().unwrap().is_none(),
            "cold scratch owner exited before ready"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "cold scratch owner did not become ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let owned = owned_dirs(&root);
    assert_eq!(owned.len(), 1);
    owner.0.kill().unwrap(); // Actual abrupt death: no Rust destructor can run.
    owner.0.wait().unwrap();
    assert!(owned[0].is_dir());
    assert!(root.is_dir(), "backend never removes the caller's root");
    // Explicit caller/session reclamation, not an actual server startup test.
    fs::remove_dir_all(&root).unwrap();
    assert!(!owned[0].exists());
    assert!(target.path().is_dir());
}

#[test]
fn non_utf8_absolute_and_effective_relative_or_default_roots_fail_without_side_effects() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let sandbox = tempfile::tempdir().unwrap();
    let bad = sandbox
        .path()
        .join(OsString::from_vec(b"caller-\xff".to_vec()));
    fs::create_dir(&bad).unwrap();
    let relative = bad.join("relative");
    fs::create_dir(&relative).unwrap();
    for root in [&bad, &relative] {
        fs::write(root.join("caller-marker"), b"keep exact bytes").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    fs::write(&artifact, Fixture::default().disk(true).wire().0).unwrap();
    for case in 0..4 {
        let result = sandbox.path().join(format!("error-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args(["--scratch-fixture", "--scratch-encoding-probe"])
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result);
        match case {
            0 => {
                command.arg("--scratch-root").arg(&bad);
            }
            1 => {
                command
                    .current_dir(&bad)
                    .arg("--scratch-root")
                    .arg("relative");
            }
            2 => {
                command.env("TMPDIR", &bad);
            }
            3 => {
                command.current_dir(&bad).env("TMPDIR", "relative");
            }
            _ => unreachable!(),
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .unwrap();
        assert!(status.success(), "encoding probe failed, case {case}");
        let message = fs::read_to_string(result).unwrap();
        assert!(
            message.contains("UTF-8") && message.contains("choose"),
            "{message}"
        );
        assert_eq!(
            fs::read_dir(&bad).unwrap().count(),
            2,
            "partial directory in raw-byte root, case {case}"
        );
        assert_eq!(
            fs::read_dir(&relative).unwrap().count(),
            1,
            "partial directory in effective relative root, case {case}"
        );
        for root in [&bad, &relative] {
            assert_eq!(
                fs::read(root.join("caller-marker")).unwrap(),
                b"keep exact bytes"
            );
        }
    }
}

#[test]
fn all_valid_utf8_absolute_relative_and_default_spellings_remain_supported() {
    let sandbox = tempfile::tempdir().unwrap();
    let valid = sandbox
        .path()
        .join("UTF-8:α,offset=12 ' \" \\")
        .join("long-".repeat(25));
    fs::create_dir_all(&valid).unwrap();
    let relative = valid.join("relative");
    fs::create_dir(&relative).unwrap();
    for root in [&valid, &relative] {
        fs::write(root.join("caller-marker"), b"keep").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    fs::write(&artifact, Fixture::default().disk(true).wire().0).unwrap();
    for case in 0..4 {
        let result = sandbox.path().join(format!("ok-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args([
                "--scratch-fixture",
                "--scratch-encoding-probe",
                "--scratch-expect-ok",
            ])
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result);
        match case {
            0 => {
                command.arg("--scratch-root").arg(&valid);
            }
            1 => {
                command
                    .current_dir(&valid)
                    .arg("--scratch-root")
                    .arg("relative");
            }
            2 => {
                command.env("TMPDIR", &valid);
            }
            3 => {
                command.current_dir(&valid).env("TMPDIR", "relative");
            }
            _ => unreachable!(),
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .unwrap();
        assert!(status.success(), "valid path probe failed, case {case}");
        assert_eq!(fs::read_to_string(result).unwrap(), "ok");
        assert_eq!(fs::read_dir(&valid).unwrap().count(), 2);
        assert_eq!(fs::read_dir(&relative).unwrap().count(), 1);
        for root in [&valid, &relative] {
            assert_eq!(fs::read(root.join("caller-marker")).unwrap(), b"keep");
        }
    }
}

#[test]
fn tempfile_override_is_default_authority_but_explicit_root_still_wins() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let sandbox = tempfile::tempdir().unwrap();
    let valid = sandbox.path().join("override");
    let environment = sandbox.path().join("environment");
    let explicit = sandbox.path().join("explicit");
    let bad = sandbox
        .path()
        .join(OsString::from_vec(b"override-\xff".to_vec()));
    let relative = bad.join("relative");
    let roots = [&valid, &environment, &explicit, &bad, &relative];
    for root in roots {
        fs::create_dir_all(root).unwrap();
        fs::write(root.join("caller-marker"), b"keep authority").unwrap();
    }
    let artifact = sandbox.path().join("cold-snapshot");
    fs::write(&artifact, Fixture::default().disk(true).wire().0).unwrap();
    for case in 0..5 {
        let (override_root, temp_root, explicit_root, expect_ok) = match case {
            0 => (valid.as_path(), bad.as_path(), None, true),
            1 => (bad.as_path(), environment.as_path(), None, false),
            2 => (bad.as_path(), bad.as_path(), Some(explicit.as_path()), true),
            3 => (
                valid.as_path(),
                environment.as_path(),
                Some(bad.as_path()),
                false,
            ),
            4 => (Path::new("relative"), environment.as_path(), None, false),
            _ => unreachable!(),
        };
        let result = sandbox.path().join(format!("override-result-{case}"));
        let mut command = Command::new(CHILD);
        command
            .args(["--scratch-fixture", "--scratch-encoding-probe"])
            .arg("--scratch-temp-override")
            .arg(override_root)
            .arg("--scratch-snapshot")
            .arg(&artifact)
            .arg("--scratch-result")
            .arg(&result)
            .env("TMPDIR", temp_root);
        if case == 4 {
            command.current_dir(&bad);
        }
        if let Some(root) = explicit_root {
            command.arg("--scratch-root").arg(root);
        }
        if expect_ok {
            command
                .arg("--scratch-expect-ok")
                .arg("--scratch-expected-parent")
                .arg(if case == 0 { &valid } else { &explicit });
        }
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "tempfile authority probe failed, case {case}"
        );
        let message = fs::read_to_string(result).unwrap();
        if expect_ok {
            assert_eq!(message, "ok");
        } else {
            assert!(
                message.contains("UTF-8") && message.contains("choose"),
                "{message}"
            );
        }
        for root in roots {
            assert_eq!(
                fs::read_dir(root).unwrap().count(),
                if root == &bad { 2 } else { 1 },
                "wrong/partial authority, case {case}: {root:?}"
            );
            assert_eq!(
                fs::read(root.join("caller-marker")).unwrap(),
                b"keep authority"
            );
        }
    }
}

#[test]
fn fixture_without_explicit_mode_is_benign_and_produces_no_protocol_output() {
    for args in [vec![], vec!["--help"], vec!["--test-threads=1"]] {
        let output = Command::new(CHILD)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}
#[test]
fn long_quoted_offset_root_exec_wrapper_reboots_recaptures_and_source_free_restore() {
    let source = tempfile::tempdir().unwrap();
    let root = long_root(source.path()).join("quoted ' \" backslash\\ ,offset=4096");
    fs::create_dir(&root).unwrap();
    let (binary, record) = wrapper(&root, "ok");
    let snapshot = decode(&Fixture::live().disk(true).wire().0);
    let mut m = TcgMachine::from_snapshot_in(&binary, &snapshot, &root).unwrap();
    let owned = owned_dirs(&root);
    assert_eq!(owned.len(), 1);
    assert_eq!(
        transients(&owned[0]),
        0,
        "incoming file retained after Ready"
    );
    let mut rng = Entropy::default();
    reboot(&mut m, &mut rng, 2);
    assert_eq!(
        rng.0,
        [8, 8],
        "host scratch must not change boot entropy draws"
    );
    let mut saved = None;
    for _ in 0..2 {
        saved = Some(m.snapshot().unwrap());
        assert_eq!(
            transients(&owned[0]),
            0,
            "capture/export temporary bodies retained"
        );
    }
    assert_eq!(starts(&record).len(), 3);
    assert_eq!(
        events(&record)
            .iter()
            .filter(|v| v["command"]["execute"] == "migrate")
            .count(),
        2
    );
    drop(m);
    assert!(!owned[0].exists());
    reaped(&record);
    source.close().unwrap();
    let saved = saved.unwrap();
    let target = tempfile::tempdir().unwrap();
    let root = long_root(target.path());
    let (binary, record) = wrapper(&root, "ok");
    for _ in 0..2 {
        let mut restored = TcgMachine::from_snapshot_in(&binary, &saved, &root).unwrap();
        let owned = owned_dirs(&root);
        assert_eq!(owned.len(), 1);
        assert_eq!(transients(&owned[0]), 0);
        assert_eq!(encode(&restored.snapshot().unwrap()), encode(&saved));
        drop(restored);
        assert!(owned_dirs(&root).is_empty());
    }
    reaped(&record);
    assert!(root.is_dir(), "caller root must remain owned by caller");
}
#[test]
fn early_exit_eof_is_prompt_and_setup_gdb_failure_reaps_before_cleanup() {
    for mode in ["early-exit", "bad-gdb"] {
        let target = tempfile::tempdir().unwrap();
        let root = long_root(target.path());
        let (binary, record) = wrapper(&root, mode);
        let snapshot = decode(&Fixture::live().disk(true).wire().0);
        let error = TcgMachine::from_snapshot_in(&binary, &snapshot, &root)
            .err()
            .unwrap();
        // The protocol event, not host speed, proves setup unblocked correctly.
        // A retained parent endpoint would produce a transport timeout instead
        // of QMP EOF; a bad GDB reply must reach its explicit handshake check.
        let message = error.to_string();
        assert!(
            message.starts_with(if mode == "early-exit" {
                "missing or oversized QMP response;"
            } else {
                "unexpected GDB attachment response: 0;"
            }),
            "wrong setup completion event: {message}"
        );
        assert!(
            error.to_string().contains(if mode == "early-exit" {
                "scratch fixture early exit"
            } else {
                "unexpected GDB attachment response"
            }),
            "{error}"
        );
        assert!(owned_dirs(&root).is_empty());
        assert!(root.is_dir());
        reaped(&record);
    }
}
#[test]
fn failed_snapshot_removes_partial_body_without_claiming_resumable_execution() {
    let target = tempfile::tempdir().unwrap();
    let root = long_root(target.path());
    let (binary, record) = wrapper(&root, "capture-error");
    let snapshot = decode(&Fixture::live().wire().0);
    let mut m = TcgMachine::from_snapshot_in(&binary, &snapshot, &root).unwrap();
    assert!(m.snapshot().is_err());
    assert_eq!(transients(&owned_dirs(&root)[0]), 0);
    // No retry/run after the error: teardown is the only subsequent operation.
    drop(m);
    assert!(owned_dirs(&root).is_empty());
    reaped(&record);
}
