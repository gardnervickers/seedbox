//! Simulation driving, shared time, and barrier contracts.
//!
//! A driver owns scheduling and shared simulation time. Machines own execution
//! and report their progress; the driver coordinates that progress with timers
//! and external events to establish simulation-wide barriers.

use crate::{
    clock::{Clock, SimulationClock},
    definition::MachineDefinition,
    execution::ExecutionConfig,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Input, Outbox, Request, Response},
    rng::{RngStream, SeededRng},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    num::NonZeroU32,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

mod control;
mod playback;
use playback::Playback;
pub use playback::{PlaybackRate, PlaybackStatus};

#[cfg(test)]
use crate::pv::Output;

mod workers;
use workers::Workers;

/// An insertion-ordered machine identity within one driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(usize);

/// One committed response, associated with the machine boot that emitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineResponse {
    pub machine: MachineId,
    pub boot: u64,
    pub at: Timestamp,
    pub payload: Response,
}

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
    rng: Option<SeededRng>,
    execution: ExecutionConfig,
    reached: Timestamp,
    stopped: StopReason,
    paused: bool,
    inbox: VecDeque<Input>,
    staged: Vec<MachineResponse>,
    boot: u64,
}

#[derive(Clone, Copy)]
enum DriverEvent {
    Wake(MachineId),
    Pause(MachineId),
    Resume(MachineId),
    SetExecution {
        machine: MachineId,
        config: ExecutionConfig,
    },
}

