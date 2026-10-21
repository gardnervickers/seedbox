//! Shared seeded inputs for the TCG and PV examples.

use seedbox_machine::rng::{RngStream, RngStreamExt};

pub const SEED: u64 = 7;

pub fn driver_rng() -> impl RngStream + Send + 'static {
    seeded::SeededRng::new(SEED)
}

use seedbox_machine::test_support as seeded;

pub fn sleeping_guests() -> [(&'static str, u32); 2] {
    let mut rng = seeded::SeededRng::new(SEED);
    // Four possible values divide the u32 range exactly: no modulo bias.
    ["a", "b"].map(|name| {
        let sleep = 1 + (rng.next_u32() & 3);
        (name, sleep)
    })
}
