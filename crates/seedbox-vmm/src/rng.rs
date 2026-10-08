//! Versioned SHA-256 random streams, one per named domain.

use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct Rng {
    key: [u8; 32],
    counter: u64,
}

impl Rng {
    pub fn new(seed: u64, domain: &str) -> Self {
        let mut h = Sha256::new();
        h.update(b"seedbox-v1");
        h.update(seed.to_le_bytes());
        h.update(domain.as_bytes());
        Self {
            key: h.finalize().into(),
            counter: 0,
        }
    }
    pub fn counter(&self) -> u64 { self.counter }
    pub fn next(&mut self) -> u64 {
        let mut h = Sha256::new();
        h.update(self.key);
        h.update(self.counter.to_le_bytes());
        self.counter += 1;
        u64::from_le_bytes(h.finalize()[..8].try_into().unwrap())
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let v = self.next();
            if v < zone {
                return v % n;
            }
        }
    }
}
