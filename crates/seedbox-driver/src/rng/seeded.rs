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
    /// Seeded frame loss on one ordered pair; the seed names the pair.
    NetworkLoss,
    /// Seeded frame jitter on one ordered pair; the seed names the pair.
    NetworkJitter,
    NetworkReorder,
    NetworkDuplicate,
    NetworkBurst,
    NetworkSpike,
    NetworkReset,
}
impl Domain {
    fn tag(self) -> &'static [u8] {
        match self {
            Self::Machine => b"seedbox-machine seeded bytes v1\0",
            Self::ClockFaults => b"seedbox-machine clock fault bytes v1\0",
            Self::NetworkLoss => b"seedbox-machine network loss bytes v1\0",
            Self::NetworkJitter => b"seedbox-machine network jitter bytes v1\0",
            Self::NetworkReorder => b"seedbox-machine network reorder bytes v1\0",
            Self::NetworkDuplicate => b"seedbox-machine network duplicate bytes v1\0",
            Self::NetworkBurst => b"seedbox-machine network burst bytes v1\0",
            Self::NetworkSpike => b"seedbox-machine network spike bytes v1\0",
            Self::NetworkReset => b"seedbox-machine network reset bytes v1\0",
        }
    }
}
impl SeededRng {
    // This file is also compiled by standalone examples without Driver codecs.
    #[allow(dead_code)]
    pub(super) fn parts(&self) -> (Domain, u64, u64, [u8; 32], u8) {
        (
            self.domain,
            self.seed,
            self.block,
            self.bytes,
            self.position,
        )
    }
    #[allow(dead_code)]
    pub(super) fn from_parts(
        domain: Domain,
        seed: u64,
        block: u64,
        bytes: [u8; 32],
        position: u8,
    ) -> Result<Self, &'static str> {
        let value = Self {
            domain,
            seed,
            block,
            bytes,
            position,
        };
        value.check_state(domain, None)?;
        Ok(value)
    }
    #[allow(dead_code)]
    pub(super) fn check_state(
        &self,
        domain: Domain,
        seed: Option<u64>,
    ) -> Result<(), &'static str> {
        if self.domain != domain || seed.is_some_and(|s| s != self.seed) {
            return Err("RNG owner/domain/seed");
        }
        if self.block == 0 {
            if self.position != 32 || self.bytes != [0; 32] {
                return Err("initial RNG cache");
            }
        } else {
            if !(1..=32).contains(&self.position) {
                return Err("RNG cursor");
            }
            let mut h = Sha256::new();
            h.update(self.domain.tag());
            h.update(self.seed.to_le_bytes());
            h.update((self.block - 1).to_le_bytes());
            let expected: [u8; 32] = h.finalize().into();
            if self.bytes != expected {
                return Err("RNG cache");
            }
        }
        Ok(())
    }
    // Only Driver admission consumes its simulation stream, eight bytes per
    // admitted machine. Machine streams retain arbitrary byte-fill widths.
    #[allow(dead_code)]
    pub(super) fn check_admission_state(
        &self,
        seeds: impl ExactSizeIterator<Item = u64>,
    ) -> Result<(), &'static str> {
        let n = seeds.len();
        let expected_position = if n == 0 {
            32
        } else {
            (((n - 1) % 4 + 1) * 8) as u8
        };
        if self.domain != Domain::Machine
            || u128::from(self.block) != (n as u128).div_ceil(4)
            || self.position != expected_position
        {
            return Err("simulation admission cursor");
        }
        let mut cache = [0u8; 32];
        for (i, seed) in seeds.enumerate() {
            if i % 4 == 0 {
                let mut hash = Sha256::new();
                hash.update(Domain::Machine.tag());
                hash.update(self.seed.to_le_bytes());
                hash.update(
                    u64::try_from(i / 4)
                        .map_err(|_| "admission index overflow")?
                        .to_le_bytes(),
                );
                cache = hash.finalize().into();
            }
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&cache[(i % 4) * 8..(i % 4 + 1) * 8]);
            if seed != u64::from_le_bytes(bytes) {
                return Err("machine admission seed");
            }
        }
        Ok(())
    }
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
mod tests;
