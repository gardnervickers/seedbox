//! Core traits for the seedbox machine.

pub mod application;
pub mod clock;
pub mod definition;
pub mod execution;
pub mod machine;
pub mod pv;
pub mod rng;

/// TEMPORARY trace (not for merge): per-thread split of machine runs.
#[doc(hidden)]
pub mod perf_trace {
    use std::cell::Cell;
    thread_local! {
        /// Host nanoseconds spent in the backend's bounded execute call.
        pub static EXEC_NS: Cell<u64> = const { Cell::new(0) };
        /// Number of bounded execute calls.
        pub static EXECS: Cell<u64> = const { Cell::new(0) };
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

/// Concrete-backend snapshot quotas and closed wire primitives.
pub mod snapshot;

#[cfg(unix)]
pub mod process_lease;
