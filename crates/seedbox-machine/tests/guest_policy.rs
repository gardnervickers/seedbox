//! Host-only tests of the actual shared init's named-controller parser.
#![cfg(unix)]
use std::{path::Path, process::Command};
#[test]
fn explicit_controller_overrides_environment_without_changing_legacy_fallback() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("policy-parser");
    let output = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(root.join("src/pv"))
        .arg(root.join("tests/fixtures/guest-policy.c"))
        .arg("-o")
        .arg(&binary)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
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
            .output()
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
        let o = Command::new(&binary).args(["1", command]).output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{command:?}");
        assert_eq!(o.stdout, b"invalid\n");
    }
}
