//! Tracks the passage of simulation time within the seedbox execution environment.
//!
//! Provides a `Clock` trait for managing and querying simulation time.

pub trait Clock {
    /// Returns the current simulation time.
    fn now(&self) -> u64;

    /// Advances the simulation time by the specified amount.
    fn advance(&mut self, delta: u64);
}
