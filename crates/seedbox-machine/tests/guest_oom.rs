//! Kernel log fixtures exercise the production C parser and Rust wire decoder.
#![cfg(unix)]
use seedbox_test::process::Guarded;
use std::{path::Path, process::Command};
#[test]
fn kernel_oom_records_only_and_exact_victim_details() {
    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("guest-oom-test");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pv/guest_oom_test.c");
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let build = Command::new(compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg(source)
        .arg("-o")
        .arg(&executable)
        .guarded_output()
        .unwrap();
    assert!(build.status.success(), "{build:?}");
    let output = Command::new(executable).guarded_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let kills: Vec<_> = output
        .stdout
        .split(|b| *b == b'\n')
        .filter(|s| !s.is_empty())
        .map(|line| {
            seedbox_engine_protocol::out_of_memory::decode(line)
                .unwrap()
                .unwrap()
        })
        .collect();
    assert_eq!(kills.len(), 3);
    assert_eq!(kills[0].pid, 412);
    assert_eq!(kills[0].command, "nats-server");
    assert_eq!(kills[0].anon_rss_kib, 446228);
    assert!(kills[0].application);
    assert_eq!(kills[1].command, "a (b) c");
    assert!(!kills[1].application);
    assert_eq!(kills[2].command, "\\x0a".repeat(15));
}
