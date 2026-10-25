//! The guest kernels' syscall charging, compiled on the host against a model
//! of the stopped worker's PV page and rendezvous.
#![cfg(unix)]
use seedbox_test::process::Guarded;
use std::{path::Path, process::Command};

fn kernel_source(arch: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("guest/kernel")
        .join(arch)
        .join("seedbox.c");
    std::fs::read_to_string(path).unwrap()
}

/// `source` from `start` up to `end`, both markers that must exist.
fn between<'a>(source: &'a str, start: &str, end: &str) -> &'a str {
    let from = source.find(start).unwrap_or_else(|| panic!("{start}"));
    let to = from + source[from..].find(end).unwrap_or_else(|| panic!("{end}"));
    &source[from..to]
}

/// Compile `fixture` with `charge.c` (the kernel slice) beside it, and run it.
fn run(fixture: &str, slice: String) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("charge.c"), slice).unwrap();
    let executable = directory.path().join("charge-test");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=gnu11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(directory.path())
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/guest_kernel_charges")
                .join(fixture),
        )
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    let output = Command::new(executable).guarded_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn arm_syscall_charges_and_their_tick_accounting_follow_the_syscall_kind() {
    // The PV page, clock read and charge helpers, without the clocksource and
    // clockevent registration that needs real kernel headers.
    let source = kernel_source("arm64");
    run(
        "charge_test.c",
        between(&source, "#define TIME_IPA", "static u64 cs_read").to_owned(),
    );
}

#[test]
fn x86_progress_charges_count_their_ticks_as_user_time() {
    let source = kernel_source("x86_64");
    let slice = [
        between(
            &source,
            "struct seedbox_page {",
            "static __always_inline void event_write",
        ),
        between(
            &source,
            "u64 noinstr seedbox_read(void)",
            "unsigned long long noinstr seedbox_rdtsc",
        ),
        between(
            &source,
            "/* A progress call:",
            "bool seedbox_emulate_user_tsc",
        ),
    ]
    .join("\n");
    run("charge_test_x86.c", slice);
}
