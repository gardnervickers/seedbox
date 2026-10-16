//! Core traits for the seedbox machine.

pub mod application;
pub mod clock;
pub mod definition;
pub mod execution;
pub mod machine;
pub mod pv;
pub mod rng;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// Concrete-backend snapshot quotas and closed wire primitives.
// TODO: Re-evaluate: backend and shared bridge codecs must share one budget.
pub mod snapshot;

#[cfg(unix)]
pub mod process_lease;
