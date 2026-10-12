//! Simulation driving, shared time, and barrier contracts.
//!
//! A driver owns scheduling and shared simulation time. Machines own execution
//! and report their progress; the driver coordinates that progress with timers
//! and external events to establish simulation-wide barriers.

use crate::{
    clock::{Clock, SimulationClock},
    machine::{ExitReason, Machine, StopReason, Timestamp},
};
use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

mod playback;
use playback::Playback;
pub use playback::{PlaybackRate, PlaybackStatus};

mod workers;
use workers::Workers;

/// An insertion-ordered machine identity within one driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(usize);

/// A committed simulation position. Zero-time action transitions advance the
/// boundary index without completing a quantum. Repeated observations and waits
/// preserve all three fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub boundary_index: u64,
    pub completed_quanta: u64,
    pub at: Timestamp,
}

struct Entry<M> {
    machine: Option<M>,
    reached: Timestamp,
    stopped: StopReason,
    paused: bool,
}

impl<M> Entry<M> {
    fn next(&self) -> Option<Timestamp> {
        if self.paused {
            return None;
        }
        match self.stopped {
            StopReason::BoundaryReached => Some(self.reached),
            StopReason::Blocked { deadline, .. } => deadline,
            StopReason::Exited { .. } => None,
        }
    }
}

/// A scheduling or lifecycle action applied at a barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Admit execution after a stimulus has been made visible in the machine's
    /// environment. This does not itself deliver a packet or other input data.
    Wake(MachineId),
    /// Withhold this machine's execution, including ordinary wakeups and timers.
    /// Its state and pending input remain retained; it has not exited. Repeated
    /// pauses are harmless and do not call the machine.
    Pause(MachineId),
    /// Resume a paused machine at this action's shared simulation time. A
    /// zero-budget run adopts elapsed time and delivers overdue timers without
    /// replaying paused CPU work. Resuming an active machine is a no-op.
    Resume(MachineId),
    /// Crash the machine without graceful shutdown, including while paused.
    Crash(MachineId),
}

/// Owns simulation scheduling and shared time.
///
/// Execution uses a fixed pool of worker threads. Each worker owns its context;
/// contexts must not contain machine-specific simulation state. Worker count and
/// completion order must not affect simulation results. Machines share the
/// driver's read-only clock, which stays fixed throughout each batch of calls.
///
/// The positive quantum is fixed for the driver's lifetime. Quantum boundaries
/// are anchored at the initial timestamp; intermediate barriers never shift them.
/// The caller queues actions before stepping; actions at one timestamp execute
/// in insertion order, before timer wakeups. Exited machines ignore later actions.
/// Paused machines remain unfinished, with no run or timer delivery until a
/// resume action adopts the current shared time. Pausing never freezes the clock
/// or shifts timer deadlines. Wake actions leave input retained while paused;
/// crash actions still terminate paused machines.
/// Machine-produced communication is not routed by this driver yet.
pub struct Driver<M: Machine> {
    clock: Arc<SimulationClock>,
    workers: Workers<M::Context>,
    quantum_ns: u64,
    quantum_end: Timestamp,
    machines: Vec<Entry<M>>,
    actions: BTreeMap<Timestamp, VecDeque<Action>>,
    boundary_index: u64,
    completed_quanta: u64,
    quantum_committed: bool,
    playback: Playback,
    failed: bool,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Construct the initial ordered set of machines using the private clock.
    ///
    /// The factory runs once at the initial barrier and must return newly
    /// constructed machines at `start`. Its vector order breaks scheduling ties.
    /// This keeps clock injection in construction without exposing a clock getter.
    ///
    /// `quantum` is a positive duration representable in u64 nanoseconds; its
    /// first boundary must fit after `start`. `workers` must be nonzero. Invalid
    /// configuration is rejected before either factory is called.
    ///
    /// The context factory runs once on each worker thread, before constructing
    /// machines. That context is used and destroyed on the same thread, so it
    /// need not be Send. Factories and contexts must not make simulation behavior
    /// depend on worker identity or host timing. Machines can move between the
    /// caller and workers; their errors must also be transferable between threads.
    /// Initialization failure joins all started workers before returning.
    pub fn new(
        start: Timestamp,
        quantum: Duration,
        workers: usize,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        machines: impl FnOnce(Arc<dyn Clock + Send + Sync>) -> Result<Vec<M>, M::Error>,
    ) -> Result<Self, Error<M::Error>> {
        let quantum_ns = u64::try_from(quantum.as_nanos())
            .ok()
            .filter(|&n| n > 0)
            .ok_or(Error::InvalidQuantum)?;
        let quantum_end = Timestamp::from_nanos(
            start
                .as_nanos()
                .checked_add(quantum_ns)
                .ok_or(Error::TimeOverflow)?,
        );
        if workers == 0 {
            return Err(Error::InvalidWorkerCount);
        }
        let workers = Workers::new(workers, context)?;
        let clock = Arc::new(SimulationClock::new(start.as_nanos()));
        let machines = machines(clock.clone()).map_err(Error::Construction)?;
        Ok(Self {
            clock,
            workers,
            quantum_ns,
            quantum_end,
            machines: machines
                .into_iter()
                .map(|machine| Entry {
                    machine: Some(machine),
                    reached: start,
                    stopped: StopReason::BoundaryReached,
                    paused: false,
                })
                .collect(),
            actions: BTreeMap::new(),
            boundary_index: 0,
            completed_quanta: 0,
            quantum_committed: false,
            playback: Playback::default(),
            failed: false,
        })
    }

