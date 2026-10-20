//! External process boundary only. Prepared binaries come from the same-source
//! SDK/runtime fixture package; no compilation or downloads happen in a test.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{fs::File, path::PathBuf, process::Command, time::Duration};

pub enum Profile<'a> {
    Native,
    Plain(&'a [&'a str]),
    Timers,
    DebugProf,
}
pub fn run(binary: &str, args: &[&str], profile: Profile<'_>) -> String {
    let selected = match &profile {
        Profile::Native => "native/direct-release".to_owned(),
        Profile::Plain(options) => format!("plain-release options={options:?}"),
        Profile::Timers => "client-release timers".to_owned(),
        Profile::DebugProf => "plain-debug options=[-prof_pcs]".to_owned(),
    };
    let context = format!("{binary} {args:?} profile={selected}");
    let debug = matches!(profile, Profile::DebugProf);
    let key = if debug {
        "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES"
    } else {
        "SEEDBOX_DYNAMORIO_FIXTURES"
    };
    let root = PathBuf::from(
        std::env::var_os(key)
            .unwrap_or_else(|| panic!("set {key} to prepared same-source fixtures")),
    );
    let report_dir = std::env::var_os("SEEDBOX_TEST_REPORT_DIR").map(PathBuf::from);
    let logs = log_directory(report_dir.as_deref());
    let stdout = logs.path().join("stdout");
    let stderr = logs.path().join("stderr");
    // Existing owned supervisor kills/reaps descendants on termination. The
    // child PID remains owned until wait; no process-group PID reuse race.
    let mut command = Command::new(root.join("bin/unix-supervisor"));
    if !matches!(profile, Profile::Native) {
        command.arg(root.join("runtime/bin64/drrun"));
        let build = if debug { "debug" } else { "release" };
        if debug {
            command.arg("-debug");
        }
        command
            .arg("-use_dll")
            .arg(root.join(format!("runtime/lib64/{build}/libdynamorio.so")));
        match profile {
            Profile::Plain(options) => {
                command.args(options);
            }
            Profile::Timers => {
                command
                    .arg("-c")
                    .arg(root.join("lib/libunix-signal-models-client.so"));
            }
            Profile::DebugProf => {
                command.arg("-prof_pcs");
            }
            Profile::Native => unreachable!(),
        }
        command.arg("--");
    }
    command
        .arg(root.join(format!("bin/unix-signal-models-{binary}")))
        .args(args)
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = command
        .guarded_spawn()
        .expect("start prepared signal model fixture");
    let guard = HangGuard::new("signal model positive completion", Duration::from_secs(180));
    let status = guard.wait_for(|| child.try_wait().transpose());
    let failed = !status
        .as_ref()
        .is_ok_and(|s| s.as_ref().is_ok_and(|s| s.success()));
    // Capture while the application is still present, before normal supervisor
    // cleanup. Diagnostics cannot turn a timeout or failed process into success.
    if failed && binary == "storm" {
        // Also emit the snapshot before scratch cleanup, including when the
        // report directory or diagnostic file cannot be written.
        eprintln!("{context}: {}", capture_storm(logs.path()));
    }
    if !matches!(status, Ok(Ok(_))) {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = child.wait();
    }
    let out = std::fs::read_to_string(stdout).unwrap();
    let err = std::fs::read_to_string(stderr).unwrap();
    let retained = if failed { Some(logs.keep()) } else { None };
    assert!(
        status
            .as_ref()
            .is_ok_and(|s| s.as_ref().is_ok_and(|s| s.success())),
        "{context}: {status:?} retained={retained:?}\n{out}\n{err}"
    );
    if matches!(profile, Profile::Timers) {
        let receipt = err
            .lines()
            .find_map(|line| line.strip_prefix("signal-model-client callbacks="))
            .expect("public dr_set_itimer callback receipt");
        let counts: Vec<u64> = receipt.split(',').map(|s| s.parse().unwrap()).collect();
        assert!(counts.len() == 3 && counts.iter().all(|n| *n > 0), "{err}");
    }
    out
}

fn log_directory(report_dir: Option<&std::path::Path>) -> tempfile::TempDir {
    // The checker deletes TMPDIR after this target, but owns a separate report
    // directory for retained evidence. Successful runs still remove their logs.
    if let Some(dir) = report_dir {
        match tempfile::Builder::new()
            .prefix("signal-model-")
            .tempdir_in(dir)
        {
            Ok(logs) => return logs,
            Err(error) => eprintln!("signal model report directory {}: {error}", dir.display()),
        }
    }
    tempfile::tempdir().expect("signal model scratch directory")
}

