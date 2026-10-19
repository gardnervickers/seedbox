//! External DR cases requiring explicitly delegated namespace/pids fixtures.
#![cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
// The Unix owner's frozen isolated_cases module is wired here at integration.
