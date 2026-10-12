//! Shared seeded inputs for the TCG and PV examples.

use seedbox_machine::rng::{RngStream, RngStreamExt};

pub const SEED: u64 = 7;

/// Adapt the existing deterministic generator to the byte-stream interface.
/// Retain unused bytes so splitting a read does not change the stream.
struct ExampleRng {
    inner: seedbox_engine_core::rng::Rng,
    bytes: [u8; 8],
    offset: usize,
}

impl RngStream for ExampleRng {
    fn fill_bytes(&mut self, buffer: &mut [u8]) {
        for byte in buffer {
            if self.offset == self.bytes.len() {
                self.bytes = self.inner.next().to_le_bytes();
                self.offset = 0;
            }
            *byte = self.bytes[self.offset];
            self.offset += 1;
        }
    }
}

impl RngStreamExt for ExampleRng {}

pub fn sleeping_guests() -> [(&'static str, u32); 2] {
    let mut rng = ExampleRng {
        inner: seedbox_engine_core::rng::Rng::new(SEED, "example/sleep"),
        bytes: [0; 8],
        offset: 8,
    };
    // Four possible values divide the u32 range exactly: no modulo bias.
    ["a", "b"].map(|name| (name, 1 + (rng.next_u32() & 3)))
}
