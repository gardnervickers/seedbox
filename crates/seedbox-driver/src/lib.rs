//! Deterministic simulation scheduling and control.
pub use seedbox_machine::{definition, execution, pv};
#[macro_use]
mod persistence;
// TEMPORARY measurement counters (not for merge).
mod clock;
pub mod driver;
mod oracle;
#[doc(hidden)]
pub mod perf;
mod rng;

pub mod machine;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
