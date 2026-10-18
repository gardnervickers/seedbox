pub use seedbox_machine::clock::{Clock, ClockRate};
use std::sync::atomic::{AtomicU64, Ordering};
mod projection;
pub(crate) use projection::Projection;
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

/// One machine's view of a fixed barrier, already mapped to its time base.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixedClock(pub(crate) u64);

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}
