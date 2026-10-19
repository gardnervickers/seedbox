//! External DR cases requiring explicitly delegated namespace/pids fixtures.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[path = "dynamorio/unix/isolated_cases.rs"]
mod isolated_cases;

use seedbox_test::process::Guarded;
use std::{path::PathBuf, process::Command};

fn preflight(mode: &str) {
    let fixtures = PathBuf::from(
        std::env::var_os("SEEDBOX_DYNAMORIO_FIXTURES")
            .expect("set SEEDBOX_DYNAMORIO_FIXTURES to prepared fixtures"),
    );
    let selected = if mode == "pid1" {
        std::env::var_os("SEEDBOX_DYNAMORIO_PID1_LAUNCHER").map(PathBuf::from)
    } else {
        None
    };
    let mut command =
        Command::new(selected.unwrap_or_else(|| fixtures.join("bin/seedbox-dr-isolate")));
    command.arg("preflight").arg(mode).env_clear();
    if let Some(root) = std::env::var_os("SEEDBOX_DYNAMORIO_CGROUP_ROOT") {
        command.env("SEEDBOX_DYNAMORIO_CGROUP_ROOT", root);
    }
    let output = command
        .guarded_output()
        .expect("start scoped capability preflight");
    assert!(
        output.status.success(),
        "{mode} setup failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains(&format!("isolate: preflight {mode}")));
}

#[test]
fn namespace_fixture_executes_as_actual_pid1() {
    preflight("pid1");
}

#[test]
fn process_limit_fixture_requires_an_owned_delegation() {
    preflight("pids");
}
