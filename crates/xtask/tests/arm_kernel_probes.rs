//! Compile actual ARM PV kernel helpers against host mocks; never build or boot a guest.
#![cfg(unix)]
#[path = "support/cc_words.rs"]
mod cc_words;
mod support;

use std::{fs, path::PathBuf, process::Command};
use support::{diagnostic, root, run, success};

// Preserve the original source boundaries: these are the production helpers,
// not a second implementation of their time or interrupt policy.
fn section<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let start = source.find(start).expect("kernel helper start marker");
    let end = source.find(end).expect("kernel helper end marker");
    &source[start..end]
}

fn compile(directory: &std::path::Path, source: &str, optimize: bool) -> PathBuf {
    let input = directory.join("probe.c");
    let executable = directory.join("probe");
    fs::write(&input, source).unwrap();
    let mut compiler = cc_words::split_compiler();
    compiler.args(["-Wall", "-Wextra", "-Werror"]);
    if optimize {
        compiler.arg("-O2");
    }
    success(compiler.arg(input).arg("-o").arg(&executable), 120);
    executable
}

#[test]
fn hrtimer_spin_rejects_low_resolution_mode() {
    let source = fs::read_to_string(root().join("kernel-arm64/init.c")).unwrap();
    let helper = section(
        &source,
        "static void require_high_resolution(void)",
        "static void busy_wait_check(void)",
    );
    let fixture = [
        include_str!("fixtures/arm-kernel/resolution-prefix.c"),
        helper,
        include_str!("fixtures/arm-kernel/resolution-suffix.c"),
    ]
    .concat();
    let directory = tempfile::tempdir().unwrap();
    let executable = compile(directory.path(), &fixture, false);
    for (resolution, status, expected) in [
        ("1", 0, ""),
        ("10000000", 2, "PV high-resolution timer mode not active"),
        ("0", 2, "PV high-resolution timer mode not active"),
        ("-1", 2, "clock_getres"),
    ] {
        let output = run(Command::new(&executable).arg(resolution), 10);
        assert_eq!(
            output.status.code(),
            Some(status),
            "{}",
            diagnostic(&output)
        );
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(
            stderr.contains(expected),
            "resolution {resolution}: missing {expected:?} in {stderr:?}"
        );
    }
    drop(directory);
    assert!(!executable.exists());
}

#[test]
fn idle_rendezvous_unmasks_and_restores_irq_state() {
    let source = fs::read_to_string(root().join("kernel-arm64/seedbox.c")).unwrap();
    let helper = section(
        &source,
        "void seedbox_idle(void)",
        "void __init seedbox_time_init(void)",
    );
    let fixture = [
        include_str!("fixtures/arm-kernel/idle-prefix.c"),
        helper,
        include_str!("fixtures/arm-kernel/idle-suffix.c"),
    ]
    .concat();
    let directory = tempfile::tempdir().unwrap();
    let executable = compile(directory.path(), &fixture, false);
    success(&mut Command::new(&executable), 10);
    drop(directory);
    assert!(!executable.exists());
}

#[test]
fn poll_yield_preserves_caller_mask_and_retries_after_irq() {
    let source = fs::read_to_string(root().join("kernel-arm64/seedbox.c")).unwrap();
    let helper = section(
        &source,
        "u64 notrace seedbox_read(void)",
        "/* Charge syscall work",
    );
    let fixture = [
        include_str!("fixtures/arm-kernel/poll-prefix.c"),
        helper,
        include_str!("fixtures/arm-kernel/poll-suffix.c"),
    ]
    .concat();
    let directory = tempfile::tempdir().unwrap();
    let executable = compile(directory.path(), &fixture, false);
    success(&mut Command::new(&executable), 10);
    drop(directory);
    assert!(!executable.exists());
}

#[test]
fn syscall_charge_scales_and_survives_mid_charge_throttle_and_heal() {
    let source = fs::read_to_string(root().join("kernel-arm64/seedbox.c")).unwrap();
    let page = section(
        &source,
        "struct seedbox_page {",
        "static struct seedbox_page *page;",
    );
    let helpers = section(
        &source,
        "u64 notrace seedbox_read(void)",
        "static u64 cs_read",
    );
    let fixture = [
        include_str!("fixtures/arm-kernel/syscall-prefix.c"),
        page,
        include_str!("fixtures/arm-kernel/syscall-middle.c"),
        helpers,
        include_str!("fixtures/arm-kernel/syscall-suffix.c"),
    ]
    .concat();
    let directory = tempfile::tempdir().unwrap();
    let executable = compile(directory.path(), &fixture, true);
    success(&mut Command::new(&executable), 10);
    drop(directory);
    assert!(!executable.exists());
}
