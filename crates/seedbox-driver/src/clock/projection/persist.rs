//! Version 1 field/tag order. Do not renumber tags or reorder fields.
use super::*;
wire_struct!(Projection {
    anchor,
    adjustment,
    drift_ppb
});

impl Projection {
    pub(crate) fn check_persist(&self, now: u64, position: u64) -> crate::persistence::Result<()> {
        crate::persistence::require(
            self.anchor <= now
                && self.adjusted_at(self.anchor).is_ok_and(|n| n >= 0)
                && self
                    .adjusted_at(now)
                    .is_ok_and(|n| n >= i128::from(position) * DENOMINATOR)
                && crate::clock::ClockRate::new(self.drift_ppb).is_ok(),
            "time projection",
        )
    }
}