    /// Access the simulation at its initial or most recent successful barrier.
    ///
    /// The callback has exclusive access while execution is stopped. Machine
    /// references cannot escape it, and stepping cannot resume until it returns.
    /// Machine additions and scheduling changes are available only through this
    /// scope. A failed driver does not admit another barrier callback.
    ///
    /// ```compile_fail
    /// use seedbox_machine::{driver::{Driver, MachineId}, machine::Machine};
    /// fn escape<M>(driver: &mut Driver<M>, id: MachineId) -> &M
    /// where M: Machine + Send + 'static, M::Context: 'static, M::Error: Send + 'static {
    ///     driver.with_barrier(|barrier| barrier.machine(id)).ok().unwrap()
    /// }
    /// ```
    pub fn with_barrier<R>(
        &mut self,
        operation: impl for<'a> FnOnce(&mut Barrier<'a, M>) -> Result<R, Error<M::Error>>,
    ) -> Result<R, Error<M::Error>> {
        self.check_valid()?;
        operation(&mut Barrier { driver: self })
    }

    fn now(&self) -> Timestamp {
        Timestamp::from_nanos(self.clock.now())
    }

    /// Queue a barrier action. Past times and unknown machine identities are
    /// rejected without changing the queue. Actions at `now` run on the next step.
    fn schedule(&mut self, at: Timestamp, action: Action) -> Result<(), Error<M::Error>> {
        self.check_valid()?;
        if at < self.now() {
            return Err(Error::PastTime {
                now: self.now(),
                requested: at,
            });
        }
        let (Action::Wake(id) | Action::Pause(id) | Action::Resume(id) | Action::Crash(id)) =
            action;
        self.entry(id)?;
        self.actions.entry(at).or_default().push_back(action);
        Ok(())
    }

    /// Advance the simulation to its next barrier, no later than `upto`.
    ///
    /// This call is synchronous. On success, all machine execution is stopped
    /// and the returned time is the shared simulation time. The caller may
    /// inspect state or apply lifecycle operations through [`Driver::with_barrier`]
    /// before driving again. Playback controls withhold execution at these
    /// barriers; host pacing never changes simulation execution bounds. A call
    /// never sleeps. Process controls or cancellation between calls, and wait
    /// for the suggested delay when playback returns `Waiting`.
    ///
    /// A pre-action stop returns before the retained action takes effect, also
    /// for actions queued at the initial/current timestamp. Selecting a running
    /// rate or admitting a quantum resumes that action exactly once. Same-time
    /// action order and subsequent timer wakeups are preserved.
    ///
    /// # Time and execution boundaries
    ///
    /// All timestamps use simulation time, before guest clock offset or skew.
    /// `upto` must be at or after the current shared time. Returned times never
    /// move backwards or exceed `upto`. A step may stop earlier for a scheduled
    /// event or execution boundary, or because work becomes blocked or finishes.
    /// A call never crosses the next fixed quantum boundary. A shorter `upto`,
    /// timer, or action may produce an intermediate barrier, but the unfinished
    /// quantum retains its original end. Repeated short calls therefore visit
    /// the same quantum boundaries as larger calls. Quantum changes require a
    /// new driver; there is no runtime setter.
    ///
    /// The driver chooses execution bounds that allow every machine to stop at
    /// a common barrier. At the returned time, runnable machines have reached
    /// that barrier; paused, blocked or exited machines may retain an earlier
    /// execution position. No machine state or external effect beyond the barrier may be
    /// exposed. One machine stopping early does not establish a global barrier.
    ///
    /// Shared time advances only after all earlier runnable work and events
    /// have been accounted for. When all machines are blocked, the driver may
    /// skip idle time up to the next timer, external event, or `upto`, whichever
    /// comes first. It must not skip pending work or let one machine's progress
    /// advance the clock past another machine's work.
    ///
    /// # Events and caller responsibilities
    ///
    /// Except at an explicit `BeforeAction` stop, events due at a returned
    /// barrier are applied in the simulation's defined order before returning. Their effects are visible to subsequent machine
    /// execution. The ordering must not depend on host timing or worker assignment.
    /// Future events remain pending. Events newly supplied by the caller at the
    /// current barrier must be applied before further machine execution.
    ///
    /// The caller must prevent concurrent modification of machines or their
    /// environment during `step`. Lifecycle operations occur only at barriers
    /// and must update the driver's scheduling state before execution resumes.
    ///
    /// # Handling the result
    ///
    /// - [`StepOutcome::Waiting`]: playback is paused or host pacing is waiting.
    ///   Keep servicing controls and cancellation; no simulation work occurred.
    /// - [`StepOutcome::BeforeAction`]: the next effective crash is retained.
    ///   Inspect its committed position, then resume playback or admit a quantum.
    /// - [`StepOutcome::Stopped`]: permanent stop prevents any further execution.
    /// - [`StepOutcome::BarrierReached`]: the simulation can make further
    ///   progress, immediately or at a known future event. Inspect or modify it
    ///   at this barrier, then call `step` again when execution is permitted.
    /// - [`StepOutcome::Blocked`]: unfinished machines remain, but no runnable
    ///   work, timer, or queued event can advance them. Wait for external input
    ///   or a lifecycle change; do not busy-poll or infer that the test passed.
    /// - [`StepOutcome::Finished`]: every machine has exited and no scheduled
    ///   work remains. Stop driving and evaluate the simulation's result.
    ///   Completion alone does not establish whether the test passed or failed.
    ///
    /// With `upto` equal to the current time, the driver may apply events due
    /// now but must not advance execution or time. It may return the same
    /// barrier without progress; repeating that bound does not advance the
    /// simulation. Already applied events must not be applied again.
    ///
    /// # Errors
    ///
    /// `Err` means the driver could not preserve this contract. It must stop
    /// further machine execution before returning, but does not promise rollback
    /// or a resumable simulation. Every submitted call finishes before an error
    /// is returned; no new batch is submitted after a failure. When several
    /// machines fail in one batch, the lowest machine ID determines the reported
    /// error, independently of completion order. Worker execution panics are
    /// reported as errors. The caller must stop driving rather than retrying.
    /// Machine exits are simulation outcomes, not driver errors.
    pub fn step(&mut self, upto: Timestamp) -> Result<StepOutcome, Error<M::Error>> {
        self.step_at(upto, Instant::now())
    }

    fn step_at(
        &mut self,
        upto: Timestamp,
        host_now: Instant,
    ) -> Result<StepOutcome, Error<M::Error>> {
        self.check_valid()?;
        let result = (|| {
            if upto < self.now() {
                return Err(Error::PastTime {
                    now: self.now(),
                    requested: upto,
                });
            }
            if self.playback.stopped {
                return Ok(StepOutcome::Stopped { at: self.now() });
            }
            if matches!(self.outcome(), StepOutcome::Finished { .. }) {
                self.playback.finish();
                self.playback.before_next = false;
                return Ok(self.outcome());
            }
            let position = self.position();
            if let Some(delay) = self.playback.delay(position, host_now) {
                return Ok(match self.playback.held {
                    Some(action) => StepOutcome::BeforeAction {
                        at: self.now(),
                        action,
                    },
                    None => StepOutcome::Waiting {
                        at: self.now(),
                        delay,
                    },
                });
            }
            let outcome = self.step_inner(upto, host_now)?;
            let position = self.position();
            self.playback.finish_quantum(position);
            if matches!(outcome, StepOutcome::Finished { .. }) {
                self.playback.finish();
                self.playback.before_next = false;
            }
            Ok(outcome)
        })();
        if result.is_err() {
            // All submitted synchronous calls have finished before poisoning.
            self.failed = true;
        }
        result
    }

    fn position(&self) -> Position {
        Position {
            boundary_index: self.boundary_index,
            completed_quanta: self.completed_quanta,
            at: self.now(),
        }
    }

    fn commit_boundary(&mut self) -> Result<(), Error<M::Error>> {
        self.boundary_index = self
            .boundary_index
            .checked_add(1)
            .ok_or(Error::PositionOverflow)?;
        if self.now() == self.quantum_end && !self.quantum_committed {
            self.completed_quanta = self
                .completed_quanta
                .checked_add(1)
                .ok_or(Error::PositionOverflow)?;
            self.quantum_committed = true;
        }
        Ok(())
    }

    fn step_inner(
        &mut self,
        upto: Timestamp,
        host_now: Instant,
    ) -> Result<StepOutcome, Error<M::Error>> {
        if let Some(action) = self.apply_due(host_now)? {
            return Ok(StepOutcome::BeforeAction {
                at: self.now(),
                action,
            });
        }
        if upto == self.now() || self.is_quiescent() {
            return Ok(self.outcome());
        }

        if self.now() == self.quantum_end {
            self.quantum_committed = false;
            self.quantum_end = Timestamp::from_nanos(
                self.quantum_end
                    .as_nanos()
                    .checked_add(self.quantum_ns)
                    .ok_or(Error::TimeOverflow)?,
            );
        }

        // Known events limit the whole round. A timer discovered during a run
        // can shorten another machine's next budget, but cannot invalidate the
        // round's final boundary. Intermediate local progress stays private.
        let boundary = self
            .machines
            .iter()
            .filter(|entry| !entry.paused)
            .filter_map(|entry| match entry.stopped {
                StopReason::Blocked { deadline, .. } => deadline,
                _ => None,
            })
            .chain(self.actions.first_key_value().map(|(&at, _)| at))
            .fold(upto.min(self.quantum_end), Timestamp::min);

        let initial = self.now();
        let mut ran = false;
        while let Some((_, now)) = self.next() {
            if now >= boundary {
                break;
            }
            self.clock.set(now.as_nanos());
            let limit = self
                .machines
                .iter()
                .filter_map(Entry::next)
                .filter(|&at| at > now)
                .min()
                .map_or(boundary, |at| at.min(boundary));
            let ready = self
                .machines
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.next() == Some(now))
                .map(|(index, _)| MachineId(index))
                .collect::<Vec<_>>();
            self.run_batch(&ready, limit)?;
            ran = true;
        }

        // Finish early if all machines exited or became indefinitely blocked.
        // A queued action still keeps time moving toward its own barrier.
        let at = if self.is_quiescent() {
            self.machines
                .iter()
                .map(|entry| entry.reached)
                .max()
                .unwrap_or(self.now())
                .max(self.now())
        } else {
            boundary
        };
        self.clock.set(at.as_nanos());
        if at != initial || ran {
            self.commit_boundary()?;
        }
        if let Some(action) = self.apply_due(host_now)? {
            return Ok(StepOutcome::BeforeAction { at, action });
        }
        Ok(self.outcome())
    }

    fn run_batch(&mut self, ids: &[MachineId], upto: Timestamp) -> Result<(), Error<M::Error>> {
        if ids.is_empty() {
            return Ok(());
        }
        let now = self.now();
        let (send, receive) = mpsc::channel();
        let mut submission_error = None;
        for &id in ids {
            let mut machine = self.machines[id.0].machine.take().expect("stopped machine");
            let result = send.clone();
            if let Err(error) = self.workers.submit(id.0, move |context| {
                let stopped = catch_unwind(AssertUnwindSafe(|| machine.run(context, upto)))
                    .map_err(|_| Error::Panicked { id })
                    .and_then(|result| result.map_err(|source| Error::Machine { id, source }));
                let _ = result.send((id, machine, stopped));
            }) {
                submission_error = Some(error);
                break;
            }
        }
        drop(send);
        // All submitted jobs must finish before changing the clock or returning.
        // Results are ordered by identity, never by worker completion time.
        let mut reports: Vec<_> = receive.into_iter().collect();
        reports.sort_by_key(|(id, _, _)| *id);
        let mut stops = Vec::with_capacity(reports.len());
        for (id, machine, stopped) in reports {
            self.machines[id.0].machine = Some(machine);
            stops.push((id, stopped));
        }
        if let Some(error) = submission_error {
            return Err(error);
        }
        let mut accepted = Vec::with_capacity(stops.len());
        for (id, result) in stops {
            let stopped = result?;
            let reached = match stopped {
                StopReason::BoundaryReached => upto,
                StopReason::Blocked { reached, deadline } => {
                    if deadline.is_some_and(|at| at <= reached.max(now)) {
                        return Err(Error::InvalidStop { id, stopped });
                    }
                    reached
                }
                StopReason::Exited { reached, .. } => reached,
            };
            if reached < self.machines[id.0].reached || reached > upto {
                return Err(Error::InvalidStop { id, stopped });
            }
            accepted.push((id, reached, stopped));
        }
        for (id, reached, stopped) in accepted {
            let entry = &mut self.machines[id.0];
            entry.reached = reached;
            entry.stopped = stopped;
        }
        Ok(())
    }

    fn apply_due(&mut self, host_now: Instant) -> Result<Option<Action>, Error<M::Error>> {
        let now = self.now();
        let mut waking = Vec::new();
        // Keep the front action until its effect is admitted. Same-time actions
        // retain their order, even across a pre-effect stop and zero-time calls.
        while let Some(action) = self
            .actions
            .get(&now)
            .and_then(|actions| actions.front())
            .copied()
        {
            let (Action::Wake(id) | Action::Pause(id) | Action::Resume(id) | Action::Crash(id)) =
                action;
            // A held occurrence was already intercepted. Rearming resumes it
            // and targets the next actual crash, including at this timestamp.
            let was_held = self.playback.held.take().is_some();
            let exited = matches!(self.machines[id.0].stopped, StopReason::Exited { .. });
            if self.playback.before_next
                && !was_held
                && !exited
                && matches!(action, Action::Crash(_))
            {
                let position = self.position();
                self.playback.hold(action, position, host_now);
                return Ok(Some(action));
            }
            let actions = self.actions.get_mut(&now).expect("due actions");
            actions.pop_front();
            if actions.is_empty() {
                self.actions.remove(&now);
            }
            let entry = &mut self.machines[id.0];
            if !matches!(entry.stopped, StopReason::Exited { .. }) {
                match action {
                    Action::Wake(_) if !entry.paused => self.run_batch(&[id], now)?,
                    Action::Pause(_) => entry.paused = true,
                    Action::Resume(_) if entry.paused => {
                        entry.paused = false;
                        // Adoption services elapsed timers/input at this barrier;
                        // the zero budget executes no paused CPU work.
                        self.run_batch(&[id], now)?;
                    }
                    Action::Wake(_) | Action::Resume(_) => {}
                    Action::Crash(_) => {
                        entry.paused = false;
                        entry
                            .machine
                            .as_mut()
                            .expect("stopped machine")
                            .crash()
                            .map_err(|source| Error::Machine { id, source })?;
                        entry.reached = now;
                        entry.stopped = StopReason::Exited {
                            reached: now,
                            reason: ExitReason::Crashed,
                        };
                    }
                }
            }
            self.commit_boundary()?;
        }
        for index in 0..self.machines.len() {
            let stopped = self.machines[index].stopped;
            if self.machines[index].paused || matches!(stopped, StopReason::Exited { .. }) {
                continue;
            }
            if matches!(stopped,
                StopReason::Blocked { deadline: Some(at), .. } if at == now)
            {
                // Let the machine observe the new clock and deliver its timer,
                // without executing beyond this barrier.
                waking.push(MachineId(index));
            }
        }
        self.run_batch(&waking, now)?;
        if !waking.is_empty() {
            self.commit_boundary()?;
        }
        Ok(None)
    }

    fn next(&self) -> Option<(MachineId, Timestamp)> {
        self.machines
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.next().map(|at| (MachineId(index), at)))
            .min_by_key(|&(id, at)| (at, id))
    }

    fn is_quiescent(&self) -> bool {
        self.next().is_none() && self.actions.is_empty()
    }

    fn outcome(&self) -> StepOutcome {
        let at = self.now();
        if !self.is_quiescent() {
            StepOutcome::BarrierReached { at }
        } else if self
            .machines
            .iter()
            .all(|entry| matches!(entry.stopped, StopReason::Exited { .. }))
        {
            StepOutcome::Finished { at }
        } else {
            StepOutcome::Blocked { at }
        }
    }

    fn check_valid(&self) -> Result<(), Error<M::Error>> {
        if self.failed {
            Err(Error::Failed)
        } else {
            Ok(())
        }
    }

    fn entry(&self, id: MachineId) -> Result<&Entry<M>, Error<M::Error>> {
        self.machines.get(id.0).ok_or(Error::UnknownMachine(id))
    }
}

