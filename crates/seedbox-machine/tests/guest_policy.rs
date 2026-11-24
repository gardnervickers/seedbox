//! Host-only tests of the actual shared init's per-boot kernel argument parsers.
#![cfg(unix)]
use seedbox_test::process::Guarded;
use std::{path::Path, process::Command};
fn parser(dir: &Path) -> std::path::PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let binary = dir.join("policy-parser");
    let output = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(root.join("src/pv"))
        .arg(root.join("tests/fixtures/guest-policy.c"))
        .arg("-o")
        .arg(&binary)
        .guarded_output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    binary
}
#[test]
fn explicit_controller_overrides_environment_without_changing_legacy_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let binary = parser(dir.path());
    for (fallback, command, expected) in [
        ("1", "", "1\n"),
        ("0", "quiet unknown=1", "0\n"),
        ("1", "quiet", "1\n"),
        ("1", "quiet seedbox.control=0 epoch=0", "0\n"),
        ("0", "seedbox.control=1", "1\n"),
        ("1", "noseedbox.control=0 seedbox.controlled=0", "1\n"),
        ("0", "\tseedbox.control=1\n ", "1\n"),
    ] {
        let o = Command::new(&binary)
            .args([fallback, command])
            .guarded_output()
            .unwrap();
        assert!(o.status.success(), "{command:?}");
        assert_eq!(o.stdout, expected.as_bytes(), "{command:?}");
    }
    for command in [
        "seedbox.control=",
        "seedbox.control=2",
        "seedbox.control=10",
        "seedbox.control=0 seedbox.control=0",
        "seedbox.control=1 seedbox.control=0",
    ] {
        let o = Command::new(&binary)
            .args(["1", command])
            .guarded_output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{command:?}");
        assert_eq!(o.stdout, b"invalid\n");
    }
}
#[test]
fn clock_offset_is_one_signed_decimal_within_a_second_or_absent() {
    let dir = tempfile::tempdir().unwrap();
    let binary = parser(dir.path());
    for (command, expected) in [
        ("", "0\n"),
        ("quiet seedbox.control=1", "0\n"),
        ("seedbox.clock_offset_ns=0", "0\n"),
        ("quiet seedbox.clock_offset_ns=4999999 x=1", "4999999\n"),
        ("seedbox.clock_offset_ns=-5000000", "-5000000\n"),
        ("seedbox.clock_offset_ns=1000000000", "1000000000\n"),
        ("seedbox.clock_offset_ns=-1000000000", "-1000000000\n"),
        (
            "noseedbox.clock_offset_ns=7 seedbox.clock_offset_nsx=7",
            "0\n",
        ),
        ("\tseedbox.clock_offset_ns=-3\n", "-3\n"),
    ] {
        let o = Command::new(&binary)
            .args(["offset", command])
            .guarded_output()
            .unwrap();
        assert!(o.status.success(), "{command:?}");
        assert_eq!(o.stdout, expected.as_bytes(), "{command:?}");
    }
    for command in [
        "seedbox.clock_offset_ns=",
        "seedbox.clock_offset_ns=-",
        "seedbox.clock_offset_ns=+5",
        "seedbox.clock_offset_ns=5ms",
        "seedbox.clock_offset_ns=1000000001",
        "seedbox.clock_offset_ns=-99999999999999999999",
        "seedbox.clock_offset_ns=1 seedbox.clock_offset_ns=1",
    ] {
        let o = Command::new(&binary)
            .args(["offset", command])
            .guarded_output()
            .unwrap();
        assert_eq!(o.status.code(), Some(2), "{command:?}");
        assert_eq!(o.stdout, b"invalid\n");
    }
}
