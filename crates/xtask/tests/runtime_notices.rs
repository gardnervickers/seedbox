//! Exercise the actual build-time notice reader without extracting source trees.
#![cfg(unix)]
mod support;

use std::{fs, os::unix::fs::symlink, path::Path, process::Command};
use support::{diagnostic, notice_components, notices, success};

fn source_archive(sources: &Path, component: &str, kind: &str) {
    let fixture = tempfile::tempdir().unwrap();
    let name = format!("{component}-fixture");
    let tree = fixture.path().join(&name);
    fs::create_dir(&tree).unwrap();
    let member = tree.join(if kind == "missing" {
        "README"
    } else {
        "COPYING"
    });
    if kind == "symlink" {
        symlink("/outside", member).unwrap();
    } else {
        fs::write(
            member,
            format!("Synthetic {component} parser-test notice; no upstream code.\n"),
        )
        .unwrap();
    }
    success(
        Command::new("tar")
            .arg("-cf")
            .arg(sources.join(format!("{component}.source")))
            .arg("-C")
            .arg(fixture.path())
            .arg(name),
        30,
    );
}

#[test]
fn actual_notice_bytes_are_retained() {
    let directory = tempfile::tempdir().unwrap();
    let components = notice_components();
    assert!(!components.is_empty());
    for component in &components {
        source_archive(directory.path(), component, "file");
    }
    let output = notices(directory.path(), None);
    assert!(output.status.success(), "{}", diagnostic(&output));
    for component in components {
        let expected = format!("Synthetic {component} parser-test notice");
        assert!(
            output
                .stdout
                .windows(expected.len())
                .any(|b| b == expected.as_bytes()),
            "{component}: {}",
            diagnostic(&output)
        );
    }
}

#[test]
fn missing_or_linked_notice_is_rejected_without_extraction() {
    for kind in ["missing", "symlink"] {
        let directory = tempfile::tempdir().unwrap();
        source_archive(directory.path(), "musl", kind);
        let output = notices(directory.path(), Some("musl"));
        assert!(!output.status.success(), "{kind}: {}", diagnostic(&output));
        let stderr = String::from_utf8_lossy(&output.stderr);
        // Preserve the old ValueError class check as well as its message.
        assert!(
            stderr.contains("ValueError: musl: missing or invalid actual upstream notice"),
            "{kind}: {stderr}"
        );
        let files: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect();
        assert_eq!(
            files,
            ["musl.source"],
            "{kind}: no archived paths may be extracted"
        );
    }
}
