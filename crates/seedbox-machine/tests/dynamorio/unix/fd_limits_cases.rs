//! Derived controls for newly detected FD-limit port defects, not original
//! source-case completion. O_TRUNC keeps its native side-effect oracle.
use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{fs::File, path::PathBuf, process::Command, time::Duration};

#[derive(Clone, Copy, Debug)]
enum Profile {
    Native,
    Release,
    Debug,
}

fn run(profile: Profile, binary: &str, args: &[&str]) -> String {
    let debug = matches!(profile, Profile::Debug);
    let key = if debug {
        "SEEDBOX_DYNAMORIO_DEBUG_FIXTURES"
    } else {
        "SEEDBOX_DYNAMORIO_FIXTURES"
    };
    let root =
        PathBuf::from(std::env::var_os(key).unwrap_or_else(|| {
            panic!("set {key} to prepared matching application/runtime fixtures")
        }));
    let axis = format!("{profile:?} binary={binary} args={args:?}");
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
        command.arg("--");
    }
    command
        .arg(root.join(format!("bin/unix-fd-limits-{binary}")))
        .args(args)
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(std::process::Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = command
        .guarded_spawn()
        .unwrap_or_else(|error| panic!("{axis}: start fixture supervisor: {error}"));
    let guard = HangGuard::new("FD-limit positive completion", Duration::from_secs(30));
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
    out
}

fn truncate(profile: Profile) {
    let out = run(profile, "truncate", &[]);
    assert_eq!(
        out,
        format!(
            "fd-limits truncate: open=-1 errno={} retained_bytes=9 contents-match=1\n",
            libc::EMFILE
        )
    );
}

fn signalfd(profile: Profile) {
    // A64 Linux has only signalfd4; there is no unsupported-as-pass old case.
    #[cfg(target_arch = "x86_64")]
    let forms = ["old", "new"];
    #[cfg(target_arch = "aarch64")]
    let forms = ["new"];
    for form in forms {
        for mode in ["update", "create"] {
            let out = run(profile, "signalfd", &[form, mode]);
            assert_eq!(
                out,
                format!(
                    "fd-limits signalfd: {form} {mode} fd50 flags and signal delivery preserved\n"
                )
            );
        }
    }
}

#[test]
fn fd_limits_truncate_native_preserves_file_on_emfile() {
    truncate(Profile::Native);
}
#[test]
fn fd_limits_truncate_release_preserves_file_on_emfile() {
    truncate(Profile::Release);
}
#[test]
fn fd_limits_truncate_debug_preserves_file_on_emfile() {
    truncate(Profile::Debug);
}
#[test]
fn fd_limits_signalfd_native_preserves_existing_fd() {
    signalfd(Profile::Native);
}
#[test]
fn fd_limits_signalfd_release_preserves_existing_fd() {
    signalfd(Profile::Release);
}
#[test]
fn fd_limits_signalfd_debug_preserves_existing_fd() {
    signalfd(Profile::Debug);
}
