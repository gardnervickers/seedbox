//! TCG machine implementation.
pub mod machine;

// Test-only paths for the shared clock and RNG fixtures.
#[cfg(test)]
mod clock {
    pub use seedbox_machine::clock::*;
    pub use seedbox_machine::test_support::SimulationClock;
}
#[cfg(test)]
mod rng {
    pub use seedbox_machine::rng::*;
    pub use seedbox_machine::test_support::SeededRng;
}
