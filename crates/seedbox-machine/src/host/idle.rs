//! How a thread waits for a peer it expects to answer soon: the idle
//! strategy of a single-writer, single-reader shared-memory channel, as in
//! Aeron. Each check that finds no progress asks the strategy what to do
//! next; the caller reports how the wait ended.
//!
//! [`Action::Sleep`] is the caller's guaranteed-wakeup sleep (for the PV
//! transport, marking the wait and blocking on its doorbell), so it ends a
//! strategy's say over that wait.
//!
//! Strategies decide only host scheduling. Every strategy delivers the same
//! bytes in the same order, so none can change a simulation's results.
use std::time::Duration;

/// The next step of a wait that has not seen progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Check again after a short busy loop.
    Spin,
    /// Sleep until the peer's guaranteed wakeup.
    Sleep,
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Progress before any sleep.
    Progress,
    /// The wait slept after spinning without progress.
    SleptAfterTrying,
    /// The wait slept without trying.
    SleptAtOnce,
}

/// A waiting side's policy. `idle` is called for attempt 0, 1, ... of one
/// wait until the caller sees progress or the strategy says
/// [`Action::Sleep`]; `reset` then reports the outcome, before the next wait.
pub trait IdleStrategy: Send {
    /// What to do after `attempt` checks without progress, `waited` into the
    /// wait.
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action;
    /// The wait ended.
    fn reset(&mut self, outcome: Outcome);
}

/// Spin up to `spin`, then sleep. A spin pays only while the peer runs on
/// another core; on a host with no idle core the spinner may hold the core its
/// peer needs and its spins run out, so after `futile` such waits in a row it
/// sleeps at once, spinning again only on every `probe`-th wait to notice a
/// core coming free.
pub struct SpinThenSleep {
    spin: Duration,
    futile_limit: u32,
    probe: u64,
    waits: u64,
    futile: u32,
    spinning: bool,
}
impl SpinThenSleep {
    pub const SPIN: Duration = Duration::from_micros(200);
    pub const FUTILE: u32 = 4;
    pub const PROBE: u64 = 64;
    pub fn new(spin: Duration) -> Self {
        Self {
            spin,
            futile_limit: Self::FUTILE,
            probe: Self::PROBE,
            waits: 0,
            futile: 0,
            spinning: false,
        }
    }
    /// Waits in a row whose spin ran out.
    pub fn futile(&self) -> u32 {
        self.futile
    }
}
impl Default for SpinThenSleep {
    fn default() -> Self {
        Self::new(Self::SPIN)
    }
}
impl IdleStrategy for SpinThenSleep {
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action {
        if attempt == 0 {
            self.waits += 1;
            self.spinning =
                self.futile < self.futile_limit || self.waits.is_multiple_of(self.probe);
        }
        if self.spinning && (attempt == 0 || waited < self.spin) {
            Action::Spin
        } else {
            Action::Sleep
        }
    }
    fn reset(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Progress => self.futile = 0,
            Outcome::SleptAfterTrying => self.futile = self.futile.saturating_add(1),
            Outcome::SleptAtOnce => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One wait whose spin runs out: the actions until it sleeps.
    fn run_out(strategy: &mut SpinThenSleep) -> Vec<Action> {
        let mut actions = vec![strategy.idle(0, Duration::ZERO)];
        if actions[0] == Action::Spin {
            actions.push(strategy.idle(1, strategy.spin));
        }
        let outcome = if actions[0] == Action::Spin {
            Outcome::SleptAfterTrying
        } else {
            Outcome::SleptAtOnce
        };
        strategy.reset(outcome);
        actions
    }

    #[test]
    fn spins_that_run_out_stop_spinning_except_on_probes() {
        let mut strategy = SpinThenSleep::default();
        let waits = SpinThenSleep::PROBE;
        let spun: Vec<bool> = (1..=waits)
            .map(|_| run_out(&mut strategy)[0] == Action::Spin)
            .collect();
        let expected: Vec<bool> = (1..=waits)
            .map(|wait| wait <= u64::from(SpinThenSleep::FUTILE) || wait == waits)
            .collect();
        assert_eq!(spun, expected);
        assert_eq!(strategy.futile(), SpinThenSleep::FUTILE + 1);
        // Progress restores spinning.
        strategy.reset(Outcome::Progress);
        assert_eq!(strategy.idle(0, Duration::ZERO), Action::Spin);
    }

    #[test]
    fn a_wait_tries_once_then_sleeps_when_its_spin_is_spent() {
        let mut strategy = SpinThenSleep::default();
        assert_eq!(strategy.idle(0, Duration::from_secs(1)), Action::Spin);
        assert_eq!(strategy.idle(1, Duration::ZERO), Action::Spin);
        assert_eq!(strategy.idle(2, SpinThenSleep::SPIN), Action::Sleep);
    }
}
