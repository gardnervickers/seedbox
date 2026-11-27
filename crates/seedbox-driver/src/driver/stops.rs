//! Where a round stops: the Driver's boundary, or an earlier one a test asks for.
//!
//! A round may end anywhere in `(now, boundary]` without breaking safety: no
//! frame or timeline item can be due before the boundary the Driver computed,
//! so a shorter round only adds a barrier. Tests use that to check that no
//! result depends on where rounds end. Outside `test-support` this is empty
//! and every round ends at the Driver's boundary.
use super::*;

/// Host-side choice of round ends. Not simulation state: it is never part of
/// evidence or a checkpoint, and a restored Driver starts without a policy.
#[derive(Default)]
pub(super) struct Stops {
    #[cfg(any(test, feature = "test-support"))]
    policy: Option<StopPolicy>,
}

impl Stops {
    /// The end of a round that starts at `now` and may run to `boundary`.
    #[cfg_attr(not(any(test, feature = "test-support")), inline(always))]
    pub(super) fn end(&mut self, now: Timestamp, boundary: Timestamp) -> Timestamp {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(policy) = &mut self.policy {
            return policy.end(now, boundary);
        }
        let _ = now;
        boundary
    }
}

/// A seeded host RNG that ends rounds early: each round's end is drawn from
/// `(now, boundary]`, often at the boundary itself, often far before it, and
/// sometimes within a few nanoseconds of the round's start. It only ever
/// shortens a round. Its draws come from its own seed, never from the run's.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct StopPolicy {
    state: u64,
}

#[cfg(any(test, feature = "test-support"))]
impl StopPolicy {
    pub fn seeded(seed: u64) -> Self {
        Self { state: seed }
    }

    /// SplitMix64.
    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn end(&mut self, now: Timestamp, boundary: Timestamp) -> Timestamp {
        let Some(span) = boundary
            .as_nanos()
            .checked_sub(now.as_nanos())
            .filter(|&span| span > 1)
        else {
            return boundary;
        };
        let draw = self.next();
        let offset = match draw % 4 {
            0 => span,
            // Within a few nanoseconds of the start, to cut next to events.
            1 => 1 + (draw >> 2) % span.min(4),
            // A length of any magnitude up to the span.
            2 => {
                let bits = 64 - span.leading_zeros();
                let scale = 1 + (draw >> 2) % u64::from(bits);
                1 + (draw >> 8) % span.min(1 << (scale - 1).min(63))
            }
            _ => 1 + (draw >> 2) % span,
        };
        Timestamp::from_nanos(now.as_nanos() + offset)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl<M: Machine> Driver<M> {
    /// End every later round at a time `policy` draws in `(now, boundary]`
    /// instead of at the Driver's boundary; `None` restores the Driver's own.
    /// Every step then returns at the shortened barrier. Rounds that end early
    /// change no result: `tests/driver_stop_policy.rs` checks that on scripted
    /// machines, and the whole-run determinism oracles on real runs, whose
    /// execution request names a policy seed (`ExecutionRequest::stop_policy`
    /// in seedbox-driver-execution).
    pub fn set_stop_policy(&mut self, policy: Option<StopPolicy>) {
        self.stops.policy = policy;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_policy_only_shortens_rounds_and_reaches_both_ends() {
        let mut policy = StopPolicy::seeded(9);
        let now = Timestamp::from_nanos(1_000);
        let boundary = Timestamp::from_nanos(1_001_000);
        let ends: Vec<_> = (0..4_000).map(|_| policy.end(now, boundary)).collect();
        assert!(ends.iter().all(|&end| now < end && end <= boundary));
        assert!(ends.contains(&boundary));
        assert!(ends.iter().any(|end| end.as_nanos() <= now.as_nanos() + 4));
        assert!(ends
            .iter()
            .any(|end| (now.as_nanos() + 100..now.as_nanos() + 10_000).contains(&end.as_nanos())));
        // A round of one nanosecond, or none, cannot be shortened.
        let next = Timestamp::from_nanos(1_001);
        assert_eq!(policy.end(now, next), next);
        assert_eq!(policy.end(now, now), now);
        // Without a policy every round ends at the Driver's boundary.
        assert_eq!(Stops::default().end(now, boundary), boundary);
    }
}
