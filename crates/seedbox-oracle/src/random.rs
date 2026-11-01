//! A seeded random source and the draws schedules and workloads make.
use crate::Random;
use std::time::Duration;

/// xoshiro256** seeded through SplitMix64: small, fast and seedable, never
/// cryptographic. Seeded from the guest's entropy ([`Rng::from_entropy`]),
/// which Seedbox draws from the run's seed, every choice repeats with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rng {
    state: [u64; 4],
}

fn splitmix(seed: &mut u64) -> u64 {
    *seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *seed;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// Eight bytes of the operating system's entropy, if it has any; in a
/// Seedbox guest this is the simulation's seeded stream.
fn entropy() -> Option<u64> {
    use std::io::Read;
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")
        .ok()?
        .read_exact(&mut bytes)
        .ok()?;
    Some(u64::from_le_bytes(bytes))
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut s = seed;
        Self {
            state: [
                splitmix(&mut s),
                splitmix(&mut s),
                splitmix(&mut s),
                splitmix(&mut s),
            ],
        }
    }

    /// Seeded from the operating system's entropy, or a fixed seed without
    /// an entropy device, so a run stays reproducible either way.
    pub fn from_entropy() -> Self {
        Self::new(entropy().unwrap_or(1))
    }

    /// Mix fresh operating-system entropy into the state, so later draws
    /// depend on it. A run branched from a checkpoint restores this
    /// generator with the rest of the guest; a scenario that refreshes
    /// between decisions lets a branch whose guest entropy was reseeded
    /// decide differently from its parent (branch reseeding is not part of
    /// this library). The standard patterns draw through `&mut dyn Random`
    /// and never refresh. Without an entropy device this does nothing.
    pub fn refresh(&mut self) {
        if let Some(word) = entropy() {
            self.mix(word);
        }
    }

    fn mix(&mut self, word: u64) {
        *self = Self::new(self.next_u64() ^ word);
    }

    /// An independent generator for one of several streams, such as one
    /// per client, drawn from this one.
    pub fn split(&mut self) -> Self {
        Self::new(self.next_u64())
    }
}

impl Random for Rng {
    fn next_u64(&mut self) -> u64 {
        let [s0, s1, s2, s3] = &mut self.state;
        let result = s1.wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = *s1 << 17;
        *s2 ^= *s0;
        *s3 ^= *s1;
        *s1 ^= *s2;
        *s0 ^= *s3;
        *s2 ^= t;
        *s3 = s3.rotate_left(45);
        result
    }
}

/// Draws over any [`Random`], including `&mut dyn Random`.
pub trait RandomExt: Random {
    /// Uniform in `0..n`; `n` must be positive.
    fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        // Lemire's nearly divisionless method, with rejection for exactness.
        loop {
            let product = u128::from(self.next_u64()) * u128::from(n);
            if (product as u64) >= n.wrapping_neg() % n {
                return (product >> 64) as u64;
            }
        }
    }

    /// Uniform in `low..=high`.
    fn range(&mut self, low: u64, high: u64) -> u64 {
        assert!(low <= high, "empty range");
        if high - low == u64::MAX {
            return self.next_u64();
        }
        low + self.below(high - low + 1)
    }

    /// Uniform between two durations, inclusive, in whole milliseconds,
    /// which control sleeps take exactly.
    fn duration(&mut self, low: Duration, high: Duration) -> Duration {
        let (low, high) = (low.as_millis() as u64, high.as_millis() as u64);
        Duration::from_millis(self.range(low, high.max(low)))
    }

    /// True with probability `ppm` in a million.
    fn chance(&mut self, ppm: u64) -> bool {
        self.below(1_000_000) < ppm
    }

    fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// `k` distinct items in random order (a partial Fisher-Yates shuffle).
    fn sample<T: Clone>(&mut self, items: &[T], k: usize) -> Vec<T> {
        let mut pool = items.to_vec();
        let k = k.min(pool.len());
        for i in 0..k {
            let j = i + self.below((pool.len() - i) as u64) as usize;
            pool.swap(i, j);
        }
        pool.truncate(k);
        pool
    }

    fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

impl<R: Random + ?Sized> RandomExt for R {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_fixes_every_draw_and_mixing_changes_later_ones() {
        let draws = |seed| {
            let mut rng = Rng::new(seed);
            (0..8).map(|_| rng.below(1000)).collect::<Vec<_>>()
        };
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
        let after = |word| {
            let mut rng = Rng::new(7);
            rng.mix(word);
            rng.next_u64()
        };
        assert_eq!(after(1), after(1));
        assert_ne!(after(1), after(2));
        let mut parent = Rng::new(7);
        let mut child = parent.split();
        assert_ne!(parent.next_u64(), child.next_u64());
    }

    #[test]
    fn draws_stay_in_range_through_a_trait_object() {
        let mut rng = Rng::new(1);
        let random: &mut dyn Random = &mut rng;
        let mut seen = [0usize; 5];
        for _ in 0..5000 {
            seen[random.range(2, 6) as usize - 2] += 1;
        }
        assert!(seen.iter().all(|&n| n > 800), "{seen:?}");
        let d = random.duration(Duration::from_millis(10), Duration::from_millis(20));
        assert!((10..=20).contains(&d.as_millis()));
        let mut sample = random.sample(&[1, 2, 3, 4, 5], 3);
        sample.sort();
        sample.dedup();
        assert_eq!(sample.len(), 3);
        assert!(!random.chance(0) && random.chance(1_000_000));
        let mut items = [1, 2, 3, 4];
        random.shuffle(&mut items);
        items.sort();
        assert_eq!(items, [1, 2, 3, 4]);
    }
}
