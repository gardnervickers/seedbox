//! Host pacing at committed barriers, independent of simulation scheduling.

mod persist;

use super::{Error, FaultRecord, Position};
use std::time::{Duration, Instant};

/// Simulated seconds per host second. A positive rational rate avoids rounding
/// the requested speed; pacing changes host waiting only, never execution bounds.
pub use seedbox_engine_protocol::host_control::PlaybackRate;

/// Control state observed at a stopped simulation boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackStatus {
    pub position: Position,
    pub rate: PlaybackRate,
    /// One quantum has been admitted but has not completed or been cancelled.
    pub stepping: bool,
    /// Permanent stop was requested. No further machine execution is permitted.
    pub stopped: bool,
    /// A one-shot stop remains armed for the next effective fault effect.
    pub before_next_fault: bool,
    /// The held fault, admitted but with its effect still unapplied.
    pub before_fault: Option<FaultRecord>,
}

#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct Playback {
    pub(super) rate: PlaybackRate,
    anchor: Option<(u64, Instant)>,
    step_after: Option<u64>,
    pub(super) before_next: bool,
    pub(super) held: Option<FaultRecord>,
}

/// Captured continuation intent cannot contain a wall-clock anchor by type.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub(super) struct PlaybackIntent {
    pub(super) rate: PlaybackRate,
    step_after: Option<u64>,
    pub(super) before_next: bool,
    pub(super) held: Option<FaultRecord>,
}
impl Playback {
    pub(super) fn checkpoint(&self) -> PlaybackIntent {
        let Self {
            rate,
            anchor: _,
            step_after,
            before_next,
            held,
        } = self;
        PlaybackIntent {
            rate: *rate,
            step_after: *step_after,
            before_next: *before_next,
            held: held.clone(),
        }
    }
    pub(super) fn from_intent(intent: PlaybackIntent) -> Self {
        let PlaybackIntent {
            rate,
            step_after,
            before_next,
            held,
        } = intent;
        Self {
            rate,
            anchor: None,
            step_after,
            before_next,
            held,
        }
    }

    pub(super) fn status(&self, position: Position, stopped: bool) -> PlaybackStatus {
        PlaybackStatus {
            position,
            rate: self.rate,
            stepping: self.step_after.is_some(),
            stopped,
            before_next_fault: self.before_next,
            before_fault: self.held.clone(),
        }
    }

    pub(super) fn set_rate(&mut self, rate: PlaybackRate, position: Position, now: Instant) {
        self.rate = rate;
        self.anchor = Some((position.at.as_nanos(), now));
        self.step_after = None;
        self.before_next = false;
    }

    pub(super) fn step<E>(&mut self, position: Position) -> Result<(), Error<E>> {
        if self.rate != PlaybackRate::Paused {
            return Err(Error::NotPaused);
        }
        if self.step_after.is_some() {
            return Err(Error::StepPending);
        }
        self.step_after = Some(position.completed_quanta);
        self.before_next = false;
        Ok(())
    }

    pub(super) fn finish_quantum(&mut self, position: Position) {
        if self
            .step_after
            .is_some_and(|after| position.completed_quanta > after)
        {
            self.finish();
        }
    }

    pub(super) fn finish(&mut self) {
        self.step_after = None;
    }

    pub(super) fn hold(&mut self, fault: FaultRecord, position: Position, now: Instant) {
        self.set_rate(PlaybackRate::Paused, position, now);
        self.held = Some(fault);
    }

    /// Due-only service dispatch holds paused effects unless a step is admitted.
    pub(super) fn holds_unarmed_effect(&self) -> bool {
        self.rate == PlaybackRate::Paused && self.step_after.is_none()
    }

    /// No sleep is performed here. Callers remain responsible for processing
    /// controls and cancellation while waiting at the committed boundary.
    pub(super) fn delay(&mut self, position: Position, now: Instant) -> Option<Duration> {
        const POLL: Duration = Duration::from_millis(10);
        if self.step_after.is_some() {
            return None;
        }
        match self.rate {
            PlaybackRate::Unlimited => None,
            PlaybackRate::Paused => Some(POLL),
            PlaybackRate::Limited {
                numerator,
                denominator,
            } => {
                let (at, wall) = self.anchor.get_or_insert((position.at.as_nanos(), now));
                let target = u128::from(position.at.as_nanos() - *at)
                    * u128::from(denominator.get())
                    / u128::from(numerator.get());
                let elapsed = now.saturating_duration_since(*wall).as_nanos();
                let remaining = target.saturating_sub(elapsed);
                (remaining > 0).then(|| Duration::from_nanos(remaining.min(POLL.as_nanos()) as u64))
            }
        }
    }
}
