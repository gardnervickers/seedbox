//! Seeded randomness for schedules and workloads: xoshiro256** seeded through
//! SplitMix64. In a guest the seed comes from the simulator's deterministic
//! entropy, so a Seedbox seed fixes every choice; the oracle records it.

/// A small, fast, seedable generator. Never cryptographic.
#[derive(Clone, Debug)]
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

/// A 32-bit seed from the operating system's entropy (`/dev/urandom`); in a
/// Seedbox guest this is the simulation's seeded stream.
pub fn entropy_seed() -> u64 {
    use std::io::Read;
    let mut bytes = [0u8; 4];
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        if file.read_exact(&mut bytes).is_ok() {
            return u64::from(u32::from_le_bytes(bytes));
        }
    }
    // No entropy device: a fixed seed keeps the run reproducible.
    1
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

    /// A generator for one of several independent streams of one seed.
    pub fn stream(seed: u64, stream: u64) -> Self {
        Self::new(seed ^ stream.wrapping_mul(0xd1b5_4a32_d192_ed03))
    }

    pub fn next_u64(&mut self) -> u64 {
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

    /// Uniform in `0..n`; `n` must be positive.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        // Lemire's nearly divisionless method, with rejection for exactness.
        loop {
            let product = u128::from(self.next_u64()) * u128::from(n);
            let low = product as u64;
            if low >= n.wrapping_neg() % n {
                return (product >> 64) as u64;
            }
        }
    }

    /// Uniform in `low..=high`.
    pub fn range(&mut self, low: u64, high: u64) -> u64 {
        assert!(low <= high);
        if high - low == u64::MAX {
            return self.next_u64();
        }
        low + self.below(high - low + 1)
    }

    /// Uniform in `[0, 1)` with 53 bits.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[low, high)`.
    pub fn uniform(&mut self, low: f64, high: f64) -> f64 {
        low + (high - low) * self.unit()
    }

    /// Uniform seconds in `[low, high)` rounded to whole milliseconds, which
    /// control sleeps accept exactly.
    pub fn seconds(&mut self, low: f64, high: f64) -> f64 {
        (self.uniform(low, high) * 1000.0).round() / 1000.0
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }

    pub fn choice<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }

    /// `k` distinct items in random order (a partial Fisher–Yates shuffle).
    pub fn sample<T: Clone>(&mut self, items: &[T], k: usize) -> Vec<T> {
        let mut pool: Vec<T> = items.to_vec();
        let k = k.min(pool.len());
        for i in 0..k {
            let j = i + self.below((pool.len() - i) as u64) as usize;
            pool.swap(i, j);
        }
        pool.truncate(k);
        pool
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// An index chosen with probability proportional to its weight.
    pub fn weighted(&mut self, weights: &[f64]) -> usize {
        let total: f64 = weights.iter().sum();
        let mut point = self.unit() * total;
        for (index, weight) in weights.iter().enumerate() {
            if point < *weight {
                return index;
            }
            point -= weight;
        }
        weights.len() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_fixes_every_draw() {
        let draws = |seed| {
            let mut rng = Rng::new(seed);
            (0..8).map(|_| rng.below(1000)).collect::<Vec<_>>()
        };
        assert_eq!(draws(7), draws(7));
        assert_ne!(draws(7), draws(8));
    }

    #[test]
    fn draws_stay_in_range_and_cover_it() {
        let mut rng = Rng::new(1);
        let mut seen = [0usize; 5];
        for _ in 0..5000 {
            seen[rng.range(2, 6) as usize - 2] += 1;
            let x = rng.uniform(1.0, 5.0);
            assert!((1.0..5.0).contains(&x));
        }
        assert!(seen.iter().all(|&n| n > 800), "{seen:?}");
        let sample = rng.sample(&[1, 2, 3, 4, 5], 3);
        assert_eq!(sample.len(), 3);
        let mut unique = sample.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), 3);
        let mut heavy = 0;
        for _ in 0..1000 {
            if rng.weighted(&[1.0, 0.0, 9.0]) == 2 {
                heavy += 1;
            }
            assert_ne!(rng.weighted(&[1.0, 0.0, 9.0]), 1);
        }
        assert!(heavy > 850);
    }
}
