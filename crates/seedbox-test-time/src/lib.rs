//! Deterministic test-time mechanisms, usable as a dev-dependency of any component.
//!
//! Logical clocks, explicit handshakes and guest-progress budgets determine
//! functional results. Real time is confined to [`HangGuard`], which bounds a
//! hung positive wait and never establishes that an event did not occur.
//! This crate has no dependencies, runtime adapters, async API or spawning.

use std::{
    fmt,
    sync::{mpsc, Arc, Condvar, Mutex, MutexGuard},
    time::{Duration, Instant},
};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A peer panic must not prevent cancellation and strand another test actor.
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// A shared logical elapsed clock. Host passage of time never changes its value.
#[derive(Clone, Debug, Default)]
pub struct ControlledClock(Arc<Mutex<Duration>>);

/// An invalid controlled-clock advance; the clock is unchanged on error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockError {
    Backwards {
        current: Duration,
        requested: Duration,
    },
    Overflow,
}
impl fmt::Display for ClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid controlled-clock advance: {self:?}")
    }
}
impl std::error::Error for ClockError {}
impl ControlledClock {
    pub fn new(now: Duration) -> Self {
        Self(Arc::new(Mutex::new(now)))
    }
    pub fn now(&self) -> Duration {
        *lock(&self.0)
    }
    pub fn advance_to(&self, requested: Duration) -> Result<(), ClockError> {
        let mut current = lock(&self.0);
        if requested < *current {
            return Err(ClockError::Backwards {
                current: *current,
                requested,
            });
        }
        *current = requested;
        Ok(())
    }
    pub fn advance_by(&self, elapsed: Duration) -> Result<(), ClockError> {
        let mut current = lock(&self.0);
        *current = current.checked_add(elapsed).ok_or(ClockError::Overflow)?;
        Ok(())
    }
}

/// Infrastructure failure of a guarded wait, distinct from a subject's timeout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitError {
    Hang { reason: String, limit: Duration },
    Cancelled,
    ProceedBeforeEntered,
}
impl fmt::Display for WaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hang { reason, limit } => {
                write!(f, "test infrastructure hang after {limit:?}: {reason}")
            }
            Self::Cancelled => f.write_str("test handshake/channel peer cancelled"),
            Self::ProceedBeforeEntered => {
                f.write_str("test participant has not entered its handshake")
            }
        }
    }
}
impl std::error::Error for WaitError {}

/// Safety bound for infrastructure that must eventually produce a positive event.
///
/// Supply a reason and a documented limit with room for a loaded host. Clones
/// share the original deadline: retries and nested waits cannot renew the budget.
/// Expiry is always an infrastructure failure, never the subject's result.
/// Do not use remaining time to assert speed, or absence over a time interval.
#[derive(Clone, Debug)]
pub struct HangGuard {
    started: Instant,
    limit: Duration,
    reason: String,
}
impl HangGuard {
    pub fn new(reason: impl Into<String>, limit: Duration) -> Self {
        let reason = reason.into();
        assert!(!reason.trim().is_empty(), "a hang guard needs a reason");
        Self {
            started: Instant::now(),
            limit,
            reason,
        }
    }
    fn expired(&self) -> WaitError {
        WaitError::Hang {
            reason: self.reason.clone(),
            limit: self.limit,
        }
    }
    /// Remaining infrastructure budget, for adapters such as socket I/O.
    pub fn remaining(&self) -> Result<Duration, WaitError> {
        self.limit
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| self.expired())
    }
    /// Wait for a value from a nonblocking observation. `None` means pending,
    /// never success. The callback must not block or interpret timed absence.
    pub fn wait_for<T>(&self, mut observe: impl FnMut() -> Option<T>) -> Result<T, WaitError> {
        loop {
            if let Some(value) = observe() {
                return Ok(value);
            }
            std::thread::park_timeout(self.remaining()?.min(Duration::from_millis(1)));
        }
    }
    /// Wait for one actual channel message; a disconnected peer is cancellation.
    pub fn recv<T>(&self, receiver: &mpsc::Receiver<T>) -> Result<T, WaitError> {
        match receiver.try_recv() {
            Ok(value) => return Ok(value),
            Err(mpsc::TryRecvError::Disconnected) => return Err(WaitError::Cancelled),
            Err(mpsc::TryRecvError::Empty) => {}
        }
        receiver
            .recv_timeout(self.remaining()?)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => self.expired(),
                mpsc::RecvTimeoutError::Disconnected => WaitError::Cancelled,
            })
    }
}

#[derive(Default)]
struct State {
    entered: bool,
    proceed: bool,
    controller_alive: bool,
    participant_alive: bool,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}
/// The observer side of a one-shot entered/proceed handshake.
pub struct Controller(Arc<Shared>);
/// The participant side of a one-shot entered/proceed handshake.
pub struct Participant(Arc<Shared>);

