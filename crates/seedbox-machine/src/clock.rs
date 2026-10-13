//! Tracks the passage of simulation time within the seedbox execution environment.
//!
//! A read-only view of simulation time owned by the caller.

use std::sync::atomic::{AtomicU64, Ordering};

// Private clock-adjustment policy awaits guest-clock integration.
#[cfg_attr(not(test), allow(dead_code))]
mod projection;

pub trait Clock {
    /// Returns the current simulation time in nanoseconds.
    ///
    /// The owner advances time monotonically at simulation barriers. Machines
    /// observe this clock; their execution progress does not advance it.
    fn now(&self) -> u64;
}

/// Shared simulation time controlled by the driver.
pub(crate) struct SimulationClock(AtomicU64);

impl SimulationClock {
    pub(crate) fn new(nanos: u64) -> Self {
        Self(AtomicU64::new(nanos))
    }

    /// Set the time after the driver has established the scheduling position.
    pub(crate) fn set(&self, nanos: u64) {
        self.0.store(nanos, Ordering::Relaxed);
    }
}

impl Clock for SimulationClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
