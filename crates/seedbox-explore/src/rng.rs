//! A splitmix64 stream with integer-only draws, identical on every host.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    /// A stream for `seed` in `domain`, separate from every other domain.
    pub(crate) fn new(seed: u64, domain: u64) -> Self {
        Self(seed ^ domain)
    }
    pub(crate) fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in `0..bound` by rejection; `bound` must be positive.
    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        let zone = u64::MAX - u64::MAX % bound;
        loop {
            let value = self.next();
            if value < zone {
                return value % bound;
            }
        }
    }
    /// True with probability `permille` / 1000.
    pub(crate) fn chance(&mut self, permille: u32) -> bool {
        self.below(1000) < u64::from(permille)
    }
}
