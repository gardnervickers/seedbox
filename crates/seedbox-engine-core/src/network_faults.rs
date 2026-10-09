//! Independent streams and evidence for opt-in directed network impairments.
use crate::{links::Pair, rng::Rng};
use seedbox_control::MAX_PPM;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const REORDER: usize = 0;
pub const DUPLICATE: usize = 1;
pub const BURST: usize = 2;
pub const SPIKE: usize = 3;
pub const RESET: usize = 4;
const DOMAINS: [&str; 5] = [
    "link-reorder",
    "link-duplicate",
    "link-burst-loss",
    "link-latency-spike",
    "link-tcp-reset",
];

#[derive(Clone, Default)]
pub struct Streams([BTreeMap<Pair, Rng>; 5]);
impl Streams {
    pub fn reseed(&mut self, seed: u64, identity: Option<&str>, names: &[String]) {
        for (streams, domain) in self.0.iter_mut().zip(DOMAINS) {
            streams.clear();
            for a in 0..names.len() {
                for b in 0..names.len() {
                    if a != b {
                        streams.insert(
                            (a, b),
                            Rng::branch(
                                seed,
                                identity,
                                &format!("{domain}/{}/{}", names[a], names[b]),
                            ),
                        );
                    }
                }
            }
        }
    }
    /// Every ordered pair of distinct machines has a stream of each kind
    /// from the last reseed.
    fn stream(&mut self, pair: Pair, kind: usize) -> &mut Rng {
        self.0[kind]
            .get_mut(&pair)
            .expect("a network fault stream per machine pair")
    }
    pub fn choose(&mut self, pair: Pair, kind: usize, ppm: u64) -> bool {
        ppm != 0
            && (self.stream(pair, kind).next() as u128) * u128::from(MAX_PPM) < (ppm as u128) << 64
    }
    pub fn delay(&mut self, pair: Pair, maximum: u64) -> u64 {
        self.stream(pair, REORDER).below(maximum) + 1
    }
    pub fn witness(&self, h: &mut Sha256) {
        for streams in &self.0 {
            h.update((streams.len() as u64).to_le_bytes());
            for (&pair, rng) in streams {
                crate::links::pair_hash(h, pair);
                h.update(rng.counter().to_le_bytes());
            }
        }
    }
    #[cfg(test)]
    pub fn counters(&self, pair: Pair) -> [u64; 5] {
        std::array::from_fn(|kind| self.0[kind][&pair].counter())
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Counts {
    pub matched: u64,
    /// Candidates given an extra reorder delay; receipt order is workload evidence.
    pub reordered: u64,
    pub duplicated: u64,
    pub burst_dropped: u64,
    pub spiked: u64,
    pub tcp_reset: u64,
    pub delayed_max_ns: u64,
}
impl Counts {
    pub fn witness(self, h: &mut Sha256) {
        for n in [
            self.matched,
            self.reordered,
            self.duplicated,
            self.burst_dropped,
            self.spiked,
            self.tcp_reset,
            self.delayed_max_ns,
        ] {
            h.update(n.to_le_bytes());
        }
    }
}
