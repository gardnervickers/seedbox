//! Deterministic caller-owned entropy for shared machine specifications.
use crate::rng::RngStream;
use sha2::{Digest, Sha256};
/// Machine-domain fixture stream with the original seed/block/cursor byte format.
/// It owns no fault or network domains and is not a production RNG policy.
// TODO: Re-evaluate: shared specifications need their historical input bytes.
// A Driver dependency would reverse the graph; equivalence is checked in Driver.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SeededRng {
    #[serde(default)]
    domain: MachineDomain,
    seed: u64,
    block: u64,
    bytes: [u8; 32],
    position: u8,
}
#[derive(Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
enum MachineDomain {
    #[default]
    Machine,
}
impl SeededRng {
    /// Start at the original machine-domain byte cursor for `seed`.
    pub fn new(seed: u64) -> Self {
        Self {
            domain: MachineDomain::Machine,
            seed,
            block: 0,
            bytes: [0; 32],
            position: 32,
        }
    }
}
impl crate::rng::RngStreamExt for SeededRng {}
impl RngStream for SeededRng {
    fn fill_bytes(&mut self, mut out: &mut [u8]) {
        while !out.is_empty() {
            assert!(self.position <= 32, "invalid seeded RNG snapshot cursor");
            if self.position == 32 {
                let next = self
                    .block
                    .checked_add(1)
                    .expect("seeded RNG exhausted its u64 block counter");
                let mut hash = Sha256::new();
                hash.update(b"seedbox-machine seeded bytes v1\0");
                hash.update(self.seed.to_le_bytes());
                hash.update(self.block.to_le_bytes());
                self.bytes = hash.finalize().into();
                self.block = next;
                self.position = 0;
            }
            let length = out.len().min(32 - self.position as usize);
            let end = self.position as usize + length;
            out[..length].copy_from_slice(&self.bytes[self.position as usize..end]);
            self.position = end as u8;
            out = &mut out[length..];
        }
    }
}