/// Scoped access granted by the driver while all machines are stopped.
///
/// This handle cannot be constructed by callers or retained after its callback.
/// It exposes observation and mediated operations, never unrestricted mutable
/// machine access that could invalidate the driver's scheduling state.
pub struct Barrier<'a, M: Machine> {
    driver: &'a mut Driver<M>,
}

impl<M> Barrier<'_, M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Observe playback and the committed simulation position without progress.
    pub fn playback(&self) -> PlaybackStatus {
        self.driver.playback.status(self.driver.position())
    }

    /// Select host pacing from this boundary. This cancels a pending single
    /// quantum and a one-shot pre-action arm. It never advances simulation time.
    /// An explicit stop cannot be resumed by selecting a running rate.
    pub fn set_rate(&mut self, rate: PlaybackRate) -> Result<(), Error<M::Error>> {
        self.check_running()?;
        let position = self.driver.position();
        self.driver
            .playback
            .set_rate(rate, position, Instant::now());
        Ok(())
    }

    /// While paused, admit execution through exactly the next fixed quantum
    /// boundary. Intermediate barriers, including zero-time actions, do not
    /// consume this admission. Completion becomes visible in `playback()` after
    /// the boundary commits. Early finish cancels the admission; blocking keeps
    /// it pending until input arrives or the caller cancels it with `set_rate`.
    /// A second pending admission is rejected. This clears a pre-action arm.
    pub fn step_quantum(&mut self) -> Result<(), Error<M::Error>> {
        self.check_running()?;
        if matches!(self.driver.outcome(), StepOutcome::Finished { .. }) {
            return Err(Error::Completed);
        }
        let position = self.driver.position();
        self.driver.playback.step(position)
    }

    /// Run at a positive rate until just before the next queued `Action::Crash`.
    /// Ordinary wakeups, pause/resume actions, and crashes of already exited
    /// machines do not consume the arm. Rearming while held resumes the held
    /// occurrence and stops before the next effective crash, including at the
    /// same timestamp. The arm is consumed once, even at the current time. The
    /// action and all following same-time actions remain pending until playback
    /// resumes or a quantum is admitted. While held, only observation and
    /// playback controls are allowed through this handle.
    pub fn pause_before_next_crash(&mut self, rate: PlaybackRate) -> Result<(), Error<M::Error>> {
        self.check_running()?;
        if rate == PlaybackRate::Paused {
            return Err(Error::InvalidRate);
        }
        self.set_rate(rate)?;
        self.driver.playback.before_next = true;
        Ok(())
    }

    /// Permanently stop this driver at its current barrier. This is idempotent
    /// and performs no machine work, crash, or cleanup of committed state.
    /// A running synchronous `step` cannot receive controls through this handle;
    /// callers cooperate by checking cancellation between calls. There is no
    /// promise to interrupt a hung `Machine::run`.
    pub fn stop(&mut self) {
        self.driver.playback.stopped = true;
        self.driver.playback.finish();
        self.driver.playback.before_next = false;
    }

    fn check_running(&self) -> Result<(), Error<M::Error>> {
        if self.driver.playback.stopped {
            Err(Error::Stopped)
        } else {
            Ok(())
        }
    }

    fn check_mutation(&self) -> Result<(), Error<M::Error>> {
        self.check_running()?;
        if self.driver.playback.held.is_some() {
            Err(Error::BeforeAction)
        } else {
            Ok(())
        }
    }

    /// Observe machines in their stable insertion order.
    pub fn machines(&self) -> impl ExactSizeIterator<Item = (MachineId, &M)> {
        self.driver
            .machines
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    MachineId(index),
                    entry.machine.as_ref().expect("stopped machine"),
                )
            })
    }

    pub fn machine(&self, id: MachineId) -> Result<&M, Error<M::Error>> {
        Ok(self
            .driver
            .entry(id)?
            .machine
            .as_ref()
            .expect("stopped machine"))
    }

    pub fn stopped(&self, id: MachineId) -> Result<StopReason, Error<M::Error>> {
        Ok(self.driver.entry(id)?.stopped)
    }

    /// Observe whether the driver is withholding this machine's execution.
    /// This is separate from its last `StopReason`: paused machines retain their
    /// last accepted state and execution time until resume. Exited machines are
    /// never logically paused. Pause/resume changes use queued `Action`s and
    /// take effect in insertion order when the next step admits their barrier.
    pub fn paused(&self, id: MachineId) -> Result<bool, Error<M::Error>> {
        Ok(self.driver.entry(id)?.paused)
    }

    /// Construct and admit a machine at this barrier's time. Construction
    /// failure leaves the existing set unchanged. The returned identity follows
    /// all earlier additions; removal and identity reuse are not supported.
    pub fn add(
        &mut self,
        machine: impl FnOnce(Arc<dyn Clock + Send + Sync>) -> Result<M, M::Error>,
    ) -> Result<MachineId, Error<M::Error>> {
        self.check_mutation()?;
        let machine = machine(self.driver.clock.clone()).map_err(Error::Construction)?;
        let id = MachineId(self.driver.machines.len());
        self.driver.machines.push(Entry {
            machine: Some(machine),
            reached: self.driver.now(),
            stopped: StopReason::BoundaryReached,
            paused: false,
        });
        Ok(id)
    }

    /// Capture one machine without changing its scheduling state or running it,
    /// including while paused. Reconstructing a whole simulation also requires
    /// its environment and driver-owned scheduling/lifecycle state, including
    /// the pause flag observed through `paused`.
    pub fn snapshot(&mut self, id: MachineId) -> Result<M::Snapshot, Error<M::Error>> {
        self.check_mutation()?;
        self.driver.entry(id)?;
        self.driver.machines[id.0]
            .machine
            .as_mut()
            .expect("stopped machine")
            .snapshot()
            .map_err(|source| Error::Machine { id, source })
    }

    /// Queue an action in simulation order. Actions at the current barrier are
    /// applied by the next `step`, before any further execution.
    pub fn schedule(&mut self, at: Timestamp, action: Action) -> Result<(), Error<M::Error>> {
        self.check_mutation()?;
        self.driver.schedule(at, action)
    }
}

