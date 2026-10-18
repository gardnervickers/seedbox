//! Exact mapping from simulation time to one machine's time base.
//!
//! `G(t)` is a continuous, strictly increasing piecewise-linear function kept
//! exactly in units of 1e-9 ns. Each piece starts at an anchor and has slope
//! `(1e9 + ppb) / 1e9`. Reanchoring keeps `G` at the anchor unchanged, so the
//! offset accrued by earlier rates is never rounded away. Machines observe
//! `floor(G(t))`; the inverse returns the earliest simulation nanosecond whose
//! exact `G` reaches a machine nanosecond, so converted deadlines are never early.
//! This module owns arithmetic only; callers own rate policy.
const DENOMINATOR: i128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    PastAnchor,
    NonIncreasingRate,
    Overflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Projection {
    anchor: u64,
    adjustment: i128,
    drift_ppb: i64,
}

impl Projection {
    /// The identity mapping from `anchor` on: machine time equals simulation time.
    pub(crate) const fn new(anchor: u64) -> Self {
        Self {
            anchor,
            adjustment: 0,
            drift_ppb: 0,
        }
    }

    /// Simulation time of the most recent rate change (or construction).
    pub(crate) fn anchor(self) -> u64 {
        self.anchor
    }

    fn adjustment_at(self, at: u64) -> Result<i128, Error> {
        let elapsed = at.checked_sub(self.anchor).ok_or(Error::PastAnchor)?;
        i128::from(elapsed)
            .checked_mul(i128::from(self.drift_ppb))
            .and_then(|delta| self.adjustment.checked_add(delta))
            .ok_or(Error::Overflow)
    }

    /// Exact `G(at)` in units of 1e-9 ns.
    fn adjusted_at(self, at: u64) -> Result<i128, Error> {
        let adjustment = self.adjustment_at(at)?;
        i128::from(at)
            .checked_mul(DENOMINATOR)
            .and_then(|base| base.checked_add(adjustment))
            .ok_or(Error::Overflow)
    }

    /// Machine time observed at simulation time `at`: `floor(G(at))`.
    pub(crate) fn floor(self, at: u64) -> Result<u64, Error> {
        let exact = self.adjusted_at(at)?;
        u64::try_from(exact.div_euclid(DENOMINATOR)).map_err(|_| Error::Overflow)
    }

    /// Reanchor at `at` without rounding away accrued drift. A nominal rate of
    /// zero ends further drift and preserves the adjustment already accumulated.
    /// Failure leaves the mapping unchanged.
    pub(crate) fn set_drift(&mut self, at: u64, drift_ppb: i64) -> Result<(), Error> {
        if DENOMINATOR + i128::from(drift_ppb) <= 0 {
            return Err(Error::NonIncreasingRate);
        }
        let adjustment = self.adjustment_at(at)?;
        // Validate the new anchor before publishing any mutation.
        self.adjusted_at(at)?;
        self.anchor = at;
        self.adjustment = adjustment;
        self.drift_ppb = drift_ppb;
        Ok(())
    }

    /// Earliest simulation nanosecond at or after `not_before` whose exact
    /// mapped time reaches machine nanosecond `machine`. Since `machine` is an
    /// integer, this is also the earliest time whose observed `floor` reaches it.
    /// `Overflow` means no representable simulation time reaches it.
    pub(crate) fn deadline(self, not_before: u64, machine: u64) -> Result<u64, Error> {
        let current = self.adjusted_at(not_before)?;
        let target = i128::from(machine)
            .checked_mul(DENOMINATOR)
            .ok_or(Error::Overflow)?;
        if current >= target {
            return Ok(not_before);
        }
        let distance = u128::try_from(target - current).map_err(|_| Error::Overflow)?;
        let slope = u128::try_from(DENOMINATOR + i128::from(self.drift_ppb))
            .ok()
            .filter(|&slope| slope > 0)
            .ok_or(Error::NonIncreasingRate)?;
        let delta = distance / slope + u128::from(distance % slope != 0);
        let at = not_before
            .checked_add(u64::try_from(delta).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        self.adjusted_at(at)?;
        Ok(at)
    }
}

#[cfg(test)]
mod tests;

mod persist;
