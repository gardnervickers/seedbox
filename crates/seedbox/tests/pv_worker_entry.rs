//! The normal executable reaches the library worker before CLI initialization.
#![cfg(any(
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ),
    all(target_os = "macos", target_arch = "aarch64")
))]
mod support;
use support::worker_entry as cases;

#[test]
fn private_worker_eof_and_trailing_args_do_not_enter_cli() {
    cases::eof_and_trailing_arguments(std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")));
}
#[test]
fn private_worker_malformed_protocol_exits_without_state() {
    cases::malformed_streams(std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")));
}
#[test]
fn private_worker_preboot_rejections_are_framed_without_native_setup() {
    cases::preboot_requests(std::path::Path::new(env!("CARGO_BIN_EXE_seedbox")));
}