// The C schema is also the decoder's field list: no second layout to drift.
// Two bounded reads are explicitly partial, not a cross-process seqlock claim.
fn capture_storm(dir: &std::path::Path) -> String {
    use std::io::Read;
    let names: Vec<_> = include_str!("storm_diagnostics.h")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("D(")?.split_once(')').map(|v| v.0))
        .collect();
    let mut report = String::from(
        "storm diagnostic: partial native-endian snapshots; roles=TID:source-line; no atomic whole-record guarantee\n",
    );
    let mut pid = None;
    for sample in 0..2 {
        let bytes = match std::fs::read(dir.join("storm-diagnostic.bin")) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.push_str(&format!("unavailable: {error}\n"));
                break;
            }
        };
        if bytes.len() != 40 * names.len() * 8 {
            report.push_str(&format!("partial file length={}\n", bytes.len()));
            continue;
        }
        for record in bytes.chunks_exact(names.len() * 8) {
            let values: Vec<_> = record
                .as_chunks::<8>()
                .0
                .iter()
                .map(|v| u64::from_ne_bytes(*v))
                .collect();
            if values[0] == 0 {
                continue;
            }
            pid = Some(values[1]);
            report.push_str(&format!("sample={sample} "));
            for (name, value) in names.iter().zip(values) {
                if name.ends_with("_role") {
                    report.push_str(&format!("{name}={}:{} ", value >> 32, value & 0xffff_ffff));
                } else {
                    report.push_str(&format!("{name}={value} "));
                }
            }
            report.push('\n');
        }
    }
    if let Some(pid) = pid {
        let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
        let mut paths = vec![proc.join("timers")];
        match std::fs::read_dir(proc.join("task")) {
            Ok(tasks) => {
                for task in tasks.take(128).flatten() {
                    paths.push(task.path().join("status"));
                    paths.push(task.path().join("wchan"));
                    paths.push(task.path().join("syscall"));
                }
            }
            Err(error) => report.push_str(&format!("tasks unavailable: {error}\n")),
        }
        for path in paths {
            let mut text = String::new();
            let result =
                File::open(&path).and_then(|file| file.take(16384).read_to_string(&mut text));
            report.push_str(&format!("{} {result:?}\n{text}\n", path.display()));
        }
    }
    if let Err(error) = std::fs::write(dir.join("storm-diagnostic.txt"), &report) {
        eprintln!("storm diagnostic write failed: {error}");
    }
    report
}

#[test]
fn storm_diagnostic_retains_interrupted_publication() {
    let dir = tempfile::tempdir().unwrap();
    // A controller interrupted halfway through publication is evidence, not a
    // reason for the diagnostic reader to spin until it becomes consistent.
    let mut words = vec![0_u64; 40 * 45];
    words[0] = 1;
    words[1] = u64::MAX; // No corresponding /proc entry is expected or required.
    words[2] = (123_u64 << 32) | 456;
    words[7] = 3;
    words[8] = 254;
    let mut bytes: Vec<_> = words.into_iter().flat_map(u64::to_ne_bytes).collect();
    std::fs::write(dir.path().join("storm-diagnostic.bin"), &bytes).unwrap();
    capture_storm(dir.path());
    let text = std::fs::read_to_string(dir.path().join("storm-diagnostic.txt")).unwrap();
    assert!(text.contains("main_role=123:456"), "{text}");
    assert!(text.contains("controller_seq=3 predicate=254"), "{text}");
    assert!(text.contains("sample=1"), "{text}");
    assert!(text.contains("tasks unavailable:"), "{text}");
    // Exercise the existing bounded proc reader against this test process only.
    // Access restrictions remain printable diagnostics, never a test prerequisite.
    let pid = std::process::id();
    bytes[8..16].copy_from_slice(&u64::from(pid).to_ne_bytes());
    std::fs::write(dir.path().join("storm-diagnostic.bin"), &bytes).unwrap();
    let live = capture_storm(dir.path());
    assert!(
        live.contains(&format!("/proc/{pid}/task/{pid}/syscall ")),
        "{live}"
    );
    std::fs::write(dir.path().join("storm-diagnostic.bin"), [0; 7]).unwrap();
    capture_storm(dir.path());
    let text = std::fs::read_to_string(dir.path().join("storm-diagnostic.txt")).unwrap();
    assert!(text.contains("partial file length=7"), "{text}");
    // A failed diagnostic write still returns printable evidence and cannot
    // replace the original process-status assertion with a filesystem panic.
    std::fs::remove_file(dir.path().join("storm-diagnostic.txt")).unwrap();
    std::fs::create_dir(dir.path().join("storm-diagnostic.txt")).unwrap();
    assert!(capture_storm(dir.path()).contains("partial file length=7"));
}

