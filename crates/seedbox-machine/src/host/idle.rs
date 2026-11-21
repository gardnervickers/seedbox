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
use std::time::{Duration, Instant};

/// The next step of a wait that has not seen progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Check again after a short busy loop.
    Spin,
    /// Pause between position loads. The count increases during one wait.
    Pause(u32),
    /// Wait for a write to the watched cache line, up to this remaining budget.
    Monitor(Duration),
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
    /// The wait ended. A learning policy samples total elapsed time, including
    /// a kernel sleep. Other policies need only the outcome.
    fn reset(&mut self, outcome: Outcome, started: Instant);
}

/// The built-in strategies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// [`SpinThenSleep`] with its standard limits.
    SpinThenSleep,
    /// [`IdleSpin`]: a long spin while the host has idle cores, else
    /// `SpinThenSleep`.
    IdleSpin,
    /// [`BusySpin`]: poll continuously without sleeping.
    BusySpin,
    /// [`Backoff`]: spin, then yield, then sleep.
    Backoff,
    /// Set the budget from recent wait lengths on this endpoint.
    Adaptive,
    /// Increase the number of CPU pause hints between reads, then park.
    PauseBackoff,
    /// Wait for a write to the monitored cache line, then park.
    Monitor,
}
impl Default for Idle {
    fn default() -> Self {
        if spin::monitor_by_default() {
            Self::Monitor
        } else {
            Self::SpinThenSleep
        }
    }
}

impl Idle {
    pub const ALL: [Self; 7] = [
        Self::SpinThenSleep,
        Self::IdleSpin,
        Self::BusySpin,
        Self::Backoff,
        Self::Adaptive,
        Self::PauseBackoff,
        Self::Monitor,
    ];
    pub fn build(self) -> Box<dyn IdleStrategy> {
        self.with_budget(SpinThenSleep::SPIN)
    }
    /// The worker can wait across a whole round while the coordinator waits
    /// for the first reply. Each endpoint keeps its own observations.
    pub fn with_budget(self, ceiling: Duration) -> Box<dyn IdleStrategy> {
        match self {
            Self::SpinThenSleep => Box::new(SpinThenSleep::new(ceiling)),
            Self::Adaptive => Box::new(Adaptive::new(ceiling)),
            Self::PauseBackoff | Self::Monitor => Box::new(QuietSpin {
                monitor: self == Self::Monitor,
                limit: ceiling,
                spin: SpinThenSleep::new(ceiling),
            }),
            Self::IdleSpin => Box::new(IdleSpin::default()),
            Self::BusySpin => Box::new(BusySpin),
            Self::Backoff => Box::new(Backoff::default()),
        }
    }
}

mod spin;
pub use spin::wait_on_word;

/// An EWMA of actual wait times and of how often even the maximum spin
/// would have missed. Parked samples keep updating both estimates, allowing
/// a previously inactive endpoint to begin spinning again without a probe.
/// Each sample has 1/8 weight. Spin for three recent mean waits, between
/// half and all of the ceiling. Park immediately once 3/4 of the weight
/// exceeds that ceiling.
struct Adaptive {
    ceiling_ns: u64,
    mean_ns: u64,
    long_fraction: u64,
}
impl Adaptive {
    fn new(ceiling: Duration) -> Self {
        // Bound intermediates in the 7/8-weighted sum even for an arbitrary Duration.
        let ceiling_ns = ceiling.as_nanos().min(u128::from(u64::MAX / 16)) as u64;
        Self {
            ceiling_ns,
            mean_ns: ceiling_ns / 8,
            long_fraction: 0,
        }
    }
    fn observe(&mut self, elapsed: Duration) {
        let ns = elapsed.as_nanos().min(u128::from(self.ceiling_ns) * 2) as u64;
        self.mean_ns = (7 * self.mean_ns + ns) / 8;
        self.long_fraction = (7 * self.long_fraction + 256 * u64::from(ns > self.ceiling_ns)) / 8;
    }
}
impl IdleStrategy for Adaptive {
    fn idle(&mut self, _: u64, waited: Duration) -> Action {
        let ns = (3 * self.mean_ns)
            .max(self.ceiling_ns / 2)
            .min(self.ceiling_ns);
        if self.long_fraction >= 192 || waited.as_nanos() >= u128::from(ns) {
            Action::Sleep
        } else {
            Action::Spin
        }
    }
    fn reset(&mut self, _: Outcome, started: Instant) {
        self.observe(started.elapsed());
    }
}

