//! The ARM guest kernel's syscall charging, compiled on the host against a
//! model of the stopped worker's PV page and rendezvous.
#![cfg(unix)]
use seedbox_test::process::Guarded;
use std::{path::Path, process::Command};

/// The kernel file's PV page, clock read and charge helpers, without the
/// clocksource and clockevent registration that needs real kernel headers.
fn charge_source() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("guest/kernel/arm64/seedbox.c");
    let source = std::fs::read_to_string(path).unwrap();
    let start = source
        .find("#define TIME_IPA")
        .expect("PV page definitions");
    let end = source
        .find("static u64 cs_read")
        .expect("clocksource callbacks");
    source[start..end].to_owned()
}

#[test]
fn arm_syscall_charges_follow_the_syscall_kind() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("charge.c"), charge_source()).unwrap();
    let executable = directory.path().join("charge-test");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(directory.path())
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/guest_kernel_charges/charge_test.c"))
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    let output = Command::new(executable).guarded_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}