/// Create a synchronous handshake without spawning an actor.
/// Dropping either endpoint wakes its peer with cancellation; only an explicit
/// [`Controller::proceed`] permits a participant to continue successfully.
pub fn handshake() -> (Controller, Participant) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            controller_alive: true,
            participant_alive: true,
            ..State::default()
        }),
        changed: Condvar::new(),
    });
    (Controller(shared.clone()), Participant(shared))
}
impl Shared {
    fn wait<'a>(
        &self,
        state: MutexGuard<'a, State>,
        guard: &HangGuard,
    ) -> Result<MutexGuard<'a, State>, WaitError> {
        let (state, _) = self
            .changed
            .wait_timeout(state, guard.remaining()?)
            .unwrap_or_else(|error| error.into_inner());
        Ok(state)
    }
}
impl Controller {
    /// Observe that the participant reached its gate. It cannot pass the gate
    /// successfully until this controller explicitly grants permission.
    pub fn wait_entered(&self, guard: &HangGuard) -> Result<(), WaitError> {
        let mut state = lock(&self.0.state);
        loop {
            if !state.participant_alive {
                return Err(WaitError::Cancelled);
            }
            if state.entered {
                return Ok(());
            }
            state = self.0.wait(state, guard)?;
        }
    }
    /// Grant permission once, only after the participant entered.
    pub fn proceed(self) -> Result<(), WaitError> {
        let mut state = lock(&self.0.state);
        if !state.participant_alive {
            return Err(WaitError::Cancelled);
        }
        if !state.entered {
            return Err(WaitError::ProceedBeforeEntered);
        }
        state.proceed = true;
        self.0.changed.notify_all();
        Ok(())
    }
}
impl Participant {
    /// Signal entry and wait for explicit permission. Cancellation and guard
    /// expiry cannot be mistaken for successful permission to proceed.
    pub fn enter_and_wait(self, guard: &HangGuard) -> Result<(), WaitError> {
        let mut state = lock(&self.0.state);
        if !state.controller_alive {
            return Err(WaitError::Cancelled);
        }
        state.entered = true;
        self.0.changed.notify_all();
        loop {
            if state.proceed {
                return Ok(());
            }
            if !state.controller_alive {
                return Err(WaitError::Cancelled);
            }
            state = self.0.wait(state, guard)?;
        }
    }
}
impl Drop for Controller {
    fn drop(&mut self) {
        lock(&self.0.state).controller_alive = false;
        self.0.changed.notify_all();
    }
}
impl Drop for Participant {
    fn drop(&mut self) {
        lock(&self.0.state).participant_alive = false;
        self.0.changed.notify_all();
    }
}

/// Invalid simulated progress; no host clock participates in this result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProgressError {
    InvalidBudget,
    ZeroQuantum,
    Backwards,
    NoProgress,
    Deadline,
}
impl fmt::Display for ProgressError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid guest progress: {self:?}")
    }
}
impl std::error::Error for ProgressError {}

/// A caller-owned simulated-time budget, not a machine driver.
///
/// Derive `deadline` from the workload. Explain `same_instant_rounds` from the
/// fixture's legitimate same-instant transitions; zero requires strict progress.
/// Call [`Self::next`] only while the desired behaviour has not completed:
/// completion exactly at the deadline is valid and requires no further step.
#[derive(Debug)]
pub struct GuestProgress {
    phase: String,
    start: Duration,
    deadline: Duration,
    previous: Option<Duration>,
    same_instant_rounds: u64,
    unchanged: u64,
}
impl GuestProgress {
    pub fn new(
        phase: impl Into<String>,
        start: Duration,
        deadline: Duration,
        same_instant_rounds: u64,
    ) -> Result<Self, ProgressError> {
        let phase = phase.into();
        if phase.trim().is_empty() || deadline <= start {
            return Err(ProgressError::InvalidBudget);
        }
        Ok(Self {
            phase,
            start,
            deadline,
            previous: None,
            same_instant_rounds,
            unchanged: 0,
        })
    }
    pub fn phase(&self) -> &str {
        &self.phase
    }
    /// Validate this unfinished round's simulated start and cap its next limit.
    /// Errors do not consume or mutate the budget.
    pub fn next(&mut self, now: Duration, quantum: Duration) -> Result<Duration, ProgressError> {
        if quantum.is_zero() {
            return Err(ProgressError::ZeroQuantum);
        }
        if now < self.start || self.previous.is_some_and(|previous| now < previous) {
            return Err(ProgressError::Backwards);
        }
        if now >= self.deadline {
            return Err(ProgressError::Deadline);
        }
        let unchanged = if self.previous == Some(now) {
            self.unchanged
                .checked_add(1)
                .ok_or(ProgressError::NoProgress)?
        } else {
            0
        };
        if unchanged > self.same_instant_rounds {
            return Err(ProgressError::NoProgress);
        }
        self.previous = Some(now);
        self.unchanged = unchanged;
        Ok(now
            .checked_add(quantum)
            .unwrap_or(self.deadline)
            .min(self.deadline))
    }
}

#[cfg(test)]
mod tests;
