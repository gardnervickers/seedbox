//! Finite public fault-context contract. X86 CALL/JMP read their target through
//! NULL; A64 LDR faults before any branch (A64 has no memory-indirect CTI).
//! The A64 case covers app PC restoration, not the x86 copied-CTI source bug.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{fs::File, path::PathBuf, process::Command, time::Duration};

#[derive(Clone, Copy, Debug)]
enum Profile {
    Native,
    Plain,
    DefaultClient,
    StoredClient,
}

fn run(debug: bool, profile: Profile, operation: &str, mapping: &str) {
    let key = if debug {
        "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES"
    } else {
        "SEEDBOX_DYNAMORIO_FIXTURES"
    };
    let root =
        PathBuf::from(std::env::var_os(key).unwrap_or_else(|| {
            panic!("set {key} to prepared matching SDK/client/runtime fixtures")
        }));
    let axis = format!("debug={debug} {profile:?} operation={operation} mapping={mapping}");
    let logs = tempfile::tempdir().unwrap();
    let stdout = logs.path().join("stdout");
    let stderr = logs.path().join("stderr");
    // Use the existing native supervisor, which owns and reaps descendants.
    let mut command = Command::new(root.join("bin/unix-supervisor"));
    if !matches!(profile, Profile::Native) {
        let build = if debug { "debug" } else { "release" };
        command.arg(root.join("runtime/bin64/drrun"));
        if debug {
            command.arg("-debug");
        }
        command
            .arg("-use_dll")
            .arg(root.join(format!("runtime/lib64/{build}/libdynamorio.so")));
        if matches!(profile, Profile::DefaultClient | Profile::StoredClient) {
            command
                .arg("-c")
                .arg(root.join("lib/libunix-fault-context.so"));
            if matches!(profile, Profile::StoredClient) {
                command.arg("store");
            }
        }
        command.arg("--");
    }
    command
        .arg(root.join("bin/unix-fault-context"))
        .args([operation, mapping])
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(std::process::Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = command
        .guarded_spawn()
        .unwrap_or_else(|error| panic!("{axis}: start fixture supervisor: {error}"));
    let guard = HangGuard::new("fault-context positive completion", Duration::from_secs(30));
    let status = guard.wait_for(|| child.try_wait().transpose());
    if !matches!(status, Ok(Ok(_))) {
        // This is still our unreaped child; TERM lets the existing supervisor
        // clean up its process tree before we read the diagnostic files.
        unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
        let _ = child.wait();
    }
    let out = std::fs::read_to_string(stdout).unwrap();
    let err = std::fs::read_to_string(stderr).unwrap();
    assert!(
        status
            .as_ref()
            .is_ok_and(|s| s.as_ref().is_ok_and(|s| s.success())),
        "{axis}: {status:?}\n{out}\n{err}"
    );
    assert_eq!(
        out, "fault-context: signal-match=1 null-address=1 application-pc=1\n",
        "{axis}\n{err}"
    );
    if matches!(profile, Profile::DefaultClient | Profile::StoredClient) {
        let store = u8::from(matches!(profile, Profile::StoredClient));
        assert!(
            err.lines()
                .any(|line| line == format!("fault-context-client: store={store} translating=0")),
            "{axis}: public BB callback must see the faulting app instruction\n{err}"
        );
    }
}

fn matrix(debug: bool) {
    #[cfg(target_arch = "x86_64")]
    let operations = ["call", "jump"];
    #[cfg(target_arch = "aarch64")]
    let operations = ["load"];
    for operation in operations {
        for mapping in ["shared", "private"] {
            // Native is the application control; it needs no duplicate Debug run.
            if !debug {
                run(false, Profile::Native, operation, mapping);
            }
            for profile in [
                Profile::Plain,
                Profile::DefaultClient,
                Profile::StoredClient,
            ] {
                run(debug, profile, operation, mapping);
            }
        }
    }
}

#[test]
fn fault_context_release_preserves_original_application_pc() {
    matrix(false);
}

#[test]
fn fault_context_debug_preserves_original_application_pc() {
    matrix(true);
}
