//! Native public-service acceptance through the normal release CLI/runtime
//! package: `kvm-tests` builds the PV cases and `tcg-tests` the TCG cases.
#![cfg(all(
    any(feature = "kvm-tests", feature = "tcg-tests"),
    target_os = "linux",
    target_arch = "x86_64"
))]

// Public so helpers used by only one backend are not dead code when only that
// backend's feature is built.
pub mod scenarios;

mod controller;
mod held_checkpoint;
#[cfg(feature = "kvm-tests")]
mod pv;
#[cfg(feature = "tcg-tests")]
mod tcg;
