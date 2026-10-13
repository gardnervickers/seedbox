//! Exact fault deltas applied to canonical time. This module owns arithmetic
//! only: callers retain fault policy, baseline epochs and configuration authority.
const DENOMINATOR: i128 = 1_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Error {
    PastAnchor,
    NonIncreasingRate,
    Overflow,
}

/// Numerators share DENOMINATOR; no guest readout rounding is chosen here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Sample {
    pub(super) adjusted: i128,
    pub(super) realtime: i128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Domain {
    Adjusted,
    Realtime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Projection {
    anchor: u64,
    adjustment: i128,
    drift_ppb: i64,
    realtime_step: i128,
}

impl Projection {
    pub(super) const fn new(anchor: u64) -> Self {
        Self {
            anchor,
            adjustment: 0,
            drift_ppb: 0,
            realtime_step: 0,
        }
    }

    fn adjustment_at(self, at: u64) -> Result<i128, Error> {
        let elapsed = at.checked_sub(self.anchor).ok_or(Error::PastAnchor)?;
        i128::from(elapsed)
            .checked_mul(i128::from(self.drift_ppb))
            .and_then(|delta| self.adjustment.checked_add(delta))
            .ok_or(Error::Overflow)
    }

    fn adjusted_at(self, at: u64) -> Result<i128, Error> {
        let adjustment = self.adjustment_at(at)?;
        i128::from(at)
            .checked_mul(DENOMINATOR)
            .and_then(|base| base.checked_add(adjustment))
            .ok_or(Error::Overflow)
    }

    fn value_at(self, at: u64, domain: Domain) -> Result<i128, Error> {
        let adjusted = self.adjusted_at(at)?;
        match domain {
            Domain::Adjusted => Ok(adjusted),
            Domain::Realtime => adjusted
                .checked_add(self.realtime_step)
                .ok_or(Error::Overflow),
        }
    }

    pub(super) fn evaluate(self, at: u64) -> Result<Sample, Error> {
        let adjusted = self.adjusted_at(at)?;
        let realtime = adjusted
            .checked_add(self.realtime_step)
            .ok_or(Error::Overflow)?;
        Ok(Sample { adjusted, realtime })
    }

    /// Reanchor without rounding away accrued drift. A nominal rate of zero
    /// ends further drift and preserves the adjustment already accumulated.
    pub(super) fn set_drift(&mut self, at: u64, drift_ppb: i64) -> Result<(), Error> {
        if DENOMINATOR + i128::from(drift_ppb) <= 0 {
            return Err(Error::NonIncreasingRate);
        }
        let adjustment = self.adjustment_at(at)?;
        // Validate the new anchor before publishing any mutation.
        self.evaluate(at)?;
        self.anchor = at;
        self.adjustment = adjustment;
        self.drift_ppb = drift_ppb;
        Ok(())
    }

    /// Add a signed realtime-only step at its canonical application boundary.
    /// Reanchor atomically so this projection cannot describe time before the
    /// new step. Undoing a step is the caller's policy.
    pub(super) fn step(&mut self, at: u64, nanoseconds: i128) -> Result<(), Error> {
        let adjustment = self.adjustment_at(at)?;
        let delta = nanoseconds
            .checked_mul(DENOMINATOR)
            .ok_or(Error::Overflow)?;
        let step = self
            .realtime_step
            .checked_add(delta)
            .ok_or(Error::Overflow)?;
        let candidate = Self {
            anchor: at,
            adjustment,
            realtime_step: step,
            ..*self
        };
        candidate.evaluate(at)?;
        *self = candidate;
        Ok(())
    }

    /// Earliest canonical integer nanosecond at or after `not_before` whose
    /// exact projected time reaches an integer-nanosecond deadline. Ceiling
    /// belongs to deadline inversion, not to guest clock readout policy.
    pub(super) fn deadline(
        self,
        not_before: u64,
        nanoseconds: i128,
        domain: Domain,
    ) -> Result<u64, Error> {
        let current = self.value_at(not_before, domain)?;
        let target = nanoseconds
            .checked_mul(DENOMINATOR)
            .ok_or(Error::Overflow)?;
        if current >= target {
            return Ok(not_before);
        }
        // Order-preserving signed-to-unsigned conversion admits the full exact
        // difference without overflowing signed i128 subtraction.
        let ordered = |value: i128| (value as u128) ^ (1u128 << 127);
        let distance = ordered(target)
            .checked_sub(ordered(current))
            .ok_or(Error::Overflow)?;
        let slope = u128::try_from(DENOMINATOR + i128::from(self.drift_ppb))
            .ok()
            .filter(|&slope| slope > 0)
            .ok_or(Error::NonIncreasingRate)?;
        let delta = distance / slope + u128::from(distance % slope != 0);
        let at = not_before
            .checked_add(u64::try_from(delta).map_err(|_| Error::Overflow)?)
            .ok_or(Error::Overflow)?;
        self.value_at(at, domain)?;
        Ok(at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_and_negative_drift_preserve_canonical_coordinate_and_step_isolation() {
        for rate in [-100_000_000, 0, 100_000_000] {
            let mut projection = Projection::new(7);
            projection.set_drift(7, rate).unwrap();
            let before = projection.evaluate(17).unwrap();
            assert_eq!(before.adjusted, 17 * DENOMINATOR + 10 * i128::from(rate));
            assert_eq!(before.realtime, before.adjusted);
            projection.step(projection.anchor, -23).unwrap();
            let after = projection.evaluate(17).unwrap();
            assert_eq!(after.adjusted, before.adjusted);
            assert_eq!(after.realtime, before.realtime - 23 * DENOMINATOR);
            projection.step(projection.anchor, 23).unwrap();
            assert_eq!(projection.evaluate(17).unwrap(), before);
            assert_eq!(projection.anchor, 7);
        }
    }

    #[test]
    fn delayed_step_reanchors_at_its_boundary_without_changing_drift() {
        let mut projection = Projection::new(7);
        projection.set_drift(7, -1).unwrap();
        let before = projection.evaluate(17).unwrap();
        projection.step(17, 23).unwrap();
        assert_eq!(projection.anchor, 17);
        assert_eq!(projection.evaluate(16), Err(Error::PastAnchor));
        assert_eq!(
            projection.evaluate(17).unwrap(),
            Sample {
                adjusted: before.adjusted,
                realtime: before.realtime + 23 * DENOMINATOR,
            }
        );
        assert_eq!(
            projection.evaluate(27).unwrap().adjusted,
            27 * DENOMINATOR - 20
        );
        // Undo at a later boundary preserves every fractional drift unit.
        projection.step(27, -23).unwrap();
        assert_eq!(projection.anchor, 27);
        assert_eq!(
            projection.evaluate(27).unwrap().realtime,
            27 * DENOMINATOR - 20
        );
    }

    #[test]
    fn failed_step_neither_moves_its_boundary_nor_changes_accrued_drift() {
        let mut projection = Projection::new(7);
        projection.set_drift(7, 1).unwrap();
        projection.step(17, -3).unwrap();
        let before = projection;
        assert_eq!(projection.step(16, 1), Err(Error::PastAnchor));
        assert_eq!(projection, before);
        assert_eq!(projection.step(27, i128::MAX), Err(Error::Overflow));
        assert_eq!(projection, before);
        assert_eq!(
            projection.evaluate(27).unwrap(),
            before.evaluate(27).unwrap()
        );
        let mut large = Projection::new(0);
        large.step(0, i128::MAX / DENOMINATOR).unwrap();
        let before = large;
        // The step numerator fits, but its application at a later canonical
        // boundary would overflow the final realtime sample.
        assert_eq!(large.step(u64::MAX, 0), Err(Error::Overflow));
        assert_eq!(large, before);
    }

    #[test]
    fn fractional_carry_survives_rate_changes_and_nominal_heal() {
        let mut projection = Projection::new(0);
        projection.set_drift(0, 1).unwrap();
        projection.set_drift(7, -2).unwrap();
        projection.set_drift(10, 0).unwrap();
        // Seven billionths minus six billionths is retained after heal.
        assert_eq!(
            projection.evaluate(10).unwrap().adjusted,
            10 * DENOMINATOR + 1
        );
        assert_eq!(
            projection.evaluate(100).unwrap().adjusted,
            100 * DENOMINATOR + 1
        );
        let mut split = Projection::new(0);
        split.set_drift(0, 1).unwrap();
        for at in 1..=7 {
            split.set_drift(at, 1).unwrap();
        }
        split.set_drift(7, -2).unwrap();
        for at in 8..=10 {
            split.set_drift(at, -2).unwrap();
        }
        split.set_drift(10, 0).unwrap();
        assert_eq!(split, projection);
    }

    #[test]
    fn inverse_rounds_to_earliest_canonical_deadline_in_each_domain() {
        for rate in [-100_000_000, -1, 0, 1, 100_000_000] {
            for step in [-7, 0, 7] {
                let mut projection = Projection::new(3);
                projection.set_drift(3, rate).unwrap();
                projection.step(projection.anchor, step).unwrap();
                for domain in [Domain::Adjusted, Domain::Realtime] {
                    for deadline in -10..100 {
                        let at = projection.deadline(3, deadline, domain).unwrap();
                        let value = |at| {
                            let sample = projection.evaluate(at).unwrap();
                            match domain {
                                Domain::Adjusted => sample.adjusted,
                                Domain::Realtime => sample.realtime,
                            }
                        };
                        assert!(value(at) >= deadline * DENOMINATOR);
                        if at > 3 {
                            assert!(value(at - 1) < deadline * DENOMINATOR);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn inverse_observes_fractional_adjustment_and_already_due_deadlines() {
        let mut projection = Projection::new(0);
        projection.set_drift(0, -1).unwrap();
        assert_eq!(projection.deadline(0, 1, Domain::Adjusted), Ok(2));
        projection.set_drift(1, 0).unwrap();
        assert_eq!(projection.deadline(1, 1, Domain::Adjusted), Ok(2));
        projection.step(projection.anchor, 2).unwrap();
        assert_eq!(projection.deadline(1, 1, Domain::Realtime), Ok(1));
        assert_eq!(projection.deadline(7, 1, Domain::Adjusted), Ok(7));
    }

    #[test]
    fn inverse_checks_only_the_requested_clock_domain() {
        let mut projection = Projection::new(0);
        projection.step(0, i128::MAX / DENOMINATOR).unwrap();
        assert_eq!(projection.evaluate(u64::MAX), Err(Error::Overflow));
        assert_eq!(
            projection.deadline(0, i128::from(u64::MAX), Domain::Adjusted),
            Ok(u64::MAX)
        );
        // Isolation also applies to evaluation of the starting position and
        // to a deadline that is already due there.
        assert_eq!(
            projection.deadline(u64::MAX, 0, Domain::Adjusted),
            Ok(u64::MAX)
        );
        assert_eq!(
            projection.deadline(u64::MAX, 0, Domain::Realtime),
            Err(Error::Overflow)
        );
    }

    #[test]
    fn full_canonical_range_and_checked_errors_leave_state_unchanged() {
        let mut projection = Projection::new(0);
        projection.set_drift(0, 100_000_000).unwrap();
        let sample = projection.evaluate(u64::MAX).unwrap();
        assert_eq!(sample.adjusted, i128::from(u64::MAX) * 1_100_000_000);
        assert_eq!(
            projection.deadline(u64::MAX, 1, Domain::Adjusted),
            Ok(u64::MAX)
        );
        let before = projection;
        assert_eq!(
            projection.step(projection.anchor, i128::MAX),
            Err(Error::Overflow)
        );
        assert_eq!(projection, before);
        assert_eq!(
            projection.set_drift(0, -1_000_000_000),
            Err(Error::NonIncreasingRate)
        );
        assert_eq!(projection, before);
        assert_eq!(
            projection.deadline(0, i128::MAX, Domain::Adjusted),
            Err(Error::Overflow)
        );
        let mut extreme = Projection::new(0);
        extreme
            .step(extreme.anchor, i128::MAX / DENOMINATOR)
            .unwrap();
        let extreme_before = extreme;
        assert_eq!(extreme.step(extreme.anchor, 1), Err(Error::Overflow));
        assert_eq!(extreme, extreme_before);
        // A representable step numerator must also fit when added to adjusted
        // time at the current anchor; rejected publication is atomic.
        let mut anchored = Projection::new(u64::MAX);
        let anchored_before = anchored;
        assert_eq!(
            anchored.step(anchored.anchor, i128::MAX / DENOMINATOR),
            Err(Error::Overflow)
        );
        assert_eq!(anchored, anchored_before);
        let mut fast = Projection::new(0);
        fast.set_drift(0, i64::MAX).unwrap();
        let fast_before = fast;
        assert_eq!(fast.evaluate(u64::MAX), Err(Error::Overflow));
        assert_eq!(fast, fast_before);
        let nominal = Projection::new(0);
        assert_eq!(
            nominal.deadline(0, i128::from(u64::MAX) + 1, Domain::Adjusted),
            Err(Error::Overflow)
        );
        projection.set_drift(11, 0).unwrap();
        let before = projection;
        assert_eq!(projection.set_drift(10, 1), Err(Error::PastAnchor));
        assert_eq!(projection.evaluate(10), Err(Error::PastAnchor));
        assert_eq!(
            projection.deadline(10, 0, Domain::Adjusted),
            Err(Error::PastAnchor)
        );
        assert_eq!(projection, before);
    }
}
