pub use seedbox_machine::rng::{RngStream, RngStreamExt};
mod persist;
mod seeded;
pub(crate) use seeded::{Domain as RngDomain, SeededRng};

/// Only the owned concrete stream can be captured without guessing its state.
pub(crate) enum SimulationRng {
    External(Box<dyn RngStream + Send>),
    Seeded(SeededRng),
}
impl RngStream for SimulationRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        match self {
            Self::External(rng) => rng.fill_bytes(bytes),
            Self::Seeded(rng) => rng.fill_bytes(bytes),
        }
    }
}
