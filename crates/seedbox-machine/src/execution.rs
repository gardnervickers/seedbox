//! Validated CPU-work pricing supplied by the execution environment.
//!
//! A rate changes charged CPU work, not simulation clocks, idle time or timer
//! deadlines. The driver owns the configured rate and copies it into each run.

/// CPU-work pricing configuration for one execution interval.
///
/// A factor of 1000 is nominal. Larger factors slow CPU progress, up to
/// 1_000_000 (one thousand times nominal cost). Fractional cost accumulates
/// across work: 1000 nominal one-nanosecond work units at factor 1001 cost
/// 1001 nanoseconds, rather than independently rounding each unit upward.
/// The factor prices new work. Already-charged partial work retains its earlier
/// price across later configuration changes. The caller owns this setting;
/// machine snapshots retain progress and charge caches, not rate authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionConfig(u32);

impl ExecutionConfig {
    /// Construct a configuration with a factor in 1000..=1_000_000.
    pub fn new(factor_permille: u32) -> Result<Self, InvalidExecutionRate> {
        if (1000..=1_000_000).contains(&factor_permille) {
            Ok(Self(factor_permille))
        } else {
            Err(InvalidExecutionRate { factor_permille })
        }
    }

    /// Return the CPU-work cost multiplier in thousandths.
    pub fn factor_permille(self) -> u32 {
        self.0
    }
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self(1000)
    }
}

/// A requested CPU-work factor outside 1000..=1_000_000.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidExecutionRate {
    pub factor_permille: u32,
}

impl std::fmt::Display for InvalidExecutionRate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "execution rate factor {} is outside 1000..=1000000",
            self.factor_permille
        )
    }
}
impl std::error::Error for InvalidExecutionRate {}

#[cfg(test)]
mod tests;
