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
    let logs = tempfile::tempdir().unwrap();
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
        capture_storm(logs.path());
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
        "{binary} {args:?}: {status:?} retained={retained:?}\n{out}\n{err}"
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

// The C schema is also the decoder's field list: no second layout to drift.
// Two bounded reads are explicitly partial, not a cross-process seqlock claim.
fn capture_storm(dir: &std::path::Path) {
    use std::io::Read;
    let names: Vec<_> = include_str!("storm_diagnostics.h")
        .lines()
        .filter_map(|line| line.trim().strip_prefix("D(")?.split_once(')').map(|v| v.0))
        .collect();
    let mut report = String::from("storm diagnostic: partial native-endian snapshots; roles=TID:source-line; no atomic whole-record guarantee\n");
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
    if let Err(error) = std::fs::write(dir.join("storm-diagnostic.txt"), report) {
        eprintln!("storm diagnostic write failed: {error}");
    }
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
    let bytes: Vec<_> = words.into_iter().flat_map(u64::to_ne_bytes).collect();
    std::fs::write(dir.path().join("storm-diagnostic.bin"), bytes).unwrap();
    capture_storm(dir.path());
    let text = std::fs::read_to_string(dir.path().join("storm-diagnostic.txt")).unwrap();
    assert!(text.contains("main_role=123:456"), "{text}");
    assert!(text.contains("controller_seq=3 predicate=254"), "{text}");
    assert!(text.contains("sample=1"), "{text}");
    assert!(text.contains("tasks unavailable:"), "{text}");
    std::fs::write(dir.path().join("storm-diagnostic.bin"), [0; 7]).unwrap();
    capture_storm(dir.path());
    let text = std::fs::read_to_string(dir.path().join("storm-diagnostic.txt")).unwrap();
    assert!(text.contains("partial file length=7"), "{text}");
}