#[test]
fn storm_failure_diagnostics_survive_checker_scratch_cleanup() {
    const CHILD: &str = "SEEDBOX_STORM_RETENTION_TEST_CHILD";
    if std::env::var_os(CHILD).is_some() {
        // A synthetic failing supervisor completes immediately. No storm or
        // timeout is induced merely to exercise the failure/retention path.
        run("storm", &[], Profile::Native);
        panic!("synthetic failure must fail the original status assertion");
    }
    let test = "unix_cases::signal_models_cases::signal_runner::storm_failure_diagnostics_survive_checker_scratch_cleanup";
    for mode in ["checker", "direct", "missing", "bad-report"] {
        let owner = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir_in(owner.path()).unwrap();
        let reports = owner.path().join("reports");
        std::fs::create_dir(&reports).unwrap();
        let bin = owner.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let supervisor = bin.join("unix-supervisor");
        let diagnostic = match mode {
            "missing" => "",
            _ => "printf partial > storm-diagnostic.bin\n",
        };
        seedbox_test::process::install_executable(
            &supervisor,
            format!("#!/bin/sh\nprintf synthetic-stdout\nprintf synthetic-stderr >&2\n{diagnostic}exit 7\n").as_bytes(),
            0o700,
        )
        .unwrap();
        let stdout = owner.path().join("child-stdout");
        let stderr = owner.path().join("child-stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", test, "--nocapture"])
            .env(CHILD, "1")
            .env("SEEDBOX_DYNAMORIO_FIXTURES", owner.path())
            .env("TMPDIR", scratch.path())
            .env_remove("SEEDBOX_TEST_REPORT_DIR")
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if mode == "bad-report" {
            command.env("SEEDBOX_TEST_REPORT_DIR", &supervisor); // Not a directory.
        } else if mode != "direct" {
            command.env("SEEDBOX_TEST_REPORT_DIR", &reports);
        }
        let mut child = crate::Running(command.guarded_spawn().unwrap());
        let status = HangGuard::new("synthetic storm failure", Duration::from_secs(30))
            .wait_for(|| child.0.try_wait().transpose())
            .unwrap()
            .unwrap();
        assert!(!status.success(), "{mode}: original failure must survive");
        let output = format!(
            "{}{}",
            std::fs::read_to_string(stdout).unwrap(),
            std::fs::read_to_string(stderr).unwrap()
        );
        assert!(
            output.contains("storm [] profile=native/direct-release: Ok(Ok(ExitStatus"),
            "{mode}: {output}"
        );
        assert!(output.contains("synthetic-stdout"), "{mode}: {output}");
        assert!(output.contains("synthetic-stderr"), "{mode}: {output}");
        let expected = if mode == "missing" {
            "unavailable:"
        } else {
            "partial file length=7"
        };
        assert!(output.contains(expected), "{mode}: {output}");
        let retained = output
            .split("retained=Some(\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        let retained = PathBuf::from(retained);
        assert!(retained.is_dir(), "{mode}: {output}");
        if mode == "direct" || mode == "bad-report" {
            assert!(retained.starts_with(scratch.path()), "{output}");
        } else {
            drop(scratch); // The checker's Temporary parent has the same lifetime.
            assert!(retained.starts_with(&reports), "{output}");
            assert_eq!(
                std::fs::read_to_string(retained.join("stdout")).unwrap(),
                "synthetic-stdout"
            );
            assert!(
                std::fs::read_to_string(retained.join("storm-diagnostic.txt"))
                    .unwrap()
                    .contains(expected)
            );
        }
        if mode == "bad-report" {
            assert!(output.contains("signal model report directory"), "{output}");
        }
    }
}
