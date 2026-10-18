//! The standalone worker retains its private launch and preboot protocol.
#![cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "aarch64")
))]
#[path = "support/worker_entry.rs"]
mod cases;

#[test]
fn private_worker_eof_and_trailing_args_do_not_enter_cli() {
    cases::eof_and_trailing_arguments(std::path::Path::new(env!(
        "CARGO_BIN_EXE_seedbox-pv-worker"
    )));
}
#[test]
fn private_worker_malformed_protocol_exits_without_state() {
    cases::malformed_streams(std::path::Path::new(env!(
        "CARGO_BIN_EXE_seedbox-pv-worker"
    )));
}
#[test]
fn private_worker_preboot_rejections_are_framed_without_native_setup() {
    cases::preboot_requests(std::path::Path::new(env!(
        "CARGO_BIN_EXE_seedbox-pv-worker"
    )));
}

#[test]
fn standalone_requires_its_private_launch_marker() {
    let (status, stdout, stderr) = cases::invoke(
        std::path::Path::new(env!("CARGO_BIN_EXE_seedbox-pv-worker")),
        &[],
        b"",
    );
    assert_eq!(status.code(), Some(2));
    assert!(stdout.is_empty());
    assert_eq!(
        stderr,
        b"private seedbox PV worker requires its protocol launch\n"
    );
}
