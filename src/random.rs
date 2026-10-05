use sha2::{Digest, Sha256};
use std::collections::VecDeque;

/// Versioned, domain-separated stream. No host entropy and no RNG crate defaults.
pub struct RandomStream {
    key: [u8; 32],
    counter: u128,
    buffer: VecDeque<u8>,
}

impl RandomStream {
    pub fn new(seed: u64, domain: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"seedbox-rng-v1\0");
        digest.update(seed.to_le_bytes());
        digest.update(domain.as_bytes());
        Self {
            key: digest.finalize().into(),
            counter: 0,
            buffer: VecDeque::new(),
        }
    }
    pub fn read(&mut self, length: usize) -> Vec<u8> {
        while self.buffer.len() < length {
            let mut digest = Sha256::new();
            digest.update(self.key);
            digest.update(self.counter.to_le_bytes());
            self.buffer.extend(digest.finalize());
            self.counter += 1;
        }
        self.buffer.drain(..length).collect()
    }
    pub fn below(&mut self, bound: u64) -> u64 {
        assert!(bound > 0, "bounded RNG requires a positive bound");
        let range = 1u128 << 64;
        let limit = range - range % u128::from(bound);
        loop {
            let value = u64::from_le_bytes(self.read(8).try_into().expect("eight random bytes"));
            if u128::from(value) < limit {
                return value % bound;
            }
        }
    }
}
