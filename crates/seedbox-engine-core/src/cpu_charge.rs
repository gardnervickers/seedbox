//! CPU charging shared by native adapters. Only authored jitter consumes RNG.
pub const SYSCALL_NS: u64 = 1_000;

/// Scale a retained cost by a throttle, rounding up to whole nanoseconds.
pub fn throttled_ns(syscall_ns: u64, factor_permille: u64) -> u64 {
    syscall_ns.saturating_mul(factor_permille).div_ceil(1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn nominal_throttle_and_heal_have_no_seed_variation() {
        assert_eq!(SYSCALL_NS, 1000);
        for factor in [1000, 2500, 4000, 1_000_000, 1000] {
            assert_eq!(throttled_ns(SYSCALL_NS, factor), factor);
        }
        assert_eq!(throttled_ns(937, 2500), 2343);
    }
}
