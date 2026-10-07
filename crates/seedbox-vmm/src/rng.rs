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
    pub fn branch(seed: u64, identity: Option<&str>, domain: &str) -> Self {
        match identity {
            Some(identity) => Self::new(seed, &format!("checkpoint/{identity}/{domain}")),
            None => Self::new(seed, domain),
        }
    }
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

#[cfg(test)]
mod checkpoint_tests {
    use super::*;

    #[test]
    fn branch_streams_bind_the_full_identity_and_domain() {
        let id = crate::checkpoint::branch_identity("parent", 100, 42);
        let draws = |id: &str, domain: &str| {
            let mut rng = Rng::branch(42, Some(id), domain);
            std::array::from_fn::<_, 8, _>(|_|rng.next())
        };
        for domain in ["link/a/b", "timer-jitter/a/0", "guest-entropy/a/0", "reboot-entropy/a/1", "disk-loss/a/0"] {
            let want = draws(&id, domain);
            assert_eq!(want, draws(&id, domain));
            assert_ne!(want, draws(&crate::checkpoint::branch_identity("parent", 101, 42), domain));
            assert_ne!(want, draws(&crate::checkpoint::branch_identity("other", 100, 42), domain));
            assert_ne!(want, draws(&crate::checkpoint::branch_identity("parent", 100, 43), domain));
            assert_ne!(want, draws(&id, &format!("{domain}/other")));
        }
        let mut old = Rng::new(7, "link/a/b");
        let mut ordinary = Rng::branch(7, None, "link/a/b");
        for _ in 0..8 {assert_eq!(ordinary.next(), old.next());}
    }
}
