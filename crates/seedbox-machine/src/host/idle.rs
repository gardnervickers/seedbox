//! How a thread waits for a peer it expects to answer soon: the idle
//! strategy of a single-writer, single-reader shared-memory channel, as in
//! Aeron. Each check that finds no progress asks the strategy what to do
//! next; the caller reports how the wait ended.
//!
//! [`Action::Sleep`] is the caller's guaranteed-wakeup sleep (for the PV
//! transport, marking the wait and blocking on its doorbell), so it ends a
//! strategy's say over that wait. A [`Action::Yield`] gives the core away
//! without marking anything: no one owes the yielding thread a wakeup, so on
//! a host with no idle core it can stay descheduled long after its peer
//! answers. Strategies therefore yield only briefly, before sleeping.
//!
//! Strategies decide only host scheduling. Every strategy delivers the same
//! bytes in the same order, so none can change a simulation's results.
use super::IdleCores;
use std::time::Duration;

/// The next step of a wait that has not seen progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Check again after a short busy loop.
    Spin,
    /// Give the core away once, then check again.
    Yield,
    /// Sleep until the peer's guaranteed wakeup.
    Sleep,
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Progress before any sleep.
    Progress,
    /// The wait slept after spinning or yielding without progress.
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

/// The built-in strategies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// [`SpinThenSleep`] with its standard limits.
    SpinThenSleep,
    /// [`IdleSpin`]: a long spin while the host has idle cores, else
    /// `SpinThenSleep`.
    IdleSpin,
    /// [`BusySpin`]: no sleep while the host has idle cores.
    BusySpin,
    /// [`Backoff`]: spin, then yield, then sleep.
    Backoff,
}
impl Idle {
    /// Spin briefly for a reply, then park with a guaranteed wakeup.
    pub const DEFAULT: Self = Self::SpinThenSleep;
    pub const ALL: [Self; 4] = [
        Self::SpinThenSleep,
        Self::IdleSpin,
        Self::BusySpin,
        Self::Backoff,
    ];
    pub fn build(self) -> Box<dyn IdleStrategy> {
        match self {
            Self::SpinThenSleep => Box::new(SpinThenSleep::default()),
            Self::IdleSpin => Box::new(IdleSpin::default()),
            Self::BusySpin => Box::new(BusySpin::default()),
            Self::Backoff => Box::new(Backoff::default()),
        }
    }
}

/// Idle cores the host needs before a strategy spins past its short limit.
/// One spare keeps a core free for whatever wakes next.
pub const IDLE_CORES: u64 = 2;

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

/// While the host has [`IDLE_CORES`] idle cores, spin up to `long` on every
/// wait: a spin then cannot take a core anyone needs. Otherwise behave as
/// [`SpinThenSleep`].
pub struct IdleSpin {
    long: Duration,
    cores: IdleCores,
    short: SpinThenSleep,
    idle: bool,
}
impl IdleSpin {
    /// Covers nearly every wait between a PV worker's runs in a round (96%
    /// on the keel-smr workload), and every wait for a run.
    pub const LONG: Duration = Duration::from_millis(1);
}
impl Default for IdleSpin {
    fn default() -> Self {
        Self {
            long: Self::LONG,
            cores: IdleCores::default(),
            short: SpinThenSleep::default(),
            idle: false,
        }
    }
}
impl IdleStrategy for IdleSpin {
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action {
        let short = self.short.idle(attempt, waited);
        if attempt == 0 {
            self.idle = self.cores.idle() >= IDLE_CORES;
        }
        match self.idle {
            true if attempt == 0 || waited < self.long => Action::Spin,
            true => Action::Sleep,
            false => short,
        }
    }
    fn reset(&mut self, outcome: Outcome) {
        self.short.reset(outcome);
    }
}

/// Spin while the host has [`IDLE_CORES`] idle cores, so a waiting thread
/// costs a whole core; sleep once it has fewer. A wait still sleeps after
/// [`BusySpin::LIMIT`], far beyond a round, so that a sleeping caller can
/// notice a peer that exited.
#[derive(Default)]
pub struct BusySpin {
    cores: IdleCores,
}
impl BusySpin {
    pub const LIMIT: Duration = Duration::from_millis(10);
}
impl IdleStrategy for BusySpin {
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action {
        if attempt == 0 || waited < Self::LIMIT && self.cores.idle() >= IDLE_CORES {
            Action::Spin
        } else {
            Action::Sleep
        }
    }
    fn reset(&mut self, _: Outcome) {}
}

/// Aeron's backoff: spin for `spin`, yield until `yield_until`, then sleep
/// (where Aeron parks with a growing timeout, the doorbell guarantees the
/// wakeup instead). Every wait starts over.
pub struct Backoff {
    spin: Duration,
    yield_until: Duration,
}
impl Backoff {
    pub const SPIN: Duration = Duration::from_micros(20);
    pub const YIELD_UNTIL: Duration = Duration::from_micros(220);
}
impl Default for Backoff {
    fn default() -> Self {
        Self {
            spin: Self::SPIN,
            yield_until: Self::YIELD_UNTIL,
        }
    }
}
impl IdleStrategy for Backoff {
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action {
        if attempt == 0 || waited < self.spin {
            Action::Spin
        } else if waited < self.yield_until {
            Action::Yield
        } else {
            Action::Sleep
        }
    }
    fn reset(&mut self, _: Outcome) {}
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
    fn backoff_spins_then_yields_then_sleeps() {
        let mut strategy = Backoff::default();
        assert_eq!(strategy.idle(0, Duration::from_secs(1)), Action::Spin);
        assert_eq!(strategy.idle(1, Duration::ZERO), Action::Spin);
        assert_eq!(strategy.idle(2, Backoff::SPIN), Action::Yield);
        assert_eq!(strategy.idle(3, Backoff::YIELD_UNTIL), Action::Sleep);
    }

    #[test]
    fn every_strategy_tries_once_then_bounds_its_wait() {
        for idle in Idle::ALL {
            let mut strategy = idle.build();
            assert_eq!(strategy.idle(0, Duration::ZERO), Action::Spin, "{idle:?}");
            let action = strategy.idle(1_000_000, Duration::from_secs(10));
            assert_eq!(action, Action::Sleep, "{idle:?} bounds its wait");
        }
    }
}