/// Driver setup, execution, or contract failure.
#[derive(Debug)]
pub enum Error<E> {
    /// A machine factory failed before its machines were admitted.
    Construction(E),
    /// Quantum must be positive and representable in u64 nanoseconds.
    InvalidQuantum,
    /// At least one execution worker is required.
    InvalidWorkerCount,
    /// Committed position counters cannot represent another boundary.
    PositionOverflow,
    /// A quantum can only be admitted while playback is paused.
    NotPaused,
    /// A previously admitted quantum has not completed.
    StepPending,
    /// A pre-action arm requires a positive playback rate.
    InvalidRate,
    /// Every machine has exited and no queued work remains.
    Completed,
    /// A permanent stop prohibits execution and scheduling changes.
    Stopped,
    /// Only observation and playback controls are allowed before a held action.
    BeforeAction,
    /// The next full quantum boundary cannot be represented.
    TimeOverflow,
    /// A worker's context factory failed.
    Context {
        worker: usize,
        source: E,
    },
    /// A host thread could not start or accept work.
    Worker {
        worker: usize,
        source: std::io::Error,
    },
    /// Execution panicked; the machine is not resumable.
    Panicked {
        id: MachineId,
    },
    PastTime {
        now: Timestamp,
        requested: Timestamp,
    },
    UnknownMachine(MachineId),
    Machine {
        id: MachineId,
        source: E,
    },
    InvalidStop {
        id: MachineId,
        stopped: StopReason,
    },
    /// A previous step failed; further execution is prohibited.
    Failed,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidQuantum => {
                f.write_str("quantum must be positive and fit in u64 nanoseconds")
            }
            Self::InvalidWorkerCount => f.write_str("worker count must be positive"),
            Self::PositionOverflow => f.write_str("simulation position counter overflow"),
            Self::NotPaused => f.write_str("playback must be paused before admitting a quantum"),
            Self::StepPending => f.write_str("a quantum admission is already pending"),
            Self::InvalidRate => f.write_str("pre-action playback rate must be positive"),
            Self::Completed => f.write_str("simulation has completed"),
            Self::Stopped => f.write_str("driver has been permanently stopped"),
            Self::BeforeAction => {
                f.write_str("only playback controls are allowed before the held action")
            }
            Self::TimeOverflow => f.write_str("quantum boundary exceeds the simulation time range"),
            Self::Context { worker, source } => write!(f, "worker {worker} context: {source}"),
            Self::Worker { worker, source } => write!(f, "worker {worker}: {source}"),
            Self::Panicked { id } => write!(f, "machine {} panicked during execution", id.0),
            Self::Construction(source) => write!(f, "machine construction: {source}"),
            Self::PastTime { now, requested } => write!(
                f,
                "time {} precedes current time {}",
                requested.as_nanos(),
                now.as_nanos()
            ),
            Self::UnknownMachine(id) => write!(f, "unknown machine {}", id.0),
            Self::Machine { id, source } => write!(f, "machine {}: {source}", id.0),
            Self::InvalidStop { id, stopped } => {
                write!(f, "machine {} returned an invalid stop: {stopped:?}", id.0)
            }
            Self::Failed => f.write_str("driver cannot execute after a failed step"),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for Error<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Construction(source)
            | Self::Machine { source, .. }
            | Self::Context { source, .. } => Some(source),
            Self::Worker { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Why a driver returned control at a simulation-wide barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// Playback withholds execution because it is paused or a limited rate is
    /// waiting for host time. `Barrier::playback` distinguishes those states.
    /// The suggested delay is bounded for servicing controls; `step` never sleeps.
    Waiting { at: Timestamp, delay: Duration },
    /// The arm was consumed and this queued action has not taken effect.
    /// Repeated calls while paused return the same position and action.
    BeforeAction { at: Timestamp, action: Action },
    /// A permanent stop was requested at a barrier. No machine has been crashed.
    Stopped { at: Timestamp },
    /// Further work is runnable or has a known future scheduling time.
    BarrierReached { at: Timestamp },
    /// Unfinished machines can make no progress without new external input.
    Blocked { at: Timestamp },
    /// Every machine has exited and no scheduled work remains.
    Finished { at: Timestamp },
}

#[cfg(test)]
mod tests;
