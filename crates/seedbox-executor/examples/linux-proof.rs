//! Libc-free, opt-in diagnostic fixture. See `proof/mod.rs` for its boundary.
#![cfg_attr(target_os = "linux", no_std)]
#![cfg_attr(target_os = "linux", no_main)]
#[cfg(target_os = "linux")]
extern crate alloc;
#[cfg(target_os = "linux")]
mod proof;
#[cfg(not(target_os = "linux"))]
fn main() {
    panic!("linux-proof must be built and run as a Linux ELF");
}
