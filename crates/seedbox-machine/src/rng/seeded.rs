//! Private owned byte stream. SHA-256(seed || block index) with a fixed format
//! tag supplies 32-byte blocks. This defines deterministic bytes, not a security
//! guarantee. The u64 counter counts blocks; exhaustion panics before reuse.
use super::RngStream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeededRng {
    #[serde(default)]
    domain: Domain,
    seed: u64,
    block: u64,
    bytes: [u8; 32],
    position: u8,
}
/// Domain-separated byte streams derived from one machine seed. Each stream
/// has its own cursor; consuming one does not advance another.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Domain {
    /// The machine's own stream (boot and guest entropy).
    #[default]
    Machine,
    /// Seeded clock-fault values drawn for that machine as a target.
    ClockFaults,
}
impl Domain {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Machine => b"seedbox-machine seeded bytes v1\0",
            Self::ClockFaults => b"seedbox-machine clock fault bytes v1\0",
        }
    }
}
impl SeededRng {
    pub(crate) fn new(seed: u64) -> Self {
        Self::derived(seed, Domain::Machine)
    }
    pub(crate) fn derived(seed: u64, domain: Domain) -> Self {
        Self {
            domain,
            seed,
            block: 0,
            bytes: [0; 32],
            position: 32,
        }
    }
}
impl super::RngStreamExt for SeededRng {}
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
                hash.update(self.domain.tag());
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
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunks_and_restored_snapshots_continue_identical_bytes() {
        let mut whole = SeededRng::new(9);
        let mut want = [0; 107];
        whole.fill_bytes(&mut want);
        let mut pieces = SeededRng::new(9);
        let mut got = [0; 107];
        pieces.fill_bytes(&mut got[..3]);
        pieces.fill_bytes(&mut []);
        pieces.fill_bytes(&mut got[3..37]);
        let serialized = serde_json::to_vec(&pieces).unwrap();
        let mut restored: SeededRng = serde_json::from_slice(&serialized).unwrap();
        let mut clone = pieces.clone();
        pieces.fill_bytes(&mut got[37..]);
        assert_eq!(want, got);
        let mut a = [0; 70];
        let mut b = [0; 70];
        restored.fill_bytes(&mut a);
        clone.fill_bytes(&mut b);
        assert_eq!(a, b);
        assert_eq!(a, want[37..]);
        let mut other = [0; 107];
        SeededRng::new(10).fill_bytes(&mut other);
        assert_ne!(want, other);
    }
    #[test]
    #[should_panic(expected = "block counter")]
    fn exhaustion_does_not_wrap() {
        let mut rng = SeededRng::new(0);
        rng.block = u64::MAX;
        rng.fill_bytes(&mut [0]);
    }
    #[test]
    fn domains_share_a_seed_without_sharing_bytes() {
        let mut machine = SeededRng::new(9);
        let mut clock = SeededRng::derived(9, Domain::ClockFaults);
        let (mut a, mut b) = ([0; 64], [0; 64]);
        machine.fill_bytes(&mut a);
        clock.fill_bytes(&mut b);
        assert_ne!(a, b);
        // The machine domain keeps its original format tag and bytes.
        let mut hash = Sha256::new();
        hash.update(b"seedbox-machine seeded bytes v1\0");
        hash.update(9u64.to_le_bytes());
        hash.update(0u64.to_le_bytes());
        assert_eq!(a[..32], <[u8; 32]>::from(hash.finalize()));
    }
}