#[derive(Clone)]
enum Scheduled {
    Driver(DriverEvent),
    Control(control::Event),
    Input { id: MachineId, input: Input },
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
    /// Set the execution configuration at this action's barrier, in FIFO order.
    /// Applies while active, paused or exited and survives crash and reboot.
    /// Global playback pause still withholds timeline admission.
    /// The setting does not execute or wake the machine, change its pause state,
    /// adopt time or shift timers. Subsequent calls receive this configuration;
    /// already charged partial work retains its original cost. The default
    /// configuration restores nominal execution cost.
    SetExecution {
        /// Machine whose subsequent execution receives the configuration.
        machine: MachineId,
        /// Validated execution configuration owned by the driver.
        config: ExecutionConfig,
    },
    /// Crash the machine without graceful shutdown, including while paused.
    Crash(MachineId),
    /// Request reset of a live, exited, or crashed machine at this
    /// barrier, clearing its driver pause and old blocked/timer state. Reboot
    /// executes no guest work and preserves this identity and all queued actions;
    /// later actions target the new boot. Repeated actions perform separate
    /// resets in distinct zero-budget admission runs. Failure poisons the driver
    /// without promising rollback.
    Reboot(MachineId),
    /// Latch a graceful shutdown request for the current boot. Repeats and
    /// already-exited machines retain their exit state. All machines receive a
    /// zero-budget admission run, including while paused; this may deliver the
    /// request and refresh scheduling without executing guest instructions.
    /// Paused machines remain paused until resume.
    /// Subsequent runs drive graceful exit. Crash and reboot discard the request.
    Shutdown(MachineId),
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
/// in insertion order, before timer wakeups. Execution settings apply in every
/// machine state and survive lifecycle changes without executing the machine.
/// Exited machines ignore ordinary wake/pause/resume actions; queued lifecycle
/// requests still receive zero-budget admission, and reboot admits a fresh boot.
/// Paused machines remain unfinished and receive no positive execution budget.
/// Due requests still receive zero-budget admission runs, which adopt shared
/// time and make due timers eligible without executing program instructions.
/// Pausing never freezes the clock or shifts timer deadlines. Wake actions leave input retained while paused;
/// crash actions still terminate paused machines.
/// Guest simulation-control calls are interpreted after their full round commits.
/// Their effects and replies join the same ordered timeline at that barrier;
/// draining responses only observes them. Accepted control work survives caller
/// loss. Calls generated together are queued before their generated effects;
/// dependent operations must await acknowledgment rather than assume that a
/// concurrent Pause/Resume sequence succeeds. Oracle completion publishes the full response batch before stopping
/// further admission.
pub struct Driver<M: Machine> {
    clock: Arc<SimulationClock>,
    rng: Box<dyn RngStream + Send>,
    workers: Workers<M::Context>,
    quantum_ns: u64,
    quantum_end: Timestamp,
    machines: Vec<Entry<M>>,
    events: BTreeMap<Timestamp, VecDeque<Scheduled>>,
    committed_outputs: Vec<MachineResponse>,
    boundary_index: u64,
    completed_quanta: u64,
    quantum_committed: bool,
    playback: Playback,
    failed: bool,
    oracle: MachineId,
    finished: Option<(Timestamp, ExitReason)>,
    oracle_stops: BTreeSet<(u64, NonZeroU32)>,
    control: control::Control,
    trigger_marker: Option<Timestamp>,
    definitions: Vec<MachineDefinition>,
    factory: Box<dyn FnMut(&MachineDefinition) -> Result<M, M::Error> + Send>,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Construct machines and driver-owned simulation dependencies.
    ///
    /// The factory constructs the required oracle first, followed by the other
    /// definitions in list order. The driver retains the factory and definitions.
    /// The factory must return newly configured machines without executing them. This order
    /// breaks scheduling ties and assigns
    /// independent machine RNG streams by drawing one little-endian u64 seed
    /// from the simulation RNG for each admitted machine. Failed additions draw
    /// no seed. Only run calls borrow the clock and the machine's continuing RNG.
    /// Machine-only snapshots do not capture either RNG cursor.
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
        rng: impl RngStream + Send + 'static,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        oracle: MachineDefinition,
        machines: Vec<MachineDefinition>,
        mut factory: impl FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
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
        let definitions: Vec<_> = std::iter::once(oracle).chain(machines).collect();
        let machines = definitions
            .iter()
            .map(&mut factory)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Construction)?;
        let mut rng: Box<dyn RngStream + Send> = Box::new(rng);
        let mut draw_seed = || {
            let mut bytes = [0; 8];
            rng.fill_bytes(&mut bytes);
            u64::from_le_bytes(bytes)
        };
        let machines = machines
            .into_iter()
            .map(|machine| Entry {
                machine: Some(machine),
                rng: Some(SeededRng::new(draw_seed())),
                execution: ExecutionConfig::default(),
                reached: start,
                stopped: StopReason::BoundaryReached,
                paused: false,
                inbox: VecDeque::new(),
                staged: Vec::new(),
                boot: 0,
            })
            .collect();
        Ok(Self {
            clock,
            workers,
            quantum_ns,
            quantum_end,
            machines,
            rng,
            events: BTreeMap::new(),
            committed_outputs: Vec::new(),
            boundary_index: 0,
            completed_quanta: 0,
            quantum_committed: false,
            playback: Playback::default(),
            failed: false,
            oracle: MachineId(0),
            finished: None,
            oracle_stops: BTreeSet::new(),
            control: control::Control::new(&definitions, start),
            trigger_marker: None,
            definitions,
            factory: Box::new(factory),
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
        let (Action::Wake(id)
        | Action::Pause(id)
        | Action::Resume(id)
        | Action::Crash(id)
        | Action::Reboot(id)
        | Action::Shutdown(id)
        | Action::SetExecution { machine: id, .. }) = action;
        self.entry(id)?;
        let scheduled = match action {
            Action::Wake(id) => Scheduled::Driver(DriverEvent::Wake(id)),
            Action::Pause(id) => Scheduled::Driver(DriverEvent::Pause(id)),
            Action::Resume(id) => Scheduled::Driver(DriverEvent::Resume(id)),
            Action::SetExecution { machine, config } => {
                Scheduled::Driver(DriverEvent::SetExecution { machine, config })
            }
            Action::Crash(id) => Scheduled::Input {
                id,
                input: Input {
                    at,
                    payload: Request::Crash,
                },
            },
            Action::Reboot(id) => Scheduled::Input {
                id,
                input: Input {
                    at,
                    payload: Request::Reboot,
                },
            },
            Action::Shutdown(id) => Scheduled::Input {
                id,
                input: Input {
                    at,
                    payload: Request::Shutdown,
                },
            },
        };
        self.events.entry(at).or_default().push_back(scheduled);
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
    /// - [`StepOutcome::Finished`]: the oracle application or machine terminated. Its reason is retained
    ///   permanently after publishing the full round's responses. Other machines
    ///   and scheduled work receive no further execution or admission.
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
            if matches!(self.outcome(), StepOutcome::Finished { .. }) {
                self.playback.finish();
                self.playback.before_next = false;
                return Ok(self.outcome());
            }
            if self.playback.stopped {
                return Ok(StepOutcome::Stopped { at: self.now() });
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
            for entry in &mut self.machines {
                entry.staged.clear();
            }
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
        let mut outputs = Vec::new();
        for (index, entry) in self.machines.iter_mut().enumerate() {
            outputs.extend(entry.staged.drain(..).map(|output| {
                debug_assert_eq!(output.machine, MachineId(index));
                output
            }));
        }
        // Stable sorting preserves each machine's emission order at one time.
        outputs.sort_by_key(|output| (output.at, output.machine));
        let mut terminal = None;
        for output in &outputs {
            if output.machine != self.oracle {
                continue;
            }
            match output.payload {
                Response::ApplicationOutcome { code } => {
                    terminal.get_or_insert(ExitReason::ProgramExit {
                        code: i32::from(code),
                    });
                }
                Response::CommandAcknowledged { id, result } => {
                    if self.oracle_stops.remove(&(output.boot, id))
                        && result == crate::pv::CommandResult::Completed
                    {
                        terminal.get_or_insert(ExitReason::Shutdown);
                    }
                }
                _ => {}
            }
        }
        self.committed_outputs.extend(outputs.iter().cloned());
        let mut plans = vec![];
        let mut calls = vec![];
        for output in &outputs {
            plans.extend(
                self.control
                    .observe(output, self.now())
                    .map_err(Error::Control)?,
            );
            if let Response::SimulationControl { id, payload } = &output.payload {
                calls.push(control::Event::Call {
                    source: output.machine,
                    id: *id,
                    observed: output.at.as_nanos(),
                    payload: payload.clone(),
                });
            }
        }
        for index in 0..self.machines.len() {
            plans.extend(
                self.control
                    .terminal(MachineId(index), self.machines[index].stopped, self.now())
                    .map_err(Error::Control)?,
            );
        }
        if self.finished.is_none() {
            let reason = terminal.or_else(|| match self.machines[self.oracle.0].stopped {
                StopReason::Exited { reason, .. } => Some(reason),
                _ => None,
            });
            if let Some(reason) = reason {
                self.finished = Some((self.now(), reason));
            }
        }
        if self.finished.is_some() {
            self.control.service.cut_off_triggers(self.now().as_nanos());
        } else {
            for call in calls {
                self.events
                    .entry(self.now())
                    .or_default()
                    .push_back(Scheduled::Control(call));
            }
            self.control_plans(plans)?;
            let now = self.now().as_nanos();
            let plans = self
                .control
                .invoke_plans(|service, view| service.trigger_effects(view, now))
                .map_err(Error::Control)?;
            self.control_plans(plans)?;
            self.sync_trigger_marker();
        }
        Ok(())
    }

    fn sync_trigger_marker(&mut self) {
        let next = self
            .control
            .service
            .trigger_boundary()
            .map(Timestamp::from_nanos);
        if next == self.trigger_marker {
            return;
        }
        // Only these internally owned markers are retired. External FIFO
        // actions and requests at the same timestamp retain their exact order.
        for events in self.events.values_mut() {
            events.retain(|event| {
                !matches!(
                    event,
                    Scheduled::Control(
                        control::Event::TriggerBoundary | control::Event::TriggerFinalize
                    )
                )
            });
        }
        self.events.retain(|_, events| !events.is_empty());
        self.trigger_marker = next;
        if let Some(at) = next {
            self.events
                .entry(at)
                .or_default()
                .push_back(Scheduled::Control(control::Event::TriggerBoundary));
        }
    }

    fn control_plans(
        &mut self,
        plans: Vec<crate::oracle::service::Plan>,
    ) -> Result<(), Error<M::Error>> {
        use crate::oracle::service::Plan;
        for plan in plans {
            if let Plan::Settled(settlement) = plan {
                for events in self.events.values_mut() {
                    events.retain(|event| !matches!(event, Scheduled::Control(control::Event::Plan(Plan::Deadline { fault, .. })) if *fault == settlement.fault));
                }
                self.events.retain(|_, events| !events.is_empty());
                // Immutable receipt ownership transferred in invoke_plans.
                continue;
            }
            let at = match &plan {
                Plan::Effect { at, .. }
                | Plan::Wake { at, .. }
                | Plan::Deadline { at, .. }
                | Plan::Reply { at, .. } => Timestamp::from_nanos(*at),
                Plan::Settled(_) => unreachable!(),
            };
            if at < self.now() {
                return Err(Error::Control("control plan scheduled in the past".into()));
            }
            self.events
                .entry(at)
                .or_default()
                .push_back(Scheduled::Control(control::Event::Plan(plan)));
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
        if self.finished.is_some() || upto == self.now() || self.is_quiescent() {
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
            .chain(self.events.first_key_value().map(|(&at, _)| at))
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
        let offered_apps: Vec<_> = ids
            .iter()
            .flat_map(|&machine| {
                self.machines[machine.0]
                    .inbox
                    .iter()
                    .filter_map(move |input| match input.payload {
                        Request::Application { id, .. } => Some((machine, id, input.at)),
                        _ => None,
                    })
            })
            .collect();
        let offered_stops: Vec<_> = if ids.contains(&self.oracle) {
            self.machines[self.oracle.0]
                .inbox
                .iter()
                .filter_map(|input| {
                    if let Request::Application {
                        id,
                        command: crate::pv::ApplicationCommand::Stop,
                    } = input.payload
                    {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Vec::new()
        };
        let (send, receive) = mpsc::channel();
        let mut submission_error = None;
        for &id in ids {
            let entry = &mut self.machines[id.0];
            let mut machine = entry.machine.take().expect("stopped machine");
            let mut rng = entry.rng.take().expect("stopped machine RNG");
            let execution = entry.execution;
            let mut inbox = std::mem::take(&mut entry.inbox);
            let mut outputs = Vec::new();
            let clock = self.clock.clone();
            let result = send.clone();
            if let Err(error) = self.workers.submit(id.0, move |context| {
                let stopped = catch_unwind(AssertUnwindSafe(|| {
                    machine.run(
                        &mut RunContext {
                            worker: context,
                            clock: clock.as_ref(),
                            rng: &mut rng,
                            execution,
                            inbox: Inbox::new(&mut inbox, now),
                            outbox: Outbox::new(&mut outputs, now, upto),
                        },
                        upto,
                    )
                }))
                .map_err(|_| Error::Panicked { id })
                .and_then(|result| result.map_err(|source| Error::Machine { id, source }));
                let _ = result.send((id, machine, rng, inbox, outputs, stopped));
            }) {
                submission_error = Some(error);
                break;
            }
        }
        drop(send);
        // All submitted jobs must finish before changing the clock or returning.
        // Results are ordered by identity, never by worker completion time.
        let mut reports: Vec<_> = receive.into_iter().collect();
        reports.sort_by_key(|(id, _, _, _, _, _)| *id);
        let mut stops = Vec::with_capacity(reports.len());
        for (id, machine, rng, inbox, outputs, stopped) in reports {
            self.machines[id.0].machine = Some(machine);
            self.machines[id.0].rng = Some(rng);
            self.machines[id.0].inbox = inbox;
            stops.push((id, outputs, stopped));
        }
        if let Some(error) = submission_error {
            return Err(error);
        }
        let mut accepted = Vec::with_capacity(stops.len());
        for (id, outputs, result) in stops {
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
            if reached < self.machines[id.0].reached.max(now) || reached > upto {
                return Err(Error::InvalidStop { id, stopped });
            }
            if let Some(output) = outputs.iter().find(|output| output.at > reached) {
                return Err(Error::InvalidOutput {
                    id,
                    at: output.at,
                    reached,
                });
            }
            accepted.push((id, reached, stopped, outputs));
        }
        for &(id, command, at) in &offered_apps {
            if !self.machines[id.0].inbox.iter().any(|input| matches!(input.payload, Request::Application { id: pending, .. } if pending == command)) {
                self.control.accepted(id, self.machines[id.0].boot, command, at, upto).map_err(Error::Control)?;
            }
        }
        for (id, reached, stopped, outputs) in accepted {
            let entry = &mut self.machines[id.0];
            if id == self.oracle {
                for &command in &offered_stops {
                    let unconsumed = entry.inbox.iter().any(|input| matches!(input.payload,
                        Request::Application { id, command: crate::pv::ApplicationCommand::Stop } if id == command));
                    if !unconsumed {
                        self.oracle_stops.insert((entry.boot, command));
                    }
                }
            }
            entry.reached = reached;
            entry.stopped = stopped;
            entry
                .staged
                .extend(outputs.into_iter().map(|output| MachineResponse {
                    machine: id,
                    boot: entry.boot,
                    at: output.at,
                    payload: output.payload,
                }));
            if matches!(stopped, StopReason::Exited { .. }) {
                // Only admitted work belongs to this ended boot. Future
                // requests remain ordered in the driver timeline.
                entry.inbox.clear();
            }
        }
        Ok(())
    }

    fn apply_due(&mut self, host_now: Instant) -> Result<Option<Action>, Error<M::Error>> {
        if self.finished.is_some() {
            return Ok(None);
        }
        let now = self.now();
        let mut waking = Vec::new();
        // Keep the front event until its effect is admitted, including across
        // a pre-crash stop. Driver controls and requests share one FIFO timeline.
        while let Some(scheduled) = self
            .events
            .get(&now)
            .and_then(|events| events.front())
            .cloned()
        {
            let crash = match &scheduled {
                Scheduled::Input {
                    id,
                    input:
                        Input {
                            payload: Request::Crash,
                            ..
                        },
                } => Some(*id),
                Scheduled::Control(control::Event::Plan(
                    crate::oracle::service::Plan::Effect {
                        target,
                        boot,
                        effect: crate::oracle::service::Effect::Crash,
                        ..
                    },
                )) => {
                    let id = self
                        .control
                        .target(target)
                        .map_err(|e| Error::Control(e.message))?;
                    (self.machines[id.0].boot == *boot).then_some(id)
                }
                _ => None,
            };
            if let Some(id) = crash {
                // The held cursor names this exact queued request. The public
                // Action is only its playback/status descriptor, not a driver event.
                let was_held = self.playback.held.take().is_some();
                if self.playback.before_next
                    && !was_held
                    && !matches!(self.machines[id.0].stopped, StopReason::Exited { .. })
                {
                    let action = Action::Crash(id);
                    self.playback.hold(action, self.position(), host_now);
                    return Ok(Some(action));
                }
            }
            let events = self.events.get_mut(&now).expect("due events");
            events.pop_front();
            if events.is_empty() {
                self.events.remove(&now);
            }
            let mut service_effect = None;
            let mut service_origin = None;
            let mut accepted_fault = None;
            let scheduled = match scheduled {
                Scheduled::Control(event) => {
                    use crate::oracle::service::{Effect, Plan};
                    match event {
                        control::Event::Call {
                            source,
                            id,
                            observed,
                            payload,
                        } => {
                            let plans = self
                                .control
                                .invoke_plans(|service, view| {
                                    service.admit(
                                        view,
                                        control::Control::source(source),
                                        id,
                                        observed,
                                        now.as_nanos(),
                                        &payload,
                                    )
                                })
                                .map_err(Error::Control)?;
                            self.control_plans(plans)?;
                            self.commit_boundary()?;
                            continue;
                        }
                        control::Event::TriggerBoundary => {
                            self.events
                                .entry(now)
                                .or_default()
                                .push_back(Scheduled::Control(control::Event::TriggerFinalize));
                            continue;
                        }
                        control::Event::TriggerFinalize => {
                            self.control.service.expire_triggers(now.as_nanos());
                            self.sync_trigger_marker();
                            continue;
                        }
                        control::Event::Plan(Plan::Wake { token, .. }) => {
                            let plan = self
                                .control
                                .service
                                .wake(token, now.as_nanos())
                                .map_err(|e| Error::Control(e.message))?;
                            self.control_plans(vec![plan])?;
                            continue;
                        }
                        control::Event::Plan(Plan::Deadline { fault, .. }) => {
                            if let Err(failure) =
                                self.control.service.deadline(fault, now.as_nanos())
                            {
                                self.control.settlements.extend(failure.settlements);
                                return Err(Error::Control(failure.message));
                            }
                            continue;
                        }
                        control::Event::Plan(Plan::Reply {
                            source,
                            id,
                            payload,
                            ..
                        }) => {
                            let target =
                                self.control.reply_target(&source).map_err(Error::Control)?;
                            Scheduled::Input {
                                id: target,
                                input: Input {
                                    at: now,
                                    payload: Request::SimulationReply { id, payload },
                                },
                            }
                        }
                        control::Event::Plan(Plan::Effect {
                            fault,
                            target,
                            boot,
                            effect,
                            command,
                            ..
                        }) => {
                            accepted_fault = Some(fault);
                            let id = self
                                .control
                                .target(&target)
                                .map_err(|e| Error::Control(e.message))?;
                            let check = self
                                .control
                                .invoke(|service, view| service.dispatch_check(view, fault))
                                .map_err(Error::Control)?;
                            match check {
                                None => continue,
                                Some(Err(reason)) => {
                                    let plans = self
                                        .control
                                        .invoke_plans(|service, _| {
                                            service.rejected(
                                                fault,
                                                now.as_nanos(),
                                                now.as_nanos(),
                                                reason,
                                            )
                                        })
                                        .map_err(Error::Control)?;
                                    self.control_plans(plans)?;
                                    continue;
                                }
                                Some(Ok(())) => {}
                            }
                            debug_assert_eq!(self.machines[id.0].boot, boot);
                            if !matches!(effect, Effect::Process(_)) {
                                service_effect = Some(fault);
                            }
                            match effect {
                                Effect::Pause => Scheduled::Driver(DriverEvent::Pause(id)),
                                Effect::Resume => Scheduled::Driver(DriverEvent::Resume(id)),
                                Effect::Execution(config) => {
                                    Scheduled::Driver(DriverEvent::SetExecution {
                                        machine: id,
                                        config,
                                    })
                                }
                                Effect::Crash => Scheduled::Input {
                                    id,
                                    input: Input {
                                        at: now,
                                        payload: Request::Crash,
                                    },
                                },
                                Effect::Reboot => Scheduled::Input {
                                    id,
                                    input: Input {
                                        at: now,
                                        payload: Request::Reboot,
                                    },
                                },
                                Effect::Process(action) => {
                                    service_origin =
                                        Some(crate::oracle::observation::Origin::Service { fault });
                                    Scheduled::Input {
                                        id,
                                        input: Input {
                                            at: now,
                                            payload: Request::Application {
                                                id: command.ok_or_else(|| {
                                                    Error::Control(
                                                        "process plan lacks command".into(),
                                                    )
                                                })?,
                                                command: action,
                                            },
                                        },
                                    }
                                }
                            }
                        }
                        control::Event::Plan(Plan::Settled(_)) => {
                            // Receipts transfer in invoke_plans and are consumed
                            // by control_plans; they are never executable work.
                            return Err(Error::Control(
                                "settlement entered execution queue".into(),
                            ));
                        }
                    }
                }
                scheduled => scheduled,
            };
            let mut confirmed_transition = None;
            let mechanical = (|| -> Result<(), Error<M::Error>> {
                match scheduled {
                    Scheduled::Control(_) => unreachable!(),
                    Scheduled::Input { id, input } => {
                        if let Request::Application {
                            id: command,
                            command: action,
                        } = input.payload
                        {
                            let origin = service_origin
                                .unwrap_or(crate::oracle::observation::Origin::External);
                            if service_origin.is_none() {
                                self.control
                                    .retire_external(id, command)
                                    .map_err(Error::Control)?;
                            }
                            if !matches!(self.machines[id.0].stopped, StopReason::Exited { .. }) {
                                self.control
                                    .offered(id, command, action, origin)
                                    .map_err(Error::Control)?;
                            }
                        }
                        use crate::oracle::observation::Transition;
                        let transition = match input.payload {
                            Request::Crash => Some(Transition::Crash),
                            Request::Reboot => Some(Transition::Reboot {
                                new_boot: self.machines[id.0]
                                    .boot
                                    .checked_add(1)
                                    .ok_or(Error::BootOverflow { id })?,
                            }),
                            _ => None,
                        };
                        confirmed_transition = transition.map(|transition| (id, transition));
                        let entry = &mut self.machines[id.0];
                        match &input.payload {
                            Request::Reboot | Request::Crash => {
                                if id == self.oracle {
                                    self.oracle_stops.clear();
                                }
                                entry.inbox.clear();
                                entry.paused = false;
                            }
                            Request::Application { .. } | Request::SimulationReply { .. }
                                if matches!(entry.stopped, StopReason::Exited { .. }) =>
                            {
                                return Ok(());
                            }
                            Request::Application { .. }
                            | Request::SimulationReply { .. }
                            | Request::Shutdown => {}
                        }
                        if matches!(input.payload, Request::Reboot) {
                            entry.boot = entry
                                .boot
                                .checked_add(1)
                                .ok_or(Error::BootOverflow { id })?;
                        }
                        entry.inbox.push_back(input);
                        self.run_batch(&[id], now)?;
                    }
                    Scheduled::Driver(event) => {
                        let (DriverEvent::Wake(id)
                        | DriverEvent::Pause(id)
                        | DriverEvent::Resume(id)
                        | DriverEvent::SetExecution { machine: id, .. }) = event;
                        use crate::oracle::observation::Transition;
                        let entry = &self.machines[id.0];
                        let transition = match event {
                            DriverEvent::SetExecution { config, .. } => {
                                Some(Transition::Execution(config))
                            }
                            DriverEvent::Pause(_)
                                if !entry.paused
                                    && !matches!(entry.stopped, StopReason::Exited { .. }) =>
                            {
                                Some(Transition::Pause)
                            }
                            DriverEvent::Resume(_) if entry.paused => Some(Transition::Resume),
                            _ => None,
                        };
                        confirmed_transition = transition.map(|transition| (id, transition));
                        let entry = &mut self.machines[id.0];
                        match event {
                            DriverEvent::SetExecution { config, .. } => entry.execution = config,
                            _ if matches!(entry.stopped, StopReason::Exited { .. }) => {}
                            DriverEvent::Wake(_) if !entry.paused => self.run_batch(&[id], now)?,
                            DriverEvent::Pause(_) => entry.paused = true,
                            DriverEvent::Resume(_) if entry.paused => {
                                entry.paused = false;
                                self.run_batch(&[id], now)?;
                            }
                            DriverEvent::Wake(_) | DriverEvent::Resume(_) => {}
                        }
                    }
                }
                Ok(())
            })();
            if let Err(error) = mechanical {
                if let Some(fault) = accepted_fault {
                    let failure = self.control.service.execution_failed(
                        fault,
                        None,
                        now.as_nanos(),
                        "machine effect failed; execution state is uncertain".into(),
                    );
                    self.control.settlements.extend(failure.settlements);
                }
                self.control.uncertain_failure();
                return Err(error);
            }
            if let Some((id, transition)) = confirmed_transition {
                let plans = self
                    .control
                    .admission(id, now, transition)
                    .map_err(Error::Control)?;
                self.control_plans(plans)?;
            }
            if let Some(fault) = service_effect {
                let plans = self
                    .control
                    .invoke_plans(|service, _| {
                        service.applied(fault, now.as_nanos(), now.as_nanos())
                    })
                    .map_err(Error::Control)?;
                self.control_plans(plans)?;
            }
            self.commit_boundary()?;
            if self.finished.is_some() {
                return Ok(None);
            }
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
        self.next().is_none() && self.events.is_empty()
    }

    fn outcome(&self) -> StepOutcome {
        if let Some((at, reason)) = self.finished {
            return StepOutcome::Finished { at, reason };
        }
        let at = self.now();
        if self.is_quiescent() {
            StepOutcome::Blocked { at }
        } else {
            StepOutcome::BarrierReached { at }
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
    /// Ordinary wakeups, pause/resume/reboot/shutdown actions, and crashes of already
    /// exited machines do not consume the arm. Rearming while held resumes the held
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
        if self.driver.finished.is_some() {
            Err(Error::Completed)
        } else if self.driver.playback.stopped {
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
    /// This is separate from its last `StopReason`: paused machines receive no
    /// positive execution budget, but due requests can update their state and
    /// adopt time through zero-budget runs. Exited machines are
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
        machine: impl FnOnce() -> Result<M, M::Error>,
    ) -> Result<MachineId, Error<M::Error>> {
        self.check_mutation()?;
        let machine = machine().map_err(Error::Construction)?;
        let mut bytes = [0; 8];
        self.driver.rng.fill_bytes(&mut bytes);
        let rng = SeededRng::new(u64::from_le_bytes(bytes));
        let id = MachineId(self.driver.machines.len());
        self.driver.machines.push(Entry {
            machine: Some(machine),
            rng: Some(rng),
            execution: ExecutionConfig::default(),
            reached: self.driver.now(),
            stopped: StopReason::BoundaryReached,
            paused: false,
            inbox: VecDeque::new(),
            staged: Vec::new(),
            boot: 0,
        });
        self.driver.control.add_unnamed(self.driver.now());
        Ok(id)
    }

    /// Capture one machine without changing its scheduling state or running it,
    /// including while paused. Reconstructing a whole simulation also requires
    /// its environment and driver-owned scheduling/lifecycle state, including
    /// each machine's execution configuration and pause flag observed through
    /// `paused`, global RNG cursor, and the independently continuing RNG cursor
    /// of every machine.
    pub fn snapshot(&mut self, id: MachineId) -> Result<M::Snapshot, Error<M::Error>> {
        if self.driver.finished.is_none() {
            self.check_mutation()?;
        }
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
    /// Queue a request in the same admission order as scheduled actions.
    /// Admission occurs in a subsequent `step`; enqueueing executes no work
    /// and does not imply success or a guest acknowledgment. Its boot is
    /// determined when it becomes due. Application IDs must not be reused
    /// within that admitted boot. Ordinary requests for an exited boot are
    /// discarded; Reboot starts a new boot. Zero-budget runs service due
    /// requests even while machine execution is paused.
    pub fn schedule_request(
        &mut self,
        at: Timestamp,
        id: MachineId,
        request: Request,
    ) -> Result<(), Error<M::Error>> {
        self.check_mutation()?;
        self.driver.check_valid()?;
        if at < self.driver.now() {
            return Err(Error::PastTime {
                now: self.driver.now(),
                requested: at,
            });
        }
        self.driver.entry(id)?;
        if let Request::Application { id: command, .. } = request {
            self.driver
                .control
                .reserve_external(id, command)
                .map_err(Error::Control)?;
        }
        self.driver
            .events
            .entry(at)
            .or_default()
            .push_back(Scheduled::Input {
                id,
                input: Input {
                    at,
                    payload: request,
                },
            });
        Ok(())
    }

    /// Transfer committed responses once, preserving commit/publication order.
    /// Within each commit, responses sort by timestamp, machine identity, and
    /// per-machine emission order. Separate commits are not globally resorted.
    /// Boot association is fixed at emission, including across reboots.
    /// Simulation-control messages belong to driver-owned handling. Calls in the
    /// final oracle batch remain observable but are not admitted after terminal
    /// cutoff. Draining never transfers dispatch or reply responsibility, and
    /// observers must not redispatch these messages.
    pub fn drain_responses(&mut self) -> Vec<MachineResponse> {
        std::mem::take(&mut self.driver.committed_outputs)
    }
}

/// Driver setup, execution, or contract failure.
#[derive(Debug)]
pub enum Error<E> {
    /// A machine factory failed before its machines were admitted.
    Construction(E),
    /// Simulation-control policy or correlation failed.
    Control(String),
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
    /// Oracle termination was committed; further scheduling is prohibited.
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
    BootOverflow {
        id: MachineId,
    },
    Machine {
        id: MachineId,
        source: E,
    },
    InvalidStop {
        id: MachineId,
        stopped: StopReason,
    },
    /// A machine emitted beyond the stop it returned. The driver discards the
    /// failed batch's staged outputs and prohibits further execution.
    InvalidOutput {
        id: MachineId,
        at: Timestamp,
        reached: Timestamp,
    },
    /// A previous step failed; further execution is prohibited.
    Failed,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Control(message) => write!(f, "simulation control: {message}"),
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
            Self::BootOverflow { id } => write!(f, "machine {id:?} boot generation overflow"),
            Self::UnknownMachine(id) => write!(f, "unknown machine {}", id.0),
            Self::Machine { id, source } => write!(f, "machine {}: {source}", id.0),
            Self::InvalidStop { id, stopped } => {
                write!(f, "machine {} returned an invalid stop: {stopped:?}", id.0)
            }
            Self::InvalidOutput { id, at, reached } => write!(
                f,
                "machine {} emitted output at {} beyond returned stop {}",
                id.0,
                at.as_nanos(),
                reached.as_nanos()
            ),
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
    /// Oracle termination was committed: its main application produced an
    /// outcome or a successful Stop acknowledgment, or its machine exited.
    /// An application outcome reports ProgramExit while its guest may remain alive. A confirmed
    /// application Stop reports Shutdown without claiming that its machine exited.
    /// Other machines and queued work remain stopped. This result is permanent;
    /// committed responses remain available through the barrier.
    Finished { at: Timestamp, reason: ExitReason },
}

#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod execution_tests;

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
mod control_failure_tests;
#[cfg(test)]
mod control_timer_tests;
#[cfg(test)]
mod oracle_correlation_tests;