struct QuietSpin {
    monitor: bool,
    limit: Duration,
    spin: SpinThenSleep,
}
impl IdleStrategy for QuietSpin {
    fn idle(&mut self, attempt: u64, waited: Duration) -> Action {
        match self.spin.idle(attempt, waited) {
            Action::Spin if attempt > 0 && self.monitor => {
                Action::Monitor(self.limit.saturating_sub(waited))
            }
            Action::Spin if attempt > 0 => Action::Pause(1 << attempt.min(5)),
            other => other,
        }
    }
    fn reset(&mut self, outcome: Outcome, started: Instant) {
        self.spin.reset(outcome, started);
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
    fn reset(&mut self, outcome: Outcome, _: Instant) {
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
    fn reset(&mut self, outcome: Outcome, started: Instant) {
        self.short.reset(outcome, started);
    }
}

/// Dedicate the calling thread to polling. PV observes its peer's exit on
/// the transport even if the idle policy never parks.
#[derive(Default)]
pub struct BusySpin;
impl IdleStrategy for BusySpin {
    fn idle(&mut self, _: u64, _: Duration) -> Action {
        Action::Spin
    }
    fn reset(&mut self, _: Outcome, _: Instant) {}
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
    fn reset(&mut self, _: Outcome, _: Instant) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_parks_after_long_waits_and_recovers_from_parked_samples() {
        let mut policy = Adaptive::new(Duration::from_micros(200));
        for _ in 0..32 {
            policy.observe(Duration::from_millis(3));
        }
        assert_eq!(policy.idle(0, Duration::ZERO), Action::Sleep);
        for _ in 0..32 {
            policy.observe(Duration::from_micros(10));
        }
        assert_eq!(policy.idle(0, Duration::ZERO), Action::Spin);
        assert_eq!(policy.idle(1, Duration::from_micros(110)), Action::Sleep);
    }

    #[test]
    fn pause_backoff_spaces_reads_and_monitor_uses_the_remaining_budget() {
        let limit = Duration::from_micros(200);
        let mut pause = Idle::PauseBackoff.with_budget(limit);
        let mut monitor = Idle::Monitor.with_budget(limit);
        for policy in [&mut pause, &mut monitor] {
            assert_eq!(policy.idle(0, Duration::ZERO), Action::Spin);
        }
        assert_eq!(pause.idle(1, Duration::ZERO), Action::Pause(2));
        assert_eq!(pause.idle(9, Duration::ZERO), Action::Pause(32));
        assert_eq!(monitor.idle(1, limit / 2), Action::Monitor(limit / 2));
        assert_eq!(monitor.idle(2, limit), Action::Sleep);
    }

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
        strategy.reset(outcome, Instant::now());
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
        strategy.reset(Outcome::Progress, Instant::now());
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
    fn every_strategy_spins_initially_and_only_busy_spin_keeps_spinning() {
        for idle in Idle::ALL {
            let mut strategy = idle.build();
            assert_eq!(strategy.idle(0, Duration::ZERO), Action::Spin, "{idle:?}");
            let action = strategy.idle(1_000_000, Duration::from_secs(10));
            let expected = if idle == Idle::BusySpin {
                Action::Spin
            } else {
                Action::Sleep
            };
            assert_eq!(action, expected, "{idle:?}");
        }
    }
}
