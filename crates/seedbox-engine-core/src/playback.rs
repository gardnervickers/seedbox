//! Host pacing policy. It only decides whether to wait at an already committed
//! boundary; it cannot choose scheduler work or advance simulated time.
use seedbox_engine_protocol::host_control::{BoundaryPosition, ControlError, PlaybackRate};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct Playback {
    rate: PlaybackRate,
    anchor: Option<(u64, Instant)>,
    step_after: Option<u64>,
    sample_origin: Option<(BoundaryPosition, Instant)>,
}

impl Playback {
    pub fn rate(&self) -> PlaybackRate {
        self.rate
    }

    pub fn set_rate(&mut self, rate: PlaybackRate, position: BoundaryPosition, now: Instant) {
        self.rate = rate;
        self.sample_origin = Some((position, now));
        self.anchor = Some((position.simulated_time_ns, now));
        self.step_after = None;
    }

    pub fn achieved_rate(
        &self,
        position: BoundaryPosition,
        now: Instant,
    ) -> Option<seedbox_engine_protocol::host_control::RateMeasurement> {
        let (origin, start) = self.sample_origin?;
        Some(seedbox_engine_protocol::host_control::RateMeasurement {
            simulated_time_ns: position
                .simulated_time_ns
                .checked_sub(origin.simulated_time_ns)?,
            wall_time_ns: u64::try_from(now.checked_duration_since(start)?.as_nanos())
                .ok()?
                .try_into()
                .ok()?,
        })
    }

    /// Permit exactly one additional committed round, even when crossing a
    /// segment boundary that calls the hook again without running a round.
    pub fn step(&mut self, position: BoundaryPosition) -> Result<(), ControlError> {
        if self.rate != PlaybackRate::Paused {
            return Err(ControlError::NotPaused);
        }
        if self.step_after.is_some() {
            return Err(ControlError::StepPending);
        }
        self.step_after = Some(position.completed_rounds);
        Ok(())
    }

    pub fn stepping(&self) -> bool {
        self.step_after.is_some()
    }

    pub fn finish(&mut self) {
        self.step_after = None;
    }

    /// None permits execution; Some is a host delay before checking controls
    /// again. A caller must keep polling cancellation while waiting.
    pub fn delay(&mut self, position: BoundaryPosition, now: Instant) -> Option<Duration> {
        const POLL: Duration = Duration::from_millis(10);
        if self.sample_origin.is_none_or(|(origin, _)| {
            position.completed_rounds < origin.completed_rounds
                || position.simulated_time_ns < origin.simulated_time_ns
        }) {
            self.sample_origin = Some((position, now));
        }
        if let Some(round) = self.step_after {
            if position.completed_rounds == round {
                return None;
            }
            // A rewind cancels an outstanding step; it must not run forward
            // to the old round number before pausing again.
            self.step_after = None;
        }
        match self.rate {
            PlaybackRate::Unlimited => None,
            PlaybackRate::Paused => Some(POLL),
            PlaybackRate::Limited {
                numerator,
                denominator,
            } => {
                let (simulated, wall) =
                    self.anchor.get_or_insert((position.simulated_time_ns, now));
                // Restoring a timeline establishes a new pacing origin.
                if position.simulated_time_ns < *simulated {
                    *simulated = position.simulated_time_ns;
                    *wall = now;
                }
                let target = u128::from(position.simulated_time_ns - *simulated)
                    * u128::from(denominator.get())
                    / u128::from(numerator.get());
                let remaining = target.saturating_sub(now.duration_since(*wall).as_nanos());
                (remaining > 0).then(|| Duration::from_nanos(remaining.min(POLL.as_nanos()) as u64))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn position(round: u64, ns: u64) -> BoundaryPosition {
        BoundaryPosition {
            boundary_index: round,
            completed_rounds: round,
            simulated_time_ns: ns,
        }
    }

    #[test]
    fn measured_rate_uses_the_selected_interval_and_resets_after_rewind() {
        let start = Instant::now();
        let mut playback = Playback::default();
        playback.delay(position(0, 0), start);
        assert!(playback.achieved_rate(position(0, 0), start).is_none());
        let time = start + Duration::from_nanos(100);
        playback.delay(position(1, 50), time);
        let measured = playback.achieved_rate(position(1, 50), time).unwrap();
        assert_eq!(
            (measured.simulated_time_ns, measured.wall_time_ns.get()),
            (50, 100)
        );
        playback.set_rate(PlaybackRate::Paused, position(1, 50), time);
        let measured = playback
            .achieved_rate(position(1, 50), time + Duration::from_nanos(100))
            .unwrap();
        assert_eq!(
            (measured.simulated_time_ns, measured.wall_time_ns.get()),
            (0, 100)
        );
        playback.delay(position(0, 0), time);
        assert!(playback.achieved_rate(position(0, 0), time).is_none());
    }

    #[test]
    fn pacing_changes_waiting_only_and_resets_its_origin_after_a_pause() {
        let start = Instant::now();
        let mut playback = Playback::default();
        let twice = PlaybackRate::Limited {
            numerator: 2.try_into().unwrap(),
            denominator: 1.try_into().unwrap(),
        };
        playback.set_rate(twice, position(0, 0), start);
        assert_eq!(
            playback.delay(position(1, 20), start),
            Some(Duration::from_nanos(10))
        );
        assert_eq!(
            playback.delay(position(1, 20), start + Duration::from_nanos(10)),
            None
        );
        playback.set_rate(PlaybackRate::Paused, position(1, 20), start);
        assert!(playback
            .delay(position(1, 20), start + Duration::from_secs(10))
            .is_some());
        playback.set_rate(twice, position(1, 20), start + Duration::from_secs(10));
        assert_eq!(
            playback.delay(position(2, 40), start + Duration::from_secs(10)),
            Some(Duration::from_nanos(10))
        );
    }

    #[test]
    fn step_waits_for_a_real_commit_and_then_stays_paused() {
        let now = Instant::now();
        let mut playback = Playback::default();
        assert!(playback.step(position(7, 100)).is_err());
        playback.set_rate(PlaybackRate::Paused, position(7, 100), now);
        playback.step(position(7, 100)).unwrap();
        assert!(playback.step(position(7, 100)).is_err());
        assert_eq!(playback.delay(position(7, 100), now), None);
        assert_eq!(playback.delay(position(7, 100), now), None);
        assert!(playback.delay(position(8, 110), now).is_some());
        playback.step(position(8, 110)).unwrap();
        assert!(playback.delay(position(3, 50), now).is_some());
        assert!(!playback.stepping());
        assert!(!playback.stepping());
        assert!(playback.delay(position(8, 110), now).is_some());
    }
}
