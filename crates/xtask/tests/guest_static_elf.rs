//! Exercise the build's real ELF checker with deterministic readelf failures.
#![cfg(unix)]
mod support;

use std::{fs, process::Command};
use support::{diagnostic, root, run};

struct Fixture(tempfile::TempDir);
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let readelf = directory.path().join("readelf");
        seedbox_test::process::install_executable(
            &readelf,
            br##"#!/bin/sh
[ "$#" = 3 ] && [ "$2" = -- ] && [ "$3" = "$EXPECTED_BINARY" ] || exit 90
printf '%s\n' "$1" >> "$TRACE"
if [ "$1" = "$FAIL_MODE" ]; then
  echo "readelf failed at $1" >&2
  exit 42
fi
case "$1" in
  -h) printf '  Class: %s\n  Machine: %s\n' "$ELF_CLASS" "$ELF_MACHINE" ;;
  -l) printf '%s\n' "$PROGRAM_HEADERS" ;;
  -d) printf '%s\n' "$DYNAMIC_SECTION" ;;
  *) exit 91 ;;
esac
"##,
            0o755,
        )
        .unwrap();
        Self(directory)
    }

    fn command(&self, binary: &str, machine: &str) -> Command {
        let parent_path = std::env::var_os("PATH").unwrap();
        let path = std::env::join_paths(
            std::iter::once(self.0.path().to_owned()).chain(std::env::split_paths(&parent_path)),
        )
        .unwrap();
        let mut command = Command::new("bash");
        command
            .arg(root().join("crates/seedbox-machine/guest-static-elf.sh"))
            .args([binary, machine])
            .env("PATH", path)
            .env("TRACE", self.0.path().join("trace"))
            .env("EXPECTED_BINARY", binary)
            .env("ELF_CLASS", "ELF64")
            .env("ELF_MACHINE", machine)
            .env("PROGRAM_HEADERS", "LOAD")
            .env(
                "DYNAMIC_SECTION",
                "There is no dynamic section in this file.",
            )
            .env("FAIL_MODE", "");
        command
    }

    fn trace(&self) -> String {
        fs::read_to_string(self.0.path().join("trace")).unwrap()
    }
}

#[test]
fn static_elf64_helpers_pass_for_both_guest_targets() {
    for machine in ["X86-64", "AArch64"] {
        for binary in ["init", "application with spaces"] {
            let fixture = Fixture::new();
            let output = run(&mut fixture.command(binary, machine), 10);
            assert!(output.status.success(), "{}", diagnostic(&output));
            assert_eq!(fixture.trace(), "-h\n-l\n-d\n");
        }
    }
}

#[test]
fn wrong_machine_or_class_fails_before_linkage_checks() {
    for (field, value) in [("ELF_MACHINE", "AArch64"), ("ELF_CLASS", "ELF32")] {
        let fixture = Fixture::new();
        let output = run(fixture.command("init", "X86-64").env(field, value), 10);
        assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("expected ELF64 for X86-64"));
        assert_eq!(fixture.trace(), "-h\n");
    }
}

#[test]
fn interpreter_segment_is_rejected_for_init_and_application() {
    for binary in ["init", "application"] {
        let fixture = Fixture::new();
        let output = run(
            fixture
                .command(binary, "X86-64")
                .env("PROGRAM_HEADERS", "LOAD\n  INTERP 0x000238"),
            10,
        );
        assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("must not contain INTERP"));
        assert_eq!(fixture.trace(), "-h\n-l\n");
    }
}

#[test]
fn needed_libraries_are_rejected_for_init_and_application() {
    for binary in ["init", "application"] {
        let fixture = Fixture::new();
        let output = run(
            fixture
                .command(binary, "AArch64")
                .env("DYNAMIC_SECTION", "0x000001 (NEEDED) libc.so"),
            10,
        );
        assert_eq!(output.status.code(), Some(1), "{}", diagnostic(&output));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("must not contain NEEDED"));
        assert_eq!(fixture.trace(), "-h\n-l\n-d\n");
    }
}

#[test]
fn readelf_errors_are_not_mistaken_for_absent_sections() {
    for (mode, trace) in [("-h", "-h\n"), ("-l", "-h\n-l\n"), ("-d", "-h\n-l\n-d\n")] {
        let fixture = Fixture::new();
        let output = run(fixture.command("init", "X86-64").env("FAIL_MODE", mode), 10);
        assert_eq!(output.status.code(), Some(42), "{}", diagnostic(&output));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains(&format!("readelf failed at {mode}")));
        assert_eq!(fixture.trace(), trace);
    }
}

#[test]
fn usage_is_rejected_before_readelf_and_both_build_outputs_use_the_checker() {
    let script = root().join("crates/seedbox-machine/guest-static-elf.sh");
    for arguments in [vec![], vec!["init"], vec!["init", "X86-64", "extra"]] {
        let output = run(Command::new("bash").arg(&script).args(arguments), 10);
        assert_eq!(output.status.code(), Some(2), "{}", diagnostic(&output));
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("usage: guest-static-elf.sh"));
    }
    let recipe = fs::read_to_string(root().join("crates/seedbox-machine/guest.nix")).unwrap();
    for binary in ["init", "application"] {
        assert!(recipe.contains(&format!(
            "bash ${{./guest-static-elf.sh}} {binary} '${{machine}}'"
        )));
    }
    assert!(!recipe.contains("! readelf"));
}
