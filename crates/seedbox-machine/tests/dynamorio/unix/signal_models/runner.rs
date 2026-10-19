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
    if !matches!(status, Ok(Ok(_))) {
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        let _ = child.wait();
    }
    let out = std::fs::read_to_string(stdout).unwrap();
    let err = std::fs::read_to_string(stderr).unwrap();
    assert!(
        status
            .as_ref()
            .is_ok_and(|s| s.as_ref().is_ok_and(|s| s.success())),
        "{binary} {args:?}: {status:?}\n{out}\n{err}"
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
