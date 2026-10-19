//! External tests of the packaged translator, not of Machine or DR internals.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]

use seedbox_test::process::Guarded;
use seedbox_test_time::HangGuard;
use std::{
    fs::File,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    time::Duration,
};

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        // A finished child has already been reaped by try_wait; Child::kill
        // then returns InvalidInput without signalling a potentially reused PID.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run(mode: &str, translated: bool, client: bool) {
    let fixtures = PathBuf::from(
        std::env::var_os("SEEDBOX_DYNAMORIO_FIXTURES")
            .expect("set SEEDBOX_DYNAMORIO_FIXTURES to the dynamorioTestFixtures Nix output"),
    );
    let fixtures = fixtures.canonicalize().expect("prepared fixture directory");
    let application = fixtures.join("bin/application");
    let mut command = if translated {
        let mut command = Command::new(fixtures.join("runtime/bin64/drrun"));
        // The normal Seedbox package contains one release/bitness, not the
        // optional debug/32-bit SDK layout. Select its actual core explicitly.
        command
            .arg("-use_dll")
            .arg(fixtures.join("runtime/lib64/release/libdynamorio.so"));
        if client {
            command.arg("-c").arg(fixtures.join("lib/libpreserve.so"));
        }
        command.arg("--").arg(application);
        command
    } else {
        Command::new(application)
    };
    // Files cannot fill a pipe and deadlock the positive completion wait.
    // These fixtures never fork; the one owned child is killed/reaped on unwind.
    let logs = tempfile::tempdir().unwrap();
    let stdout = logs.path().join("stdout");
    let stderr = logs.path().join("stderr");
    command
        .arg(mode)
        .env_clear()
        .env("DYNAMORIO_CONFIGDIR", logs.path())
        .current_dir(logs.path())
        .stdin(Stdio::null())
        .stdout(File::create(&stdout).unwrap())
        .stderr(File::create(&stderr).unwrap());
    let mut child = Running(
        command
            .guarded_spawn()
            .expect("start DR/application fixture"),
    );
    let guard = HangGuard::new(
        "DR/application fixture must exit (room for a loaded hosted runner)",
        Duration::from_secs(60),
    );
    let completed = guard.wait_for(|| child.0.try_wait().transpose());
    // Drop also cleans up a hung child before reading its diagnostics.
    drop(child);
    let out = std::fs::read_to_string(stdout).unwrap();
    let err = std::fs::read_to_string(stderr).unwrap();
    let status: ExitStatus = completed
        .unwrap_or_else(|error| {
            panic!("{mode} translated={translated} client={client}: {error}\n{out}\n{err}")
        })
        .expect("wait for fixture");
    assert!(status.success(), "{mode}: {status}\n{out}\n{err}");
    let expected = match mode {
        "signal" => "signal: context, vector, mask, alternate stack restored twice\n",
        "registers" => "registers: live values and flags preserved\n",
        "exclusive" => "exclusive: stored value and load/base overlap preserved\n",
        _ => unreachable!(),
    };
    assert_eq!(
        out, expected,
        "{mode} translated={translated} client={client}"
    );
    if client {
        let count = err
            .strip_prefix("dr-client: executed=")
            .and_then(|value| value.strip_suffix('\n'))
            .and_then(|value| value.parse::<u64>().ok());
        assert!(
            count.is_some_and(|n| n > 0),
            "client must execute instrumentation: {err}"
        );
    } else {
        assert!(err.is_empty(), "unexpected fixture/DR diagnostic: {err}");
    }
}

#[test]
fn signal_return_restores_context_vector_mask_and_alternate_stack() {
    run("signal", false, false);
    run("signal", true, false);
}

#[test]
fn public_client_preserves_signal_return_context_and_alternate_stack() {
    run("signal", false, false);
    run("signal", true, true);
}

#[test]
fn public_client_preserves_live_registers_and_flags() {
    run("registers", false, false);
    run("registers", true, false);
    run("registers", true, true);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn exclusive_instructions_preserve_stored_values_and_overlapping_load_base() {
    // Native Linux semantics only: this cannot validate the guest epoch ABI.
    run("exclusive", false, false);
    run("exclusive", true, false);
    run("exclusive", true, true);
}

#[path = "dynamorio/translator_cases.rs"]
mod translator_cases;

#[path = "dynamorio/unix_cases.rs"]
mod unix_cases;
