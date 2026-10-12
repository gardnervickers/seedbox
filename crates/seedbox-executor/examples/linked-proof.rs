//! Opt-in task-bound ARM linked-path experiment; never a runtime selector.
#![cfg_attr(all(target_os = "linux", target_arch = "aarch64"), no_std)]
#![cfg_attr(all(target_os = "linux", target_arch = "aarch64"), no_main)]
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
extern crate alloc;
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
mod linked;
#[cfg(not(all(target_os = "linux", target_arch = "aarch64")))]
fn main() {
    panic!("linked-proof requires Linux AArch64");
}
