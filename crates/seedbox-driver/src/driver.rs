//! Simulation driving, shared time, and barrier contracts.
//!
//! A driver owns scheduling and shared simulation time. Machines own execution
//! and report their progress; the driver coordinates that progress with timers
//! and external events to establish simulation-wide barriers.

use crate::{
    clock::{Clock, ClockRate, FixedClock, Projection, SimulationClock},
    definition::MachineDefinition,
    execution::ExecutionConfig,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Input, Outbox, Request, Response},
    rng::{RngStream, SeededRng, SimulationRng},
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fmt,
    num::NonZeroU32,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

mod admission;
pub use admission::{DueOutcome, PreparedInput, PreparedOutcome};
mod boot_end;
mod checkpoint;
mod evidence;
mod execution_evidence;
pub use crate::persistence::{CodecError, DecodeLimits, SnapshotReference};
pub use checkpoint::{Checkpoint, CheckpointMapError};
mod control;
mod controller;
pub use controller::ControllerSetup;
mod event;
mod input;
mod network;
#[cfg(test)]
mod network_test_frames;
pub(crate) use network::Network as NetworkState;
mod playback;
pub use event::{
    Cause, Event, EventKind, FaultAction, FaultId, FaultOrigin, FaultRecord, FaultTarget,
    SettlementStatus, Transition, TriggerResult,
};
pub use input::{
    Action, FaultHandle, HealTarget, InputKey, InputRejection, InvalidInputKey, RejectionReason,
    Target,
};
pub use network::{
    Connection, FrameFact, FrameFacts, FrameFate, InvalidNetworkConfig, LinkPolicy, NetworkConfig,
    NetworkFaultCounts, NetworkFaults, NetworkPair, NetworkPairCounts, NetworkPolicy,
    NetworkSettlement, NetworkSetup, QueueCounts, SelectionCounts,
};
use playback::Playback;
pub use playback::{PlaybackRate, PlaybackStatus};

#[cfg(test)]
use crate::pv::Output;

mod workers;
use workers::Workers;

/// An insertion-ordered machine identity within one driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MachineId(usize);
impl MachineId {
    /// The oracle is constructed first and retains this identity in checkpoints.
    pub const ORACLE: Self = Self(0);
}

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
    /// Committed boot, pause gate, execution setting and readiness, changed only
    /// through confirmed effects (`Control::admission`) and committed observations.
    lifecycle: crate::oracle::observation::State,
    /// Exact mapping from simulation time to this machine's time base.
    time: Projection,
    /// Last accepted execution position, in machine time.
    position: Timestamp,
    /// Pending timer of a Blocked stop, in machine time; re-converted to
    /// simulation time whenever the rate changes.
    timer: Option<Timestamp>,
    reached: Timestamp,
    /// Accepted run progress; physical exits are folded into `lifecycle` only
    /// when their round commits.
    stopped: StopReason,
    inbox: VecDeque<Input>,
    staged: Vec<MachineResponse>,
    /// Greatest ended ordinal accepted into retained staging/publication.
    /// None is distinct from logical boot zero; no unbounded boot registry.
    last_ended_published: Option<u64>,
}

#[derive(Clone, Copy)]
enum DriverEvent {
    Pause(MachineId),
    Resume(MachineId),
    SetExecution {
        machine: MachineId,
        config: ExecutionConfig,
    },
    SetClockRate {
        machine: MachineId,
        rate: ClockRate,
    },
}

/// A fact awaiting its publication position. Service notices stay raw until
/// publication so that their order relative to Driver facts is preserved.
#[derive(Clone)]
enum Staged {
    Driver(Timestamp, EventKind),
    Notice(crate::oracle::service::Notice),
}

#[derive(Clone)]
enum Scheduled {
    Driver(DriverEvent),
    Control(control::Event),
    Input { id: MachineId, input: Input },
}

impl<M> Entry<M> {
    /// Machine `id` joining at `at`, with the identity time mapping from there.
    fn new(id: MachineId, machine: M, seed: u64, at: Timestamp) -> Self {
        Self {
            machine: Some(machine),
            rng: Some(SeededRng::new(seed)),
            lifecycle: crate::oracle::observation::State::new(
                control::Control::source(id),
                0,
                at.as_nanos(),
                ExecutionConfig::default(),
            ),
            time: Projection::new(at.as_nanos()),
            position: at,
            timer: None,
            reached: at,
            stopped: StopReason::BoundaryReached,
            inbox: VecDeque::new(),
            staged: Vec::new(),
            last_ended_published: None,
        }
    }

    /// Map a simulation time at or after the last rate change into machine time.
    fn machine_time(&self, id: MachineId, at: Timestamp) -> Result<Timestamp, ClockOverflow> {
        self.time
            .floor(at.as_nanos())
            .map(Timestamp::from_nanos)
            .map_err(|_| ClockOverflow(id))
    }

    /// Earliest simulation time at or after `not_before` whose machine time
    /// reaches `machine`; `None` when no representable simulation time does.
    fn simulation_time(&self, not_before: Timestamp, machine: Timestamp) -> Option<Timestamp> {
        self.time
            .deadline(not_before.as_nanos(), machine.as_nanos())
            .ok()
            .map(Timestamp::from_nanos)
    }

    fn next(&self) -> Option<Timestamp> {
        if self.lifecycle.paused() {
            return None;
        }
        match self.stopped {
            StopReason::BoundaryReached => Some(self.reached),
            StopReason::Blocked { deadline, .. } => deadline,
            StopReason::Exited { .. } => None,
        }
    }
}

/// Driver bookkeeping for a live host input key. Only live keys are kept;
/// the host owns the history of every key it used.
#[derive(Clone, Debug)]
struct OwnedHandle {
    record_id: crate::oracle::RecordId,
    text: String,
}

#[derive(Clone, Debug)]
enum LiveInput {
    /// Queued and not yet due.
    Pending,
    /// Admitted and unsettled; a throttle names the handle it reserved.
    Admitted {
        handle: Option<String>,
        record_id: Option<crate::oracle::RecordId>,
    },
    /// Settled Applied with this exact record still active.
    Handle(OwnedHandle),
}

/// A machine's time base cannot represent the requested simulation time.
struct ClockOverflow(MachineId);
impl<E> From<ClockOverflow> for Error<E> {
    fn from(ClockOverflow(id): ClockOverflow) -> Self {
        Error::ClockOverflow { id }
    }
}

type MachineFactory<M> =
    Box<dyn FnMut(&MachineDefinition) -> Result<M, <M as Machine>::Error> + Send>;

/// Continuing execution, independent of host resources and observer retention.
struct ExecutionState<M> {
    clock: Arc<SimulationClock>,
    rng: SimulationRng,
    quantum_ns: u64,
    quantum_end: Timestamp,
    machines: Vec<Entry<M>>,
    events: BTreeMap<Timestamp, VecDeque<Scheduled>>,
    boundary_index: u64,
    completed_quanta: u64,
    quantum_committed: bool,
    failed: bool,
    oracle: MachineId,
    finished: Option<(Timestamp, ExitReason)>,
    oracle_stops: BTreeSet<(u64, NonZeroU32)>,
    control: control::Control,
    inputs: BTreeMap<InputKey, LiveInput>,
    trigger_marker: Option<Timestamp>,
    definitions: Vec<MachineDefinition>,
    network: network::Network,
}

/// Already-committed descriptions owned by one observer session. These are
/// distinct from Entry::staged, whose accepted outputs still drive execution.
#[derive(Clone, Default)]
struct ObservationState {
    staged_events: Vec<Staged>,
    published: Vec<Event>,
    next_sequence: u64,
}

/// Owns simulation scheduling and shared time.
///
/// # Machine time bases
///
/// Each machine runs in its own time base `G`, a continuous, strictly
/// increasing map from simulation time, kept exactly in units of 1e-9 ns. It
/// is the identity from the machine's addition until a clock rate change; a
/// rate change at barrier `T` reanchors `G` at `T` keeping `G(T)` (so accrued
/// offset is kept), and survives crash and reboot. Machines observe
/// `g(t) = floor(G(t))`. The driver translates at the machine boundary:
/// - Into the machine: its clock reads `g(now)`, it runs to `g(upto)`, and
///   due inputs are offered at `g(at)` (or `g` of the last rate change, if
///   the input became due before it).
/// - Out of the machine: an output, `Blocked`/`Exited` position or timer
///   deadline at machine time `m` maps to the earliest simulation time at or
///   after the run's start whose `G` reaches `m`. Results are never early;
///   rounding is to whole nanoseconds. A timer no representable simulation
///   time reaches never fires. Pending timers are re-converted at a rate change.
///
/// A machine has reached simulation barrier `T` exactly when its machine
/// position reaches `g(T)`: a machine time `m` maps to at most `T` if and only
/// if `m <= g(T)`. Every machine therefore stops at the same common barriers,
/// consecutive runs tile its own time without gaps, and no state beyond a
/// barrier is exposed. Outputs are published in simulation time after
/// conversion. Execution pricing is not compensated: a fast machine completes
/// proportionally more work per simulation nanosecond. Conversions happen on
/// the driver thread and never depend on worker count. A machine time base
/// that cannot represent a required simulation time fails with
/// `Error::ClockOverflow`.
///
/// Only the times the driver passes across `Machine::run` are translated.
/// Simulation-control payloads are opaque bytes: every time inside them stays
/// simulation time, both in replies (`now_ns`, `requested_at_ns`,
/// `admitted_at_ns`, `effect_at_ns`, `replied_at_ns`) and in requests
/// (`sleep` `duration_ns`, trigger `match_timeout_ns`, and the effect deadline
/// a guest command must meet). The call's observation time is its converted
/// output time. Sleep, trigger matching and deadlines therefore keep their
/// simulation-time meaning, while a drifting guest's own clocks run at its
/// rate: a 1 s control `sleep` at +10% spans 1.1 s of that guest's clocks.
///
/// Execution uses a fixed pool of worker threads. Each worker owns its context;
/// contexts must not contain machine-specific simulation state. Worker count and
/// completion order must not affect simulation results. Machines share the
/// driver's read-only clock, which stays fixed throughout each batch of calls.
///
/// The positive quantum is fixed for the driver's lifetime. Quantum boundaries
/// are anchored at the initial timestamp; intermediate barriers never shift them.
/// The caller schedules host inputs (`Barrier::schedule`) between steps; they
/// share one FIFO timeline with guest control work and are admitted through
/// the same control policy, in insertion order at one timestamp and before
/// timer wakeups. Execution settings apply in every machine state and survive
/// lifecycle changes without executing the machine. Lifecycle effects receive
/// zero-budget admission runs, and reboot admits a fresh boot.
/// Paused machines remain unfinished and receive no positive execution budget.
/// Due requests still receive zero-budget admission runs, which adopt shared
/// time and make due timers eligible without executing program instructions.
/// Pausing never freezes the clock or shifts timer deadlines. Retained input
/// stays queued while paused; crashes still terminate paused machines.
/// Guest simulation-control calls are interpreted after their full round commits.
/// Their effects and replies join the same ordered timeline at that barrier;
/// draining events (`Driver::drain_events`) only observes them. Accepted control work survives caller
/// loss. Calls generated together are queued before their generated effects;
/// dependent operations must await acknowledgment rather than assume that a
/// concurrent Pause/Resume sequence succeeds. Oracle completion publishes the full response batch before stopping
/// further admission.
pub struct Driver<M: Machine> {
    workers: Workers<M::Context>,
    execution: ExecutionState<M>,
    playback: Playback,
    observations: ObservationState,
    factory: MachineFactory<M>,
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Construct machines and driver-owned simulation dependencies.
    ///
    /// The supplied arbitrary RNG has no capture contract. Whole-Driver
    /// checkpoints are unsupported for this constructor; use [`Self::new_seeded`]
    /// when a continuing simulation must be captured.
    ///
    /// The factory constructs the required oracle first, followed by the other
    /// definitions in list order. `spawnable` contains definitions not started at
    /// boot; any machine may request each name once through simulation control.
    /// Names must be unique across all three lists, checked before either factory.
    /// Spawns join at their FIFO effect turn and run from the next round. IDs and
    /// RNG streams follow spawn admission order. Construction failure publishes a
    /// Failed settlement and poisons the driver; confirmed events remain drainable.
    /// The driver retains the factory and spawnable definitions.
    /// TODO: Re-evaluate: declared spawnables keep future machines as complete inputs.
    /// A factory alone cannot enumerate names for pre-execution uniqueness checks.
    /// Retaining definitions supports one-shot admission without template or override APIs.
    /// The factory must return newly configured machines without executing them. This order
    /// breaks scheduling ties and assigns
    /// independent machine RNG streams by drawing one little-endian u64 seed
    /// from the simulation RNG for each admitted machine. Failed additions draw
    /// no seed. Seeded clock-fault values for a target come from a separate
    /// stream derived from that same machine seed in its own domain: they draw
    /// nothing from the simulation RNG or the machine's stream, and survive
    /// reboot. Only run calls borrow the clock and the machine's continuing RNG.
    /// Machine-only snapshots capture none of these RNG cursors.
    // TODO: Re-evaluate: seeded clock faults need values that neither shift
    // later machine seeds nor consume guest entropy or another target's draws;
    // the shared simulation stream and the machine's own stream cannot isolate
    // them, so each target gets a domain-separated stream from its seed.
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
    ///
    /// # Network
    ///
    /// Definitions with a [`NetworkInterface`](crate::definition::NetworkInterface)
    /// attach to one shared L2 segment configured by `network`; MAC addresses
    /// must be unique across all three lists, checked with names before either
    /// factory. Machines added through [`Barrier::add`] are never attached.
    /// A committed [`Response::TransmitFrame`] is routed when the commit that
    /// publishes it happens, in output publication order, against the network
    /// state at that point of the timeline: a round's commit precedes the
    /// timeline items due at its boundary, and an admission run's commit
    /// follows the item that caused it. All-ones broadcast reaches every other
    /// attached machine in identity order, unicast reaches its address's owner,
    /// and any other destination reaches nobody. Each candidate is lost or
    /// delayed by latency plus jitter using streams derived from the sender's
    /// seed and receiver's name; they draw nothing from other streams. Arrivals
    /// on one ordered pair are strictly increasing; an arrival that no time can
    /// represent fails with `Error::TimeOverflow`, and a sender's exhausted
    /// frame sequence with `Error::PositionOverflow`. A partition in force when
    /// a frame is routed cuts the candidate before it draws or reserves its
    /// pair's FIFO slot: a frame stamped `T` by the round ending at `T` routes
    /// before a partition or heal applied at `T`, while one emitted by an
    /// admission run after that effect routes after it. Frames in flight survive
    /// their sender's crash, reboot or exit.
    ///
    /// A frame arriving at `T` is offered exactly at `T`, after every timeline
    /// item due at `T` and before timer wakeups, as [`Request::ReceiveFrame`]
    /// to the receiver's current boot, including while paused. It is dropped if
    /// that boot has exited. Delivery means offered to the NIC, not consumed.
    /// While an attached machine can execute, rounds end no later than its
    /// earliest execution time plus latency, so no frame arrives before the
    /// round that sent it commits. Blocked or paused machines impose no limit.
    // TODO: Re-evaluate: positive latency bounds each round from the earliest
    // attached producer, as legacy lookahead did, without early delivery; blocked
    // machines impose no bound, so idle simulations still jump to their next event.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        start: Timestamp,
        quantum: Duration,
        workers: usize,
        rng: impl RngStream + Send + 'static,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        oracle: MachineDefinition,
        machines: Vec<MachineDefinition>,
        spawnable: Vec<MachineDefinition>,
        network: NetworkConfig,
        factory: impl FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
    ) -> Result<Self, Error<M::Error>> {
        Self::construct(
            start,
            quantum,
            workers,
            SimulationRng::External(Box::new(rng)),
            context,
            oracle,
            machines,
            spawnable,
            network.into(),
            factory,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn construct(
        start: Timestamp,
        quantum: Duration,
        workers: usize,
        rng: SimulationRng,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        oracle: MachineDefinition,
        machines: Vec<MachineDefinition>,
        spawnable: Vec<MachineDefinition>,
        network: NetworkSetup,
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
        let mut names = BTreeSet::new();
        for definition in std::iter::once(&oracle).chain(&machines).chain(&spawnable) {
            if !names.insert(&definition.name) {
                return Err(Error::DuplicateName(definition.name.clone()));
            }
        }
        let mut macs = BTreeSet::new();
        for definition in std::iter::once(&oracle).chain(&machines).chain(&spawnable) {
            if let Some(interface) = &definition.network {
                if !macs.insert(interface.mac()) {
                    return Err(Error::DuplicateMac(interface.mac()));
                }
            }
        }
        network::Network::validate_setup(
            &network,
            std::iter::once(&oracle).chain(&machines).chain(&spawnable),
        )
        .map_err(Error::NetworkConfiguration)?;
        let controller = network.controller_setup();
        if let Some(ControllerSetup {
            controller: Some(id),
            ..
        }) = controller
        {
            if id != MachineId(0) || oracle.name.is_empty() {
                return Err(Error::Control(
                    "controller must designate the named oracle/end-on-exit machine".into(),
                ));
            }
        }
        let workers = Workers::new(workers, context)?;
        let clock = Arc::new(SimulationClock::new(start.as_nanos()));
        let definitions: Vec<_> = std::iter::once(oracle).chain(machines).collect();
        let machines = definitions
            .iter()
            .map(&mut factory)
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Construction)?;
        let mut rng = rng;
        let mut draw_seed = || {
            let mut bytes = [0; 8];
            rng.fill_bytes(&mut bytes);
            u64::from_le_bytes(bytes)
        };
        let seeds: Vec<u64> = machines.iter().map(|_| draw_seed()).collect();
        let machines = machines
            .into_iter()
            .zip(&seeds)
            .enumerate()
            .map(|(index, (machine, &seed))| Entry::new(MachineId(index), machine, seed, start))
            .collect();
        let mut control = control::Control::new(&definitions, &seeds);
        control.declare_spawnable(&spawnable);
        control.controller = controller;
        control.service.reply_latency_ns = controller.map_or(0, |setup| setup.latency_ns);
        let mut network = network::Network::from_setup(network);
        for (index, (definition, &seed)) in definitions.iter().zip(&seeds).enumerate() {
            network.admit(
                MachineId(index),
                &definition.name,
                seed,
                definition.network.as_ref().map(|i| i.mac()),
            );
        }
        let mut driver = Self {
            workers,
            execution: ExecutionState {
                clock,
                quantum_ns,
                quantum_end,
                machines,
                rng,
                events: BTreeMap::new(),
                boundary_index: 0,
                completed_quanta: 0,
                quantum_committed: false,
                failed: false,
                oracle: MachineId(0),
                finished: None,
                oracle_stops: BTreeSet::new(),
                control,
                inputs: BTreeMap::new(),
                trigger_marker: None,
                definitions: spawnable,
                network,
            },
            observations: ObservationState::default(),
            playback: Playback::default(),
            factory: Box::new(factory),
        };
        for index in 0..driver.execution.machines.len() {
            driver.stage(
                start,
                EventKind::MachineAdded {
                    machine: MachineId(index),
                    cause: Cause::Initial,
                },
            );
        }
        driver.flush_events()?;
        Ok(driver)
    }

    /// Configure bounded host-only frame retention, including after restore.
    /// Zero disables retention. Clearing or shrinking counts discarded facts as
    /// omissions in the next drain. Never changes policy, machine work, event
    /// sequencing or deterministic evidence.
    // TODO: Re-evaluate: an observer belongs to the host, including after restore.
    // Constructor-only capacity cannot rebind a captured Driver to an absent or
    // new observer without changing simulation authority; keep this telemetry-only.
    pub fn set_frame_capacity(&mut self, capacity: usize) {
        self.execution.network.set_frame_capacity(capacity);
    }

    /// Drain committed bounded frame observations independently of EventKind.
    /// Works after failure/stop/completion, does no machine work, and never
    /// changes authoritative counters or dense event/witness sequencing.
    pub fn drain_frames(&mut self) -> FrameFacts {
        self.execution.network.drain_frames()
    }

    /// Read cumulative per-pair decisions and confirmed receive offers without
    /// machine work, draining observations or changing event/witness sequencing.
    /// These totals remain independent of bounded frame-fact omission. Like
    /// Driver state generally, terminal failure does not promise rollback.
    // TODO: Re-evaluate: world counters cannot be reconstructed from an omitted
    // observation stream; a typed network-only projection avoids generic Inspect.
    /// Keys are directed insertion identities, not concatenated display names;
    /// all pairs are retained even when names contain `->` or other punctuation.
    pub fn network_counters(&self) -> BTreeMap<NetworkPair, NetworkPairCounts> {
        self.execution.network.counts()
    }

    /// Take all undrained events in publication order.
    ///
    /// This works in every state, including after a failed `step`, a
    /// permanent stop and oracle completion, and never runs simulation work.
    /// Each event is returned exactly once. The driver keeps no history of
    /// drained events: the host owns retained history. Undrained events are
    /// retained without bound, so hosts should drain after every `step` and
    /// barrier callback.
    ///
    /// # Order
    ///
    /// Publication order is the order in which the driver commits facts, and
    /// `Event::sequence` numbers it densely from zero. Construction publishes
    /// a `MachineAdded` event for each initial machine, oracle first. Machine
    /// outputs of one barrier are published together, sorted by occurrence
    /// time, machine identity and per-machine emission order; control facts
    /// derived from that batch follow it. Driver-applied changes publish when
    /// they take effect, in timeline FIFO order, before the outputs of the
    /// admission run they cause. Separate commits are never resorted, so
    /// `Event::at` is not monotonic across the stream. Enqueueing work,
    /// including `Barrier::schedule`, publishes nothing; a host input publishes
    /// `FaultAdmitted` or `InputRejected` when it becomes due. Sleep wakeups, internal timeline markers and playback
    /// changes are not published.
    ///
    /// # Faults and triggers
    ///
    /// Admission and settlement are separate events: `FaultAdmitted` records
    /// policy admission and a later `FaultSettled` reports the outcome. Only
    /// `SettlementStatus::Applied` proves a change. Control facts are
    /// incremental; a settlement repeats the fault's `FaultRecord` so hosts need
    /// no second store to interpret it. A fault's Driver-applied change, when
    /// it takes effect, publishes a `Transition` with `Cause::Fault` before its
    /// Applied settlement. A fired trigger publishes `TriggerResolved` before the
    /// admission of the fault it caused.
    ///
    /// # Failure
    ///
    /// Confirmed prefixes survive failure. After `step` returns `Err`, every
    /// fact confirmed before the failure remains drainable, including outputs
    /// of earlier commits and Failed or other settlements produced on the
    /// failure path. A failed execution batch publishes none of its staged
    /// outputs. A barrier mutation that succeeded remains published even if
    /// its callback later returns `Err`; for example `Barrier::add` publishes
    /// `MachineAdded` immediately. Failure, permanent stop and oracle
    /// completion are reported by `step`, not as events.
    ///
    /// # Determinism
    ///
    /// The stream, including sequences and positions, is a pure function of
    /// the machine definitions, the seed and host inputs (barrier operations
    /// and the `upto` bounds passed to `step`). Worker count and playback rate
    /// never change it.
    pub fn drain_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.observations.published)
    }

    fn stage_notices(&mut self) {
        let notices = self.execution.control.service.take_notices();
        for notice in &notices {
            Self::track_input(&mut self.execution.inputs, notice);
        }
        self.observations
            .staged_events
            .extend(notices.into_iter().map(Staged::Notice));
    }

    fn creates_handle(action: &crate::oracle::service::FaultAction) -> bool {
        use crate::oracle::service::{Effect, FaultAction};
        matches!(
            action,
            FaultAction::Effect(
                Effect::Execution(_)
                    | Effect::Drift(_)
                    | Effect::StepRealtime(_)
                    | Effect::Partition
                    | Effect::Shape(..)
                    | Effect::Select
            )
        )
    }

    /// Follow a host input key from admission to settlement and, for a
    /// handle-creating fault (throttle, drift, step), until its handle is healed.
    fn track_input(
        inputs: &mut BTreeMap<InputKey, LiveInput>,
        notice: &crate::oracle::service::Notice,
    ) {
        use crate::oracle::service::{
            FaultAction as Action, Notice, Requester, SettlementStatus as Status,
        };
        match notice {
            Notice::Admitted(admitted) => {
                if let Requester::Input(key) = &admitted.requester {
                    // Throttle, drift and step create their handle; a heal
                    // merely names one.
                    let created = Self::creates_handle(&admitted.action);
                    inputs.insert(
                        InputKey::validated(key.clone()),
                        LiveInput::Admitted {
                            handle: admitted.handle.clone().filter(|_| created),
                            record_id: admitted.record_id.filter(|_| created),
                        },
                    );
                }
            }
            Notice::Settled(settled) => {
                if let Requester::Input(key) = &settled.requester {
                    let key = InputKey::validated(key.clone());
                    match (&settled.action, settled.status, &settled.handle) {
                        (action, Status::Applied, Some(handle)) if Self::creates_handle(action) => {
                            inputs.insert(
                                key,
                                LiveInput::Handle(OwnedHandle {
                                    record_id: settled.record_id.expect("created handle identity"),
                                    text: handle.clone(),
                                }),
                            );
                        }
                        _ => {
                            inputs.remove(&key);
                        }
                    }
                }
                if let (Action::Heal, Status::Applied, Some(healed)) =
                    (&settled.action, settled.status, settled.record_id)
                {
                    inputs.retain(
                        |_, live| !matches!(live, LiveInput::Handle(handle) if handle.record_id == healed),
                    );
                }
            }
            _ => {}
        }
    }

    /// Record a Driver fact after any service facts confirmed before it.
    fn stage(&mut self, at: Timestamp, kind: EventKind) {
        self.stage_notices();
        self.observations
            .staged_events
            .push(Staged::Driver(at, kind));
    }

    /// Publish staged facts at the current committed position. A conversion
    /// failure skips only that fact and is reported after publishing the rest.
    fn flush_events(&mut self) -> Result<(), Error<M::Error>> {
        self.stage_notices();
        let position = self.position();
        let mut result = Ok(());
        for staged in std::mem::take(&mut self.observations.staged_events) {
            let (at, kind) = match staged {
                Staged::Driver(at, kind) => (at, kind),
                Staged::Notice(notice) => match self.execution.control.event(notice) {
                    Ok(event) => event,
                    Err(message) => {
                        if result.is_ok() {
                            result = Err(Error::Control(message));
                        }
                        continue;
                    }
                },
            };
            self.observations.published.push(Event {
                sequence: self.observations.next_sequence,
                position,
                at,
                kind,
            });
            self.observations.next_sequence += 1;
        }
        result
    }

    /// Access the simulation at its initial or most recent successful barrier.
    ///
    /// The callback has exclusive access while execution is stopped. Machine
    /// references cannot escape it, and stepping cannot resume until it returns.
    /// Machine additions and scheduling changes are available only through this
    /// scope. A failed driver does not admit another barrier callback.
    ///
    /// A borrowed machine can be observed inside the callback:
    ///
    /// ```
    /// use seedbox_driver::driver::{Driver, Error, MachineId};
    /// use seedbox_machine::machine::Machine;
    /// fn observe<M>(driver: &mut Driver<M>, id: MachineId) -> Result<bool, Error<M::Error>>
    /// where M: Machine + Send + 'static, M::Context: 'static, M::Error: Send + 'static {
    ///     driver.with_barrier(|barrier| {
    ///         let _machine = barrier.machine(id)?;
    ///         barrier.paused(id)
    ///     })
    /// }
    /// ```
    ///
    /// The same imports and bounds do not permit the borrow to escape:
    ///
    /// ```compile_fail
    /// use seedbox_driver::driver::{Driver, MachineId};
    /// use seedbox_machine::machine::Machine;
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
        Timestamp::from_nanos(self.execution.clock.now())
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
    /// A pre-fault stop (`Barrier::pause_before_next_fault`) returns before the
    /// held fault's effect is applied, also for inputs scheduled at the
    /// initial/current timestamp. Selecting a running rate or admitting a
    /// quantum applies that effect exactly once. Same-time FIFO order and
    /// subsequent timer wakeups are preserved.
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
    /// Except at an explicit `BeforeFault` stop, events due at a returned
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
    /// - [`StepOutcome::BeforeFault`]: the next effective fault effect is held.
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

    /// Step toward a caller-owned future input boundary, even if all machines
    /// are idle or paused individually. Global playback pause/rate and permanent
    /// completion still apply. Time advances only through ordinary Driver
    /// quantum boundaries and never beyond `upto`; idle time runs no machines.
    ///
    /// This does not queue, admit or reserve an input. The caller must deliver
    /// the due input at its barrier. The external-work assertion lasts only for
    /// this call and is not retained in checkpoints: restoration must resupply
    /// the current deadline from its own authoritative editable input suffix.
    /// A `Blocked` result may have advanced time and still needs another call
    /// until `upto` is reached. Existing `step` quiescence semantics are unchanged.
    // TODO: Re-evaluate: an external timeline must not be copied into Driver
    // merely to keep idle time moving toward an editable service-owned input.
    pub fn step_toward_external(
        &mut self,
        upto: Timestamp,
    ) -> Result<StepOutcome, Error<M::Error>> {
        self.step_at_with_external(upto, Instant::now(), true)
    }

    fn step_at(
        &mut self,
        upto: Timestamp,
        host_now: Instant,
    ) -> Result<StepOutcome, Error<M::Error>> {
        self.step_at_with_external(upto, host_now, false)
    }

    fn step_at_with_external(
        &mut self,
        upto: Timestamp,
        host_now: Instant,
        external_work: bool,
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
                return Ok(match self.playback.held.clone() {
                    Some(fault) => StepOutcome::BeforeFault {
                        at: self.now(),
                        fault,
                    },
                    None => StepOutcome::Waiting {
                        at: self.now(),
                        delay,
                    },
                });
            }
            let outcome = self.step_inner(upto, host_now, external_work)?;
            let position = self.position();
            self.playback.finish_quantum(position);
            if matches!(outcome, StepOutcome::Finished { .. }) {
                self.playback.finish();
                self.playback.before_next = false;
            }
            Ok(outcome)
        })();
        self.finish_execution(result)
    }

    /// Share the confirmed-prefix and poisoning policy with due-only admission.
    fn finish_execution<T>(
        &mut self,
        result: Result<T, Error<M::Error>>,
    ) -> Result<T, Error<M::Error>> {
        let result = result.and_then(|outcome| self.flush_events().map(|()| outcome));
        if result.is_err() {
            // All submitted synchronous calls have finished before poisoning.
            for entry in &mut self.execution.machines {
                entry.staged.clear();
            }
            self.execution.failed = true;
            // Facts confirmed before the failure remain drainable.
            let _ = self.flush_events();
        }
        result
    }

    fn position(&self) -> Position {
        Position {
            boundary_index: self.execution.boundary_index,
            completed_quanta: self.execution.completed_quanta,
            at: self.now(),
        }
    }

    fn commit_boundary(&mut self) -> Result<(), Error<M::Error>> {
        self.execution.boundary_index = self
            .execution
            .boundary_index
            .checked_add(1)
            .ok_or(Error::PositionOverflow)?;
        if self.now() == self.execution.quantum_end && !self.execution.quantum_committed {
            self.execution.completed_quanta = self
                .execution
                .completed_quanta
                .checked_add(1)
                .ok_or(Error::PositionOverflow)?;
            self.execution.quantum_committed = true;
        }
        let mut outputs = Vec::new();
        for (index, entry) in self.execution.machines.iter_mut().enumerate() {
            outputs.extend(entry.staged.drain(..).inspect(|output| {
                debug_assert_eq!(output.machine, MachineId(index));
            }));
        }
        // Stable sorting preserves each machine's emission order at one time.
        outputs.sort_by_key(|output| (output.at, output.machine));
        // Frames are routed in publication order; validated when their run was accepted.
        let position = self.position();
        let observation_mark = self.execution.network.observation_mark();
        for output in &outputs {
            if let Response::TransmitFrame { bytes } = &output.payload {
                if let Err(error) = self.execution.network.route_at(
                    position,
                    output.machine,
                    output.at.as_nanos(),
                    bytes,
                ) {
                    self.execution
                        .network
                        .discard_uncommitted_facts(observation_mark);
                    return Err(match error {
                        network::RouteError::ArrivalOverflow => Error::TimeOverflow,
                        network::RouteError::SequenceOverflow => Error::PositionOverflow,
                    });
                }
            }
        }
        let mut terminal = None;
        for output in &outputs {
            if output.machine != self.execution.oracle {
                continue;
            }
            match output.payload {
                Response::ApplicationOutcome { code } => {
                    terminal.get_or_insert(ExitReason::ProgramExit {
                        code: i32::from(code),
                    });
                }
                Response::CommandAcknowledged { id, result }
                    if self.execution.oracle_stops.remove(&(output.boot, id))
                        && result == crate::pv::CommandResult::Completed =>
                {
                    terminal.get_or_insert(ExitReason::Shutdown);
                }
                _ => {}
            }
        }
        for output in &outputs {
            self.stage(output.at, EventKind::Output(output.clone()));
        }
        let mut plans = vec![];
        let mut calls = vec![];
        let now = self.now();
        for output in &outputs {
            plans.extend(
                self.execution
                    .control
                    .observe(&mut self.execution.machines, output, now)
                    .map_err(Error::Control)?,
            );
            if let Response::SimulationControl { id, payload } = &output.payload {
                if self
                    .execution
                    .control
                    .controller
                    .is_some_and(|setup| setup.controller != Some(output.machine))
                {
                    return Err(Error::Control(
                        "control request from unauthorized machine".into(),
                    ));
                }
                calls.push(control::Event::Call {
                    source: output.machine,
                    boot: output.boot,
                    id: *id,
                    observed: output.at.as_nanos(),
                    payload: payload.clone(),
                });
            }
        }
        for index in 0..self.execution.machines.len() {
            plans.extend(
                self.execution
                    .control
                    .terminal(&mut self.execution.machines, MachineId(index), now)
                    .map_err(Error::Control)?,
            );
        }
        if self.execution.finished.is_none() {
            let reason = terminal.or_else(|| {
                match self.execution.machines[self.execution.oracle.0].stopped {
                    StopReason::Exited { reason, .. } => Some(reason),
                    _ => None,
                }
            });
            if let Some(reason) = reason {
                self.execution.finished = Some((self.now(), reason));
            }
        }
        if self.execution.finished.is_some() {
            self.execution
                .control
                .service
                .cut_off_triggers(self.now().as_nanos());
        } else {
            for call in calls {
                let at = match &call {
                    control::Event::Call { observed, .. }
                        if self.execution.control.controller.is_some() =>
                    {
                        Timestamp::from_nanos(
                            observed
                                .checked_add(self.execution.control.service.reply_latency_ns)
                                .ok_or(Error::TimeOverflow)?,
                        )
                    }
                    _ => self.now(),
                };
                if at < self.now() {
                    return Err(Error::Control(
                        "control admission precedes committed boundary".into(),
                    ));
                }
                self.execution
                    .events
                    .entry(at)
                    .or_default()
                    .push_back(Scheduled::Control(call));
            }
            self.control_plans(plans)?;
            let now = self.now().as_nanos();
            let plans = self
                .execution
                .control
                .invoke_plans(&self.execution.machines, |service, view| {
                    service.trigger_effects(view, now)
                })
                .map_err(Error::Control)?;
            self.control_plans(plans)?;
            self.sync_trigger_marker();
        }
        self.flush_events()
    }

    fn sync_trigger_marker(&mut self) {
        let next = self
            .execution
            .control
            .service
            .trigger_boundary()
            .map(Timestamp::from_nanos);
        if next == self.execution.trigger_marker {
            return;
        }
        // Only these internally owned markers are retired. External FIFO
        // actions and requests at the same timestamp retain their exact order.
        for events in self.execution.events.values_mut() {
            events.retain(|event| {
                !matches!(
                    event,
                    Scheduled::Control(
                        control::Event::TriggerBoundary | control::Event::TriggerFinalize
                    )
                )
            });
        }
        self.execution.events.retain(|_, events| !events.is_empty());
        self.execution.trigger_marker = next;
        if let Some(at) = next {
            self.execution
                .events
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
                for events in self.execution.events.values_mut() {
                    events.retain(|event| !matches!(event, Scheduled::Control(control::Event::Plan(Plan::Deadline { fault, .. })) if *fault == settlement.fault));
                }
                self.execution.events.retain(|_, events| !events.is_empty());
                // Immutable receipt ownership transferred in invoke_plans.
                continue;
            }
            if self.execution.control.controller.is_some() {
                if let Plan::Reply {
                    boot,
                    source,
                    id,
                    at,
                    payload,
                } = &plan
                {
                    let target = self
                        .execution
                        .control
                        .reply_target(source)
                        .map_err(Error::Control)?;
                    let boot = boot.ok_or_else(|| {
                        Error::Control("service reply lacks its caller boot".into())
                    })?;
                    let at = Timestamp::from_nanos(*at);
                    if at < self.now() {
                        return Err(Error::Control("control reply scheduled in the past".into()));
                    }
                    self.execution
                        .events
                        .entry(at)
                        .or_default()
                        .push_back(Scheduled::Control(control::Event::Reply {
                            target,
                            boot,
                            id: *id,
                            payload: payload.clone(),
                        }));
                    continue;
                }
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
            self.execution
                .events
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
        external_work: bool,
    ) -> Result<StepOutcome, Error<M::Error>> {
        if let Some(fault) = self.apply_due(host_now)? {
            return Ok(StepOutcome::BeforeFault {
                at: self.now(),
                fault,
            });
        }
        if self.execution.finished.is_some()
            || upto == self.now()
            || (self.is_quiescent() && !external_work)
        {
            return Ok(self.outcome());
        }

        if self.now() == self.execution.quantum_end {
            self.execution.quantum_committed = false;
            self.execution.quantum_end = Timestamp::from_nanos(
                self.execution
                    .quantum_end
                    .as_nanos()
                    .checked_add(self.execution.quantum_ns)
                    .ok_or(Error::TimeOverflow)?,
            );
        }

        // Known events limit the whole round. A timer discovered during a run
        // can shorten another machine's next budget, but cannot invalidate the
        // round's final boundary. Intermediate local progress stays private.
        let boundary = self
            .execution
            .machines
            .iter()
            .filter(|entry| !entry.lifecycle.paused())
            .filter_map(|entry| match entry.stopped {
                StopReason::Blocked { deadline, .. } => deadline,
                _ => None,
            })
            .chain(self.execution.events.first_key_value().map(|(&at, _)| at))
            .chain(
                self.execution
                    .network
                    .next_arrival()
                    .map(Timestamp::from_nanos),
            )
            .chain(self.causal_bound())
            .chain(self.controller_bound())
            .fold(upto.min(self.execution.quantum_end), Timestamp::min);

        let initial = self.now();
        let mut ran = false;
        while let Some((_, now)) = self.next() {
            if now >= boundary {
                break;
            }
            self.execution.clock.set(now.as_nanos());
            let limit = self
                .execution
                .machines
                .iter()
                .filter_map(Entry::next)
                .filter(|&at| at > now)
                .min()
                .map_or(boundary, |at| at.min(boundary));
            let ready = self
                .execution
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
        // A queued input still keeps time moving toward its own barrier.
        let at = if self.is_quiescent() && !external_work {
            self.execution
                .machines
                .iter()
                .map(|entry| entry.reached)
                .max()
                .unwrap_or(self.now())
                .max(self.now())
        } else {
            boundary
        };
        self.execution.clock.set(at.as_nanos());
        if at != initial || ran {
            self.commit_boundary()?;
        }
        if let Some(fault) = self.apply_due(host_now)? {
            return Ok(StepOutcome::BeforeFault { at, fault });
        }
        Ok(self.outcome())
    }

    fn run_batch(&mut self, ids: &[MachineId], upto: Timestamp) -> Result<(), Error<M::Error>> {
        self.run_batch_as(ids, upto, None)
    }

    /// Run a batch. `rebooting` names the operation-local prospective boot of
    /// a single machine's Reboot admission run: its outputs carry that boot,
    /// which `Control::admission` commits only after the run is confirmed.
    fn run_batch_as(
        &mut self,
        ids: &[MachineId],
        upto: Timestamp,
        rebooting: Option<u64>,
    ) -> Result<(), Error<M::Error>> {
        if ids.is_empty() {
            return Ok(());
        }
        debug_assert!(rebooting.is_none() || ids.len() == 1);
        let boot_of = |entry: &Entry<M>| rebooting.unwrap_or(entry.lifecycle.boot());
        let now = self.now();
        let offered_commands: Vec<_> = ids
            .iter()
            .flat_map(|&machine| {
                self.execution.machines[machine.0]
                    .inbox
                    .iter()
                    .filter_map(move |input| {
                        control::command(&input.payload).map(|(id, _)| (machine, id, input.at))
                    })
            })
            .collect();
        let offered_stops: Vec<_> = if ids.contains(&self.execution.oracle) {
            self.execution.machines[self.execution.oracle.0]
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
        // Map the common barrier into every machine's own time base before
        // submitting any work: start, bound and due inputs. Inputs that became
        // due before the last rate change are offered at that change.
        let mut bounds = BTreeMap::new();
        let mut views = Vec::with_capacity(ids.len());
        for &id in ids {
            let entry = &self.execution.machines[id.0];
            let start = entry.machine_time(id, now)?;
            let bound = entry.machine_time(id, upto)?;
            let anchor = Timestamp::from_nanos(entry.time.anchor());
            let inbox = entry
                .inbox
                .iter()
                .map(|input| {
                    Ok(Input {
                        at: entry.machine_time(id, input.at.max(anchor))?.min(start),
                        payload: input.payload.clone(),
                    })
                })
                .collect::<Result<VecDeque<_>, ClockOverflow>>()?;
            bounds.insert(id, (start, bound, inbox.len()));
            views.push((id, start, bound, inbox));
        }
        let (send, receive) = mpsc::channel();
        let mut submission_error = None;
        for (id, start, bound, mut inbox) in views {
            let entry = &mut self.execution.machines[id.0];
            let mut machine = entry.machine.take().expect("stopped machine");
            let mut rng = entry.rng.take().expect("stopped machine RNG");
            let execution = entry.lifecycle.execution();
            let mut outputs = Vec::new();
            let result = send.clone();
            if let Err(error) = self.workers.submit(id.0, move |context| {
                let stopped = catch_unwind(AssertUnwindSafe(|| {
                    machine.run(
                        &mut RunContext {
                            worker: context,
                            clock: &FixedClock(start.as_nanos()),
                            rng: &mut rng,
                            execution,
                            inbox: Inbox::new(&mut inbox, start),
                            outbox: Outbox::new(&mut outputs, start, bound),
                        },
                        bound,
                    )
                }))
                .map_err(|_| Error::Panicked { id })
                .and_then(|result| result.map_err(|source| Error::Machine { id, source }));
                let _ = result.send((id, machine, rng, inbox.len(), outputs, stopped));
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
        for (id, machine, rng, remaining, outputs, stopped) in reports {
            let entry = &mut self.execution.machines[id.0];
            entry.machine = Some(machine);
            entry.rng = Some(rng);
            // Accepted inputs are the taken prefix; the rest stay in simulation time.
            let taken = bounds[&id]
                .2
                .saturating_sub(remaining)
                .min(entry.inbox.len());
            let admitted_end = entry
                .inbox
                .iter()
                .take(taken)
                .any(|input| matches!(input.payload, Request::Crash | Request::Reboot));
            entry.inbox.drain(..taken);
            stops.push((id, outputs, stopped, admitted_end));
        }
        if let Some(error) = submission_error {
            return Err(error);
        }
        // Validate each stop in its machine's time base, then convert back.
        // The conversion returns the earliest simulation time whose machine
        // time reaches the reported one, so nothing is reported early, and a
        // machine time at most the mapped bound maps to at most `upto`.
        let mut accepted = Vec::with_capacity(stops.len());
        for (id, outputs, result, admitted_end) in stops {
            let stopped = result?;
            let (start, bound, _) = bounds[&id];
            let entry = &self.execution.machines[id.0];
            let last_ended = boot_end::validate(
                &outputs,
                entry.lifecycle.boot(),
                entry.last_ended_published,
                entry.stopped,
                stopped,
                admitted_end,
            )
            .map_err(|detail| Error::InvalidBootEnd { id, detail })?;
            let reached = match stopped {
                StopReason::BoundaryReached => bound,
                StopReason::Blocked { reached, deadline } => {
                    if deadline.is_some_and(|at| at <= reached.max(start)) {
                        return Err(Error::InvalidStop { id, stopped });
                    }
                    reached
                }
                StopReason::Exited { reached, .. } => reached,
            };
            if reached < entry.position.max(start) || reached > bound {
                return Err(Error::InvalidStop { id, stopped });
            }
            if let Some(output) = outputs.iter().find(|output| output.at > reached) {
                return Err(Error::InvalidOutput {
                    id,
                    at: output.at,
                    reached,
                });
            }
            if outputs.iter().any(|output| {
                matches!(&output.payload, Response::TransmitFrame { bytes }
                    if !self.execution.network.attached(id) || !crate::pv::frame_in_bounds(bytes))
            }) {
                return Err(Error::InvalidFrame { id });
            }
            let simulation = |at| entry.simulation_time(now, at).ok_or(ClockOverflow(id));
            let (converted, timer) = match stopped {
                StopReason::BoundaryReached => (StopReason::BoundaryReached, None),
                StopReason::Blocked { deadline, .. } => (
                    StopReason::Blocked {
                        reached: simulation(reached)?,
                        // A timer no simulation time can reach never fires.
                        deadline: deadline.and_then(|at| entry.simulation_time(now, at)),
                    },
                    deadline,
                ),
                StopReason::Exited { reason, .. } => (
                    StopReason::Exited {
                        reached: simulation(reached)?,
                        reason,
                    },
                    None,
                ),
            };
            let outputs = outputs
                .into_iter()
                .map(|output| {
                    Ok(crate::pv::Output {
                        at: simulation(output.at)?,
                        payload: output.payload,
                    })
                })
                .collect::<Result<Vec<_>, ClockOverflow>>()?;
            let simulated = match converted {
                StopReason::BoundaryReached => upto,
                StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
            };
            debug_assert!(simulated >= now && simulated <= upto);
            accepted.push((
                id, simulated, converted, outputs, reached, timer, last_ended,
            ));
        }
        for &(id, command, at) in &offered_commands {
            let entry = &mut self.execution.machines[id.0];
            if !entry.inbox.iter().any(|input| {
                control::command(&input.payload).is_some_and(|(pending, _)| pending == command)
            }) {
                let boot = boot_of(entry);
                self.execution
                    .control
                    .accepted(id, &mut entry.lifecycle, boot, command, at, upto)
                    .map_err(Error::Control)?;
            }
        }
        for (id, reached, stopped, outputs, position, timer, last_ended) in accepted {
            let entry = &mut self.execution.machines[id.0];
            let boot = boot_of(entry);
            if id == self.execution.oracle {
                for &command in &offered_stops {
                    let unconsumed = entry.inbox.iter().any(|input| matches!(input.payload,
                        Request::Application { id, command: crate::pv::ApplicationCommand::Stop } if id == command));
                    if !unconsumed {
                        self.execution.oracle_stops.insert((boot, command));
                    }
                }
            }
            entry.reached = reached;
            entry.stopped = stopped;
            entry.position = position;
            entry.timer = timer;
            entry.last_ended_published = last_ended;
            entry
                .staged
                .extend(outputs.into_iter().map(|output| MachineResponse {
                    machine: id,
                    boot,
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

    fn apply_due(&mut self, host_now: Instant) -> Result<Option<FaultRecord>, Error<M::Error>> {
        self.apply_due_mode(host_now, false)
    }

    fn apply_due_mode(
        &mut self,
        host_now: Instant,
        hold_paused: bool,
    ) -> Result<Option<FaultRecord>, Error<M::Error>> {
        let result = self.apply_due_inner(host_now, hold_paused);
        // Facts of a final uncommitted item belong to the current barrier.
        let flushed = self.flush_events();
        let result = result?;
        flushed.map(|()| result)
    }

    fn apply_due_inner(
        &mut self,
        host_now: Instant,
        hold_paused: bool,
    ) -> Result<Option<FaultRecord>, Error<M::Error>> {
        if self.execution.finished.is_some() {
            return Ok(None);
        }
        let now = self.now();
        let mut waking = Vec::new();
        // Keep the front event until its effect is admitted, including across
        // a pre-fault stop. Driver controls and requests share one FIFO timeline.
        // Frames due now follow every timeline item due now, then timers.
        loop {
            while let Some(scheduled) = self
                .execution
                .events
                .get(&now)
                .and_then(|events| events.front())
                .cloned()
            {
                // Facts of the previous item that committed no barrier belong to
                // the current one, before this item can commit another.
                self.flush_events()?;
                // An effect plan is effective when its dispatch check passes. That
                // check runs before the plan leaves the FIFO front, so a hold keeps
                // this exact plan, and every later same-time item, pending.
                let mut checked = None;
                if let Scheduled::Control(control::Event::Plan(
                    crate::oracle::service::Plan::Effect { fault, .. },
                )) = &scheduled
                {
                    let fault = *fault;
                    let check = self
                        .execution
                        .control
                        .invoke(&self.execution.machines, |service, view| {
                            service.dispatch_check(view, fault)
                        })
                        .map_err(Error::Control)?;
                    if matches!(check, Some(Ok(()))) {
                        let was_held = self.playback.held.take().is_some();
                        if self.playback.before_next && !was_held {
                            let record = self
                                .execution
                                .control
                                .pending_record(fault)
                                .map_err(Error::Control)?;
                            self.playback
                                .hold(record.clone(), self.position(), host_now);
                            return Ok(Some(record));
                        }
                        if hold_paused && !was_held && self.playback.holds_unarmed_effect() {
                            let record = self
                                .execution
                                .control
                                .pending_record(fault)
                                .map_err(Error::Control)?;
                            self.playback.held = Some(record.clone());
                            return Ok(Some(record));
                        }
                    }
                    checked = Some(check);
                }
                let events = self.execution.events.get_mut(&now).expect("due events");
                events.pop_front();
                if events.is_empty() {
                    self.execution.events.remove(&now);
                }
                let mut service_effect = None;
                let mut service_origin = None;
                let mut accepted_fault = None;
                let mut reply_boot = None;
                let scheduled = match scheduled {
                    Scheduled::Control(event) => {
                        use crate::oracle::service::{Effect, Plan};
                        match event {
                            control::Event::Reply {
                                target,
                                boot,
                                id,
                                payload,
                            } => {
                                if self.execution.machines[target.0].lifecycle.boot() != boot
                                    || matches!(
                                        self.execution.machines[target.0].stopped,
                                        StopReason::Exited { .. }
                                    )
                                {
                                    continue;
                                }
                                reply_boot = Some(boot);
                                Scheduled::Input {
                                    id: target,
                                    input: Input {
                                        at: now,
                                        payload: Request::SimulationReply { id, payload },
                                    },
                                }
                            }
                            control::Event::Call {
                                source,
                                boot,
                                id,
                                observed,
                                payload,
                            } => {
                                let configured = self.execution.control.controller.is_some();
                                if configured
                                    && (self.execution.machines[source.0].lifecycle.boot() != boot
                                        || matches!(
                                            self.execution.machines[source.0].stopped,
                                            StopReason::Exited { .. }
                                        ))
                                {
                                    continue;
                                }
                                let excluded = self
                                    .execution
                                    .control
                                    .controller
                                    .and_then(|setup| setup.controller);
                                if !configured {
                                    self.execution.control.record_call(source, id, boot);
                                }
                                let plans = self
                                    .execution
                                    .control
                                    .invoke_with_network(
                                        &self.execution.machines,
                                        Some(&self.execution.network),
                                        |service, view| {
                                            view.guest_controller = excluded;
                                            if configured {
                                                service.admit_for_boot(
                                                    view,
                                                    (control::Control::source(source), boot),
                                                    id,
                                                    observed,
                                                    now.as_nanos(),
                                                    &payload,
                                                )
                                            } else {
                                                service.admit(
                                                    view,
                                                    control::Control::source(source),
                                                    id,
                                                    observed,
                                                    now.as_nanos(),
                                                    &payload,
                                                )
                                            }
                                        },
                                    )
                                    .map_err(Error::Control)?;
                                self.control_plans(plans)?;
                                self.commit_boundary()?;
                                continue;
                            }
                            control::Event::Input {
                                key,
                                action,
                                handle,
                            } => {
                                self.admit_input(key, action, handle, now)?;
                                self.commit_boundary()?;
                                continue;
                            }
                            control::Event::TriggerBoundary => {
                                self.execution
                                    .events
                                    .entry(now)
                                    .or_default()
                                    .push_back(Scheduled::Control(control::Event::TriggerFinalize));
                                continue;
                            }
                            control::Event::TriggerFinalize => {
                                self.execution
                                    .control
                                    .service
                                    .expire_triggers(now.as_nanos());
                                self.sync_trigger_marker();
                                continue;
                            }
                            control::Event::Plan(Plan::Wake { token, .. }) => {
                                let plan = self
                                    .execution
                                    .control
                                    .service
                                    .wake(token, now.as_nanos())
                                    .map_err(|e| Error::Control(e.message))?;
                                self.control_plans(vec![plan])?;
                                continue;
                            }
                            control::Event::Plan(Plan::Deadline { fault, .. }) => {
                                // A Failed settlement publishes through service notices.
                                if let Err(failure) = self
                                    .execution
                                    .control
                                    .service
                                    .deadline(fault, now.as_nanos())
                                {
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
                                let target = self
                                    .execution
                                    .control
                                    .reply_target(&source)
                                    .map_err(Error::Control)?;
                                reply_boot = Some(
                                    self.execution
                                        .control
                                        .take_call(target, id)
                                        .map_err(Error::Control)?,
                                );
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
                                scope,
                                effect,
                                command,
                                handle,
                                ..
                            }) => {
                                let check = checked.take().expect("checked effect plan");
                                match check {
                                    None => continue,
                                    Some(Err(reason)) => {
                                        let collision =
                                            self.execution.control.service.handle_collision(fault);
                                        let plans = self
                                            .execution
                                            .control
                                            .invoke_plans(&self.execution.machines, |service, _| {
                                                if collision {
                                                    service.failed_without_effect(
                                                        fault,
                                                        now.as_nanos(),
                                                        reason.message,
                                                    )
                                                } else {
                                                    service.rejected(
                                                        fault,
                                                        now.as_nanos(),
                                                        now.as_nanos(),
                                                        reason,
                                                    )
                                                }
                                            })
                                            .map_err(Error::Control)?;
                                        self.control_plans(plans)?;
                                        continue;
                                    }
                                    Some(Ok(())) => {}
                                }
                                let (target, boot) = match &scope {
                                    crate::oracle::service::Scope::Machine { name, boot } => {
                                        (name.clone(), *boot)
                                    }
                                    crate::oracle::service::Scope::Connection { connection } => {
                                        self.apply_connection(fault, connection, &effect, handle)?;
                                        self.commit_boundary()?;
                                        continue;
                                    }
                                    crate::oracle::service::Scope::DirectedLinks {
                                        from: left,
                                        to: right,
                                    }
                                    | crate::oracle::service::Scope::Links { left, right } => {
                                        self.apply_links(fault, left, right, &effect, handle)?;
                                        self.commit_boundary()?;
                                        continue;
                                    }
                                };
                                if effect == Effect::Spawn {
                                    let created = self.spawn(&target, fault);
                                    if let Err(error) = created {
                                        self.execution.control.service.execution_failed(
                                            fault,
                                            None,
                                            now.as_nanos(),
                                            "spawn construction failed".into(),
                                        );
                                        self.uncertain_failure();
                                        return Err(error);
                                    }
                                    let plans = self
                                        .execution
                                        .control
                                        .invoke_plans(&self.execution.machines, |service, _| {
                                            service.applied(fault, now.as_nanos(), now.as_nanos())
                                        })
                                        .map_err(Error::Control)?;
                                    self.control_plans(plans)?;
                                    self.commit_boundary()?;
                                    continue;
                                }
                                accepted_fault = Some(fault);
                                let id = self
                                    .execution
                                    .control
                                    .reply_target(&target)
                                    .map_err(Error::Control)?;
                                debug_assert_eq!(
                                    self.execution.machines[id.0].lifecycle.boot(),
                                    boot
                                );
                                if !effect.guest_command() {
                                    service_effect = Some(fault);
                                }
                                match effect {
                                    Effect::Spawn => unreachable!("spawn handled separately"),
                                    Effect::Partition
                                    | Effect::Unpartition
                                    | Effect::Shape(..)
                                    | Effect::Unshape
                                    | Effect::Select
                                    | Effect::Unselect => {
                                        return Err(Error::Control(
                                            "link effect bound to a machine".into(),
                                        ));
                                    }
                                    Effect::Pause => Scheduled::Driver(DriverEvent::Pause(id)),
                                    Effect::Resume => Scheduled::Driver(DriverEvent::Resume(id)),
                                    Effect::Execution(config) => {
                                        Scheduled::Driver(DriverEvent::SetExecution {
                                            machine: id,
                                            config,
                                        })
                                    }
                                    Effect::Drift(rate) => {
                                        Scheduled::Driver(DriverEvent::SetClockRate {
                                            machine: id,
                                            rate,
                                        })
                                    }
                                    Effect::StepRealtime(nanoseconds) => {
                                        service_origin =
                                            Some(crate::oracle::observation::Origin::Service {
                                                fault,
                                            });
                                        Scheduled::Input {
                                            id,
                                            input: Input {
                                                at: now,
                                                payload: Request::StepRealtime {
                                                    id: command.ok_or_else(|| {
                                                        Error::Control(
                                                            "clock plan lacks command".into(),
                                                        )
                                                    })?,
                                                    nanoseconds: std::num::NonZeroI64::new(
                                                        nanoseconds,
                                                    )
                                                    .ok_or_else(|| {
                                                        Error::Control(
                                                            "zero clock step plan".into(),
                                                        )
                                                    })?,
                                                },
                                            },
                                        }
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
                                    Effect::Shutdown => Scheduled::Input {
                                        id,
                                        input: Input {
                                            at: now,
                                            payload: Request::Shutdown,
                                        },
                                    },
                                    Effect::Process(action) => {
                                        service_origin =
                                            Some(crate::oracle::observation::Origin::Service {
                                                fault,
                                            });
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
                let cause =
                    accepted_fault.map_or(Cause::Host, |fault| Cause::Fault(FaultId(fault)));
                // Published only after the item takes effect without error.
                let mut effective = None;
                let mechanical = (|| -> Result<(), Error<M::Error>> {
                    match scheduled {
                        Scheduled::Control(_) => unreachable!(),
                        Scheduled::Input { id, input } => {
                            if let Some((command, action)) = control::command(&input.payload) {
                                // Public scheduling creates guest commands only from
                                // admitted faults. Only private queue tests inject raw
                                // (external) application requests.
                                let origin = service_origin
                                    .unwrap_or(crate::oracle::observation::Origin::External);
                                if !matches!(
                                    self.execution.machines[id.0].stopped,
                                    StopReason::Exited { .. }
                                ) {
                                    self.execution
                                        .control
                                        .offered(id, command, action, origin)
                                        .map_err(Error::Control)?;
                                }
                            }
                            use crate::oracle::observation::Transition;
                            let boot = self.execution.machines[id.0].lifecycle.boot();
                            let new_boot = match input.payload {
                                Request::Reboot => {
                                    Some(boot.checked_add(1).ok_or(Error::BootOverflow { id })?)
                                }
                                _ => None,
                            };
                            let transition = match input.payload {
                                Request::Crash => Some(Transition::Crash),
                                Request::Reboot => {
                                    new_boot.map(|new_boot| Transition::Reboot { new_boot })
                                }
                                _ => None,
                            };
                            // The pause gate is cleared when this transition commits.
                            confirmed_transition = transition.map(|transition| (id, transition));
                            let entry = &mut self.execution.machines[id.0];
                            match &input.payload {
                                Request::Reboot | Request::Crash => {
                                    if id == self.execution.oracle {
                                        self.execution.oracle_stops.clear();
                                    }
                                    entry.inbox.clear();
                                }
                                Request::Application { .. }
                                | Request::StepRealtime { .. }
                                | Request::SimulationReply { .. }
                                | Request::ReceiveFrame { .. }
                                    if matches!(entry.stopped, StopReason::Exited { .. }) =>
                                {
                                    return Ok(());
                                }
                                Request::Application { .. }
                                | Request::StepRealtime { .. }
                                | Request::SimulationReply { .. }
                                | Request::ReceiveFrame { .. }
                                | Request::Shutdown => {}
                                // Request is non-exhaustive across the common crate boundary.
                                _ => {
                                    return Err(Error::Control(
                                        "unsupported machine request".into(),
                                    ));
                                }
                            }
                            let exited = matches!(entry.stopped, StopReason::Exited { .. });
                            let change = |change| EventKind::Transition {
                                machine: id,
                                boot,
                                change,
                                cause,
                            };
                            effective = match &input.payload {
                                Request::Crash => Some(change(event::Transition::Crashed)),
                                Request::Reboot => new_boot.map(|new_boot| {
                                    change(event::Transition::Rebooted { new_boot })
                                }),
                                Request::Shutdown if !exited => {
                                    Some(change(event::Transition::ShutdownRequested))
                                }
                                // A call from an earlier boot belongs to a retired
                                // session, which the machine silently discards.
                                Request::SimulationReply { id: call, .. }
                                    if reply_boot.is_none_or(|called| called == boot) =>
                                {
                                    Some(EventKind::ReplyAdmitted {
                                        machine: id,
                                        boot,
                                        id: *call,
                                    })
                                }
                                _ => None,
                            };
                            entry.inbox.push_back(input);
                            // A Reboot admission run executes the prospective boot.
                            self.run_batch_as(&[id], now, new_boot)?;
                        }
                        Scheduled::Driver(event) => {
                            let (DriverEvent::Pause(id)
                            | DriverEvent::Resume(id)
                            | DriverEvent::SetExecution { machine: id, .. }
                            | DriverEvent::SetClockRate { machine: id, .. }) = event;
                            use crate::oracle::observation::Transition;
                            let entry = &self.execution.machines[id.0];
                            let transition = match event {
                                DriverEvent::SetExecution { config, .. } => {
                                    Some(Transition::Execution(config))
                                }
                                DriverEvent::SetClockRate { rate, .. } => {
                                    Some(Transition::ClockRate(rate))
                                }
                                DriverEvent::Pause(_)
                                    if !entry.lifecycle.paused()
                                        && !matches!(entry.stopped, StopReason::Exited { .. }) =>
                                {
                                    Some(Transition::Pause)
                                }
                                DriverEvent::Resume(_) if entry.lifecycle.paused() => {
                                    Some(Transition::Resume)
                                }
                                _ => None,
                            };
                            confirmed_transition = transition.map(|transition| (id, transition));
                            effective = transition.map(|transition| EventKind::Transition {
                                machine: id,
                                boot: entry.lifecycle.boot(),
                                change: match transition {
                                    Transition::Pause => event::Transition::Paused,
                                    Transition::Resume => event::Transition::Resumed,
                                    Transition::Execution(config) => {
                                        event::Transition::ExecutionSet(config)
                                    }
                                    Transition::ClockRate(rate) => {
                                        event::Transition::ClockRate(rate)
                                    }
                                    Transition::Crash | Transition::Reboot { .. } => unreachable!(),
                                },
                                cause,
                            });
                            // Pause, resume and execution settings change only
                            // the lifecycle record, when the transition commits.
                            let entry = &mut self.execution.machines[id.0];
                            match event {
                                DriverEvent::SetExecution { .. } | DriverEvent::Pause(_) => {}
                                DriverEvent::SetClockRate { rate, .. } => {
                                    // Reanchor keeping the mapped time at this
                                    // barrier, then re-convert a pending timer.
                                    entry
                                        .time
                                        .set_drift(now.as_nanos(), i64::from(rate.ppb()))
                                        .map_err(|_| Error::ClockOverflow { id })?;
                                    if let (StopReason::Blocked { reached, .. }, Some(timer)) =
                                        (entry.stopped, entry.timer)
                                    {
                                        entry.stopped = StopReason::Blocked {
                                            reached,
                                            deadline: entry.simulation_time(now, timer),
                                        };
                                    }
                                }
                                DriverEvent::Resume(_)
                                    if transition.is_some()
                                        && !matches!(entry.stopped, StopReason::Exited { .. }) =>
                                {
                                    self.run_batch(&[id], now)?;
                                }
                                DriverEvent::Resume(_) => {}
                            }
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = mechanical {
                    if let Some(fault) = accepted_fault {
                        // The Failed settlement publishes through service notices.
                        let _ = self.execution.control.service.execution_failed(
                            fault,
                            None,
                            now.as_nanos(),
                            "machine effect failed; execution state is uncertain".into(),
                        );
                    }
                    self.uncertain_failure();
                    return Err(error);
                }
                if let Some(kind) = effective {
                    self.stage(now, kind);
                }
                if let Some((id, transition)) = confirmed_transition {
                    let plans = self
                        .execution
                        .control
                        .admission(&mut self.execution.machines, id, now, transition)
                        .map_err(Error::Control)?;
                    self.control_plans(plans)?;
                }
                if let Some(fault) = service_effect {
                    let plans = self
                        .execution
                        .control
                        .invoke_plans(&self.execution.machines, |service, _| {
                            service.applied(fault, now.as_nanos(), now.as_nanos())
                        })
                        .map_err(Error::Control)?;
                    self.control_plans(plans)?;
                }
                self.commit_boundary()?;
                if self.execution.finished.is_some() {
                    return Ok(None);
                }
            }
            if !self.deliver_frames(now)? {
                break;
            }
            if self.execution.finished.is_some() {
                return Ok(None);
            }
        }
        for index in 0..self.execution.machines.len() {
            let stopped = self.execution.machines[index].stopped;
            if self.execution.machines[index].lifecycle.paused()
                || matches!(stopped, StopReason::Exited { .. })
            {
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

    /// Offer every frame arriving at `now` to its receiver's current boot, in
    /// (sender, sequence, receiver) order, with one zero-budget admission batch
    /// and commit. A receiver whose boot has exited drops the frame. Returns
    /// whether any frame was due, so that newly generated timeline items at
    /// `now` are processed before timers.
    fn deliver_frames(&mut self, now: Timestamp) -> Result<bool, Error<M::Error>> {
        let due = self.execution.network.take_due(now.as_nanos());
        if due.is_empty() {
            return Ok(false);
        }
        // Facts of the previous item belong to the current barrier.
        self.flush_events()?;
        let mut receivers = BTreeSet::new();
        let mut offered = Vec::new();
        let position = self.position();
        for delivery in due {
            let entry = &mut self.execution.machines[delivery.to.0];
            if matches!(entry.stopped, StopReason::Exited { .. }) {
                self.execution.network.delivery_result(
                    position,
                    delivery.from,
                    delivery.to,
                    delivery.sent,
                    delivery.bytes.len(),
                    true,
                );
                continue;
            }
            offered.push((
                delivery.from,
                delivery.to,
                delivery.sent,
                delivery.bytes.len(),
            ));
            entry.inbox.push_back(Input {
                at: now,
                payload: Request::ReceiveFrame {
                    bytes: delivery.bytes,
                },
            });
            receivers.insert(delivery.to);
        }
        if receivers.is_empty() {
            return Ok(true);
        }
        let receivers: Vec<_> = receivers.into_iter().collect();
        if let Err(error) = self.run_batch(&receivers, now) {
            self.uncertain_failure();
            return Err(error);
        }
        for (from, to, sent, bytes) in offered {
            self.execution
                .network
                .delivery_result(position, from, to, sent, bytes, false);
        }
        self.commit_boundary()?;
        Ok(true)
    }

    /// Apply a partition or its heal to the network at its FIFO turn and
    /// settle it Applied. Links bind to no boot; frames in flight are kept.
    fn apply_links(
        &mut self,
        fault: u32,
        left: &[String],
        right: &[String],
        effect: &crate::oracle::service::Effect,
        handle: Option<String>,
    ) -> Result<(), Error<M::Error>> {
        use crate::oracle::service::Effect;
        let now = self.now().as_nanos();
        let handle = handle.ok_or_else(|| Error::Control("link effect lacks a handle".into()))?;
        let ids = |names: &[String]| {
            names
                .iter()
                .map(|name| self.execution.control.reply_target(name))
                .collect::<Result<Vec<_>, _>>()
        };
        let (left, right) = (
            ids(left).map_err(Error::Control)?,
            ids(right).map_err(Error::Control)?,
        );
        let (changed, network) = match effect {
            Effect::Partition => (
                self.execution.network.partition(handle, &left, &right),
                None,
            ),
            Effect::Unpartition => (self.execution.network.heal(&handle), None),
            Effect::Shape(policy, faults) => (
                self.execution
                    .network
                    .shape(fault, handle, &left, &right, *policy, *faults),
                None,
            ),
            Effect::Unshape => {
                let stats = self.execution.network.unshape(&handle);
                (stats.is_some(), stats)
            }
            _ => (false, None),
        };
        if !changed {
            let _ = self.execution.control.service.execution_failed(
                fault,
                None,
                now,
                "network link state disagrees with its handle".into(),
            );
            self.uncertain_failure();
            return Err(Error::Control("network link effect failed".into()));
        }
        let plans = self
            .execution
            .control
            .invoke_plans(&self.execution.machines, |service, _| {
                service.applied_network(fault, now, network)
            })
            .map_err(Error::Control)?;
        self.control_plans(plans)
    }

    fn apply_connection(
        &mut self,
        fault: u32,
        connection: &Connection,
        effect: &crate::oracle::service::Effect,
        handle: Option<String>,
    ) -> Result<(), Error<M::Error>> {
        use crate::oracle::service::Effect;
        let now = self.now().as_nanos();
        let handle =
            handle.ok_or_else(|| Error::Control("connection effect lacks handle".into()))?;
        if *effect == Effect::Select {
            if let Err(message) = self
                .execution
                .network
                .select(fault, handle, connection.clone())
            {
                let plans = self
                    .execution
                    .control
                    .service
                    .failed_without_effect(fault, now, message)
                    .map_err(|e| Error::Control(e.message))?;
                return self.control_plans(plans);
            }
        } else if *effect == Effect::Unselect {
            if !self.execution.network.unselect(&handle) {
                return Err(Error::Control("selection disagrees with its handle".into()));
            }
        } else {
            return Err(Error::Control(
                "nonconnection effect bound to connection".into(),
            ));
        }
        let plans = self
            .execution
            .control
            .service
            .applied_network(fault, now, None)
            .map_err(|e| Error::Control(e.message))?;
        self.control_plans(plans)
    }

    /// Admit one due host input through the ordinary-control path. A
    /// rejection publishes `InputRejected` and releases the key; admission
    /// publishes `FaultAdmitted` through service notices and queues the
    /// generated effects at the FIFO tail.
    fn admit_input(
        &mut self,
        key: InputKey,
        action: Action,
        handle: Option<FaultHandle>,
        now: Timestamp,
    ) -> Result<(), Error<M::Error>> {
        // Settlements confirmed earlier update the live-key handles first.
        self.stage_notices();
        match Self::input_admission_with_handle(
            &mut self.execution.control,
            &self.execution.machines,
            Some(&self.execution.network),
            &self.execution.inputs,
            &key,
            &action,
            handle.as_ref(),
            now,
        )? {
            Ok((_, plans)) => self.control_plans(plans),
            Err(rejection) => {
                self.execution.inputs.remove(&key);
                self.stage(now, EventKind::InputRejected { key, rejection });
                Ok(())
            }
        }
    }

    fn spawn(&mut self, name: &str, fault: u32) -> Result<(), Error<M::Error>> {
        let index = self
            .execution
            .definitions
            .iter()
            .position(|d| d.name == name)
            .ok_or_else(|| Error::Control("spawn lacks declared definition".into()))?;
        let id = self
            .execution
            .control
            .spawn_target(name)
            .map_err(Error::Control)?;
        if id.0 != self.execution.machines.len() {
            return Err(Error::Control(
                "spawn identity out of insertion order".into(),
            ));
        }
        let machine =
            (self.factory)(&self.execution.definitions[index]).map_err(Error::Construction)?;
        let mut bytes = [0; 8];
        self.execution.rng.fill_bytes(&mut bytes);
        let now = self.now();
        let seed = u64::from_le_bytes(bytes);
        self.execution
            .machines
            .push(Entry::new(id, machine, seed, now));
        let definition = &self.execution.definitions[index];
        self.execution.network.admit(
            id,
            &definition.name,
            seed,
            definition.network.as_ref().map(|i| i.mac()),
        );
        self.execution
            .control
            .spawned(name, seed)
            .map_err(Error::Control)?;
        self.execution.definitions.remove(index);
        self.stage(
            self.now(),
            EventKind::MachineAdded {
                machine: id,
                cause: Cause::Fault(FaultId(fault)),
            },
        );
        Ok(())
    }

    fn next(&self) -> Option<(MachineId, Timestamp)> {
        self.execution
            .machines
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| entry.next().map(|at| (MachineId(index), at)))
            .min_by_key(|&(id, at)| (at, id))
    }

    fn is_quiescent(&self) -> bool {
        self.next().is_none()
            && self.execution.events.is_empty()
            && self.execution.network.is_idle()
    }

    /// The latest round boundary at which no frame sent in the round can be
    /// due: the earliest time an attached machine can execute, plus latency.
    /// Positive-budget runs start only at `Entry::next`, so every send in the
    /// round is at or after it. Blocked, paused and exited machines impose
    /// none. An unrepresentable bound imposes none: no such arrival exists.
    fn causal_bound(&self) -> Option<Timestamp> {
        let earliest = self
            .execution
            .machines
            .iter()
            .enumerate()
            .filter(|&(index, _)| self.execution.network.attached(MachineId(index)))
            .filter_map(|(_, entry)| entry.next())
            .min()?;
        debug_assert!(earliest >= self.now(), "eligible producer behind now");
        earliest
            .as_nanos()
            .checked_add(self.execution.network.latency_ns())
            .map(Timestamp::from_nanos)
    }

    fn controller_bound(&self) -> Option<Timestamp> {
        let setup = self.execution.control.controller?;
        setup.controller?;
        // Target acknowledgements carry an observed effect time too. Bound all
        // runnable producers so their effect+latency cannot precede commit.
        self.execution
            .machines
            .iter()
            .filter_map(Entry::next)
            .min()?
            .as_nanos()
            .checked_add(setup.latency_ns)
            .map(Timestamp::from_nanos)
    }

    fn outcome(&self) -> StepOutcome {
        if let Some((at, reason)) = self.execution.finished {
            return StepOutcome::Finished { at, reason };
        }
        let at = self.now();
        if self.is_quiescent() {
            StepOutcome::Blocked { at }
        } else {
            StepOutcome::BarrierReached { at }
        }
    }

    /// Unknown partial effects leave no trustworthy committed lifecycle.
    fn uncertain_failure(&mut self) {
        for entry in &mut self.execution.machines {
            entry.lifecycle.uncertain_failure();
        }
    }

    fn check_valid(&self) -> Result<(), Error<M::Error>> {
        if self.execution.failed {
            Err(Error::Failed)
        } else {
            Ok(())
        }
    }

    fn entry(&self, id: MachineId) -> Result<&Entry<M>, Error<M::Error>> {
        self.execution
            .machines
            .get(id.0)
            .ok_or(Error::UnknownMachine(id))
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
    /// quantum and a one-shot pre-fault arm. It never advances simulation time.
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
    /// A second pending admission is rejected. This clears a pre-fault arm.
    pub fn step_quantum(&mut self) -> Result<(), Error<M::Error>> {
        self.check_running()?;
        if matches!(self.driver.outcome(), StepOutcome::Finished { .. }) {
            return Err(Error::Completed);
        }
        let position = self.driver.position();
        self.driver.playback.step(position)
    }

    /// Run at a positive rate until just before the next effective fault
    /// effect, from any origin: a guest control call, a trigger or a host
    /// input. An effect is effective when its dispatch check passes at its
    /// FIFO turn, immediately before the driver applies it; this includes
    /// process command dispatch, throttle, heal and spawn. Effects rejected or
    /// made stale (for example by a reboot) at their turn, admissions,
    /// replies, wakes and settlements do not consume the arm.
    ///
    /// The held fault is already admitted, so its record has a fault ID. It
    /// stays at the FIFO front, with all following same-time items pending.
    /// Repeated `step` calls return the same [`StepOutcome::BeforeFault`]
    /// without progress. While held, only observation and playback controls
    /// are allowed through this handle. Selecting a running rate or admitting
    /// a quantum applies the held effect exactly once; rearming while held
    /// applies it and stops before the next effective fault, including at the
    /// same timestamp. The arm is consumed once, even at the current time.
    pub fn pause_before_next_fault(&mut self, rate: PlaybackRate) -> Result<(), Error<M::Error>> {
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
        if self.driver.execution.finished.is_some() {
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
            Err(Error::BeforeFault)
        } else {
            Ok(())
        }
    }

    /// Observe machines in their stable insertion order.
    pub fn machines(&self) -> impl ExactSizeIterator<Item = (MachineId, &M)> {
        self.driver
            .execution
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

    /// Read actual stopped-state evidence without granting mutable machine access.
    ///
    /// Works at final `Finished` and permanently stopped boundaries, but never
    /// after a failed step: a machine execution error promises no resumable
    /// stopped state. `None` is unavailable evidence, not a full-state success.
    /// This does not execute work, drain output, or change clocks or RNG state.
    // TODO: Re-evaluate: evidence transports need a mutable stopped backend;
    // a specific operation preserves the barrier's no-execution boundary.
    pub fn state_evidence(
        &mut self,
        id: MachineId,
    ) -> Result<Option<seedbox_machine::machine::StateEvidence>, Error<M::Error>> {
        self.driver.check_valid()?;
        self.driver
            .execution
            .machines
            .get_mut(id.0)
            .ok_or(Error::UnknownMachine(id))?
            .machine
            .as_mut()
            .ok_or(Error::Failed)?
            .state_evidence()
            .map_err(|source| Error::Machine { id, source })
    }

    /// The machine's last stop projected into simulation time. Unlike a
    /// `Machine::run` result, a `Blocked` deadline may equal `reached`.
    // TODO: Re-evaluate: the projection rounds machine times up to whole
    // simulation nanoseconds so nothing fires early; keeping the strict
    // inequality would need delaying the timer, so the Driver relaxes it here
    // instead of in the machine contract.
    pub fn stopped(&self, id: MachineId) -> Result<StopReason, Error<M::Error>> {
        Ok(self.driver.entry(id)?.stopped)
    }

    /// Observe whether the driver is withholding this machine's execution.
    /// This is separate from its last `StopReason`: paused machines receive no
    /// positive execution budget, but due requests can update their state and
    /// adopt time through zero-budget runs. Exited machines are
    /// never logically paused. Pause and resume are faults: they take effect
    /// at their FIFO turn after admission (see [`Barrier::schedule`]).
    pub fn paused(&self, id: MachineId) -> Result<bool, Error<M::Error>> {
        Ok(self.driver.entry(id)?.lifecycle.paused())
    }

    /// Construct and admit a machine at this barrier's time. Construction
    /// failure leaves the existing set unchanged. The returned identity follows
    /// all earlier additions; removal and identity reuse are not supported.
    /// While a guest spawn identity is reserved but not yet dispatched, returns
    /// `Error::SpawnPending` without invoking the factory, drawing a seed or
    /// changing state. Drive the pending spawn before attempting a host addition.
    /// Success publishes `EventKind::MachineAdded` with `Cause::Host`
    /// immediately, so it survives a later error from the same callback.
    pub fn add(
        &mut self,
        machine: impl FnOnce() -> Result<M, M::Error>,
    ) -> Result<MachineId, Error<M::Error>> {
        self.check_mutation()?;
        if self.driver.execution.control.spawn_pending() {
            return Err(Error::SpawnPending);
        }
        let machine = machine().map_err(Error::Construction)?;
        let mut bytes = [0; 8];
        self.driver.execution.rng.fill_bytes(&mut bytes);
        let id = MachineId(self.driver.execution.machines.len());
        let now = self.driver.now();
        let seed = u64::from_le_bytes(bytes);
        self.driver
            .execution
            .machines
            .push(Entry::new(id, machine, seed, now));
        // Host additions have no definition and therefore no interface.
        self.driver.execution.network.admit(id, "", seed, None);
        self.driver.execution.control.add_unnamed(seed);
        self.driver.stage(
            self.driver.now(),
            EventKind::MachineAdded {
                machine: id,
                cause: Cause::Host,
            },
        );
        self.driver.flush_events()?;
        Ok(id)
    }

    /// Capture one machine without changing its scheduling state or running it,
    /// including while paused. The snapshot is in the machine's own time base.
    /// Reconstructing a whole simulation also requires its environment and
    /// driver-owned scheduling/lifecycle state, including each machine's
    /// execution configuration and pause flag observed through `paused`, its
    /// time-base mapping (a pure function of its addition time and its
    /// `Transition::ClockRate` events), its last accepted position and pending
    /// timer in machine time, global RNG cursor, the independently continuing
    /// RNG cursor and clock-fault stream cursor of every machine, and the
    /// control timeline: host inputs
    /// scheduled but not yet admitted, live input keys (with the handles they
    /// own), and admitted faults awaiting their effect or settlement. It also
    /// requires the external network: frames in flight with their arrivals,
    /// each ordered pair's loss and jitter cursors and FIFO clock, each
    /// sender's frame sequence, and active partition cuts with their handles.
    /// Use [`Driver::checkpoint`] for that whole-simulation state when the
    /// Driver was constructed with [`Driver::new_seeded`]. A machine-only
    /// snapshot does not supply it: `stopped` exposes rounded simulation-time
    /// conversions, not the exact private machine-time position and timer.
    pub fn snapshot(&mut self, id: MachineId) -> Result<M::Snapshot, Error<M::Error>> {
        if self.driver.execution.finished.is_none() {
            self.check_mutation()?;
        }
        self.driver.entry(id)?;
        self.driver.execution.machines[id.0]
            .machine
            .as_mut()
            .expect("stopped machine")
            .snapshot()
            .map_err(|source| Error::Machine { id, source })
    }

    /// Private test mechanics: queue a raw rate change in the FIFO timeline,
    /// bypassing control policy, for mock machines without control facts.
    #[cfg(test)]
    pub(crate) fn schedule_clock_rate(
        &mut self,
        at: Timestamp,
        id: MachineId,
        rate: ClockRate,
    ) -> Result<(), Error<M::Error>> {
        self.check_mutation()?;
        if at < self.driver.now() {
            return Err(Error::PastTime {
                now: self.driver.now(),
                requested: at,
            });
        }
        self.driver.entry(id)?;
        self.driver
            .execution
            .events
            .entry(at)
            .or_default()
            .push_back(Scheduled::Driver(DriverEvent::SetClockRate {
                machine: id,
                rate,
            }));
        Ok(())
    }

    /// Enqueue a host input: `action` is admitted through the control policy
    /// at `at`, identified by `key`.
    ///
    /// # Enqueue
    ///
    /// This only queues the input. It executes nothing, publishes nothing and
    /// reserves no fault ID, handle or application command ID, so not-yet-due
    /// inputs never shift the IDs guest faults receive. It is allowed only at
    /// a barrier that permits mutation, and checks the request's shape:
    ///
    /// - `at` is not before the current time ([`Error::PastTime`]);
    /// - a [`Target::Machine`] exists ([`Error::UnknownMachine`]); a
    ///   [`Target::Name`] names exactly one machine ([`Error::UnknownName`]);
    ///   a spawn names a declared machine that is not yet admitted
    ///   ([`Error::UnknownName`]);
    /// - a process command's `deadline` is positive and `at + deadline` is
    ///   representable ([`Error::InvalidDeadline`]);
    /// - a realtime step's magnitude is at most 365 days
    ///   ([`Error::InvalidClockStep`]);
    /// - `key` is not live ([`Error::LiveInputKey`]). A key is live while its
    ///   input is pending, while its admitted fault is unsettled, and while the
    ///   handle its admission created is active. The driver keeps only live
    ///   keys; the host owns their history and any wider uniqueness rule.
    ///
    /// An error leaves the queue unchanged. To act "now", schedule at the
    /// current instant: the input is admitted by the next `step`, before any
    /// machine runs.
    ///
    /// # Admission
    ///
    /// When the input reaches the front of the timeline at `at`, in FIFO order
    /// with guest calls, trigger work and earlier inputs, the control policy
    /// admits it exactly as it admits an ordinary guest control call,
    /// including the availability preflight that rejects pause, resume,
    /// shutdown and process commands for a physically exited, uncrashed target.
    /// (Output triggers keep their own admission mode.) Targets resolve and the
    /// target's current boot is bound at admission. No guest call, reply or
    /// message is fabricated.
    ///
    /// - Admitted: allocates a fault ID (and a handle for a throttle, drift or
    ///   realtime step) from the
    ///   shared owners and publishes [`EventKind::FaultAdmitted`] with
    ///   [`FaultOrigin::Input`] carrying `key`.
    /// - Rejected: publishes [`EventKind::InputRejected`] with `key`; no fault
    ///   ID is allocated. (A rejected throttle still consumes the handle
    ///   number it drew, as for guest calls.)
    ///
    /// # Execution and completion
    ///
    /// Generated effects join the FIFO tail at the admission time and are
    /// revalidated at their turn, like guest-call effects. Consequently a later
    /// same-time input is admitted before an earlier input's effect applies:
    /// Crash then Reboot of one machine at the same time admits the Crash and
    /// rejects the Reboot (the target is not yet down). Schedule dependent
    /// inputs at a later time, or after observing the settlement. Driver
    /// changes publish [`EventKind::Transition`] with [`Cause::Fault`]. Only
    /// [`SettlementStatus::Applied`] in [`EventKind::FaultSettled`] proves the
    /// change. Every effect is bound to the target's boot at admission; if that
    /// boot ends before the effect's turn (for example, a same-time Reboot
    /// effect applies first), it settles Unavailable or Rejected without
    /// mutating the machine, and its key is released. Process commands and
    /// realtime steps wait for the guest's readiness, are dispatched with a
    /// driver-allocated command ID and settle on acknowledgment; a process
    /// command's `deadline`, or the default 5 s for a realtime step, measured
    /// from admission, bounds dispatch and acknowledgment, and expiry fails the
    /// driver with a Failed settlement, as for guest controls. A guest refusal
    /// of a realtime step settles Rejected without failing the driver (Linux
    /// may refuse an admissible step); a refused process command, or a
    /// malformed, missing or late acknowledgment, is fatal. Accepted work
    /// survives caller loss.
    ///
    /// After oracle completion, a permanent stop or a driver failure, pending
    /// inputs are never admitted and no per-key event is published.
    pub fn schedule(
        &mut self,
        at: Timestamp,
        key: InputKey,
        action: Action,
    ) -> Result<(), Error<M::Error>> {
        self.schedule_as(at, key, action, None)
    }
    /// Queue an explicitly named handle-installing input. The name is carried
    /// through authored admission/prepare/commit; it is not installed afterward.
    /// Active collision is checked at admission and again before effects; named
    /// inputs never consume guest pN numbers. Pending same-text inputs coexist.
    /// Admission rejects a text already Active/Healing/Lost. An apply-time
    /// collision settles Failed without effects or poison, removing only the
    /// losing Pending record; an allocated loser never gets its pN back.
    // TODO: Re-evaluate: cross-origin heal/inspect and FIFO collision timing
    // require the alias in the Driver owner, not a service-local mapping.
    pub fn schedule_with_handle(
        &mut self,
        at: Timestamp,
        key: InputKey,
        action: Action,
        handle: FaultHandle,
    ) -> Result<(), Error<M::Error>> {
        self.schedule_as(at, key, action, Some(handle))
    }
    fn schedule_as(
        &mut self,
        at: Timestamp,
        key: InputKey,
        action: Action,
        handle: Option<FaultHandle>,
    ) -> Result<(), Error<M::Error>> {
        self.check_mutation()?;
        let driver = &mut *self.driver;
        driver.check_valid()?;
        driver.check_schedule_shape(at, &key, &action)?;
        Driver::<M>::check_named_action(&action, handle.as_ref())?;
        driver
            .execution
            .inputs
            .insert(key.clone(), LiveInput::Pending);
        driver
            .execution
            .events
            .entry(at)
            .or_default()
            .push_back(Scheduled::Control(control::Event::Input {
                key,
                action,
                handle,
            }));
        Ok(())
    }
}

/// Driver setup, execution, or contract failure.
#[derive(Debug)]
pub enum Error<E> {
    /// Invalid named baseline, bandwidth or queue setup.
    NetworkConfiguration(String),
    /// A machine factory failed before its machines were admitted.
    Construction(E),
    /// Definition names must be unique across oracle, initial and spawnable machines.
    DuplicateName(String),
    /// A reserved guest spawn identity prevents host machine addition until dispatch.
    SpawnPending,
    /// Simulation-control policy or correlation failed.
    Control(String),
    /// Quantum must be positive and representable in u64 nanoseconds.
    InvalidQuantum,
    /// At least one execution worker is required.
    InvalidWorkerCount,
    /// Committed position counters cannot represent another boundary, or a
    /// sender's frame sequence cannot order another routed frame.
    PositionOverflow,
    /// A quantum can only be admitted while playback is paused.
    NotPaused,
    /// A previously admitted quantum has not completed.
    StepPending,
    /// A pre-fault arm requires a positive playback rate.
    InvalidRate,
    /// Oracle termination was committed; further scheduling is prohibited.
    Completed,
    /// A permanent stop prohibits execution and scheduling changes.
    Stopped,
    /// Only observation and playback controls are allowed before a held fault.
    BeforeFault,
    /// Drain existing zero-time work before proposing or committing a live input.
    DueWorkPending,
    /// A policy-rejected live proposal cannot be committed.
    InvalidRequest,
    /// The exact prepared position or projected admission outcome changed.
    PreparedInputMismatch,
    /// No machine (or declared spawnable machine) has exactly this name.
    UnknownName(String),
    /// A process command deadline must be positive and representable after
    /// its scheduled time.
    InvalidDeadline(Duration),
    /// A realtime step's magnitude exceeds 365 days (including `i64::MIN`).
    InvalidClockStep(std::num::NonZeroI64),
    /// This input key is live: pending, unsettled or owning an active handle.
    LiveInputKey(InputKey),
    /// The next full quantum boundary, or a frame's arrival, cannot be
    /// represented.
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
    /// A machine's time base cannot represent a required simulation time.
    ClockOverflow {
        id: MachineId,
    },
    /// Two definitions share one MAC address.
    DuplicateMac(crate::definition::MacAddress),
    /// A machine emitted a frame without a network interface, or outside the
    /// frame bounds. The failed batch publishes nothing.
    InvalidFrame {
        id: MachineId,
    },
    /// The arbitrary simulation RNG supplied to new has no capture contract.
    CheckpointUnsupported,
    /// Stopped-boundary bookkeeping is inconsistent; no callbacks were invoked.
    InvalidCheckpoint(&'static str),
    /// A backend's boot-end payload violated committed identity or lifecycle.
    InvalidBootEnd {
        id: MachineId,
        detail: &'static str,
    },
    /// A previous step failed; further execution is prohibited.
    Failed,
}

impl<E: fmt::Display> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(f, "duplicate machine definition name: {name}"),
            Self::SpawnPending => {
                f.write_str("host addition must wait for pending guest spawn dispatch")
            }
            Self::Control(message) => write!(f, "simulation control: {message}"),
            Self::InvalidQuantum => {
                f.write_str("quantum must be positive and fit in u64 nanoseconds")
            }
            Self::InvalidWorkerCount => f.write_str("worker count must be positive"),
            Self::PositionOverflow => f.write_str("simulation position counter overflow"),
            Self::NotPaused => f.write_str("playback must be paused before admitting a quantum"),
            Self::StepPending => f.write_str("a quantum admission is already pending"),
            Self::InvalidRate => f.write_str("pre-fault playback rate must be positive"),
            Self::Completed => f.write_str("simulation has completed"),
            Self::Stopped => f.write_str("driver has been permanently stopped"),
            Self::BeforeFault => {
                f.write_str("only playback controls are allowed before the held fault")
            }
            Self::UnknownName(name) => write!(f, "no unique machine named {name:?}"),
            Self::InvalidClockStep(nanoseconds) => {
                write!(f, "realtime step {nanoseconds} ns exceeds 365 days")
            }
            Self::InvalidDeadline(deadline) => {
                write!(f, "process command deadline {deadline:?} is invalid")
            }
            Self::NetworkConfiguration(message) => write!(f, "network configuration: {message}"),
            Self::LiveInputKey(key) => write!(f, "input key {key} is still live"),
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
            Self::ClockOverflow { id } => {
                write!(
                    f,
                    "machine {} time base exceeds its representable range",
                    id.0
                )
            }
            Self::DuplicateMac(mac) => write!(f, "duplicate machine MAC address: {mac}"),
            Self::InvalidFrame { id } => write!(
                f,
                "machine {} emitted a frame without an interface or outside frame bounds",
                id.0
            ),
            Self::CheckpointUnsupported => f.write_str(
                "driver checkpoint requires new_seeded; arbitrary RNG state cannot be captured",
            ),
            Self::InvalidCheckpoint(message) => write!(f, "invalid driver checkpoint: {message}"),
            Self::DueWorkPending => {
                f.write_str("work due now must be drained before live admission")
            }
            Self::InvalidRequest => f.write_str("cannot commit a rejected input proposal"),
            Self::PreparedInputMismatch => {
                f.write_str("prepared input differs from current admission")
            }
            Self::InvalidBootEnd { id, detail } => {
                write!(f, "machine {}: invalid boot-end evidence: {detail}", id.0)
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[expect(
    clippy::large_enum_variant,
    reason = "network records retain the existing public by-value playback API"
)]
pub enum StepOutcome {
    /// Playback withholds execution because it is paused or a limited rate is
    /// waiting for host time. `Barrier::playback` distinguishes those states.
    /// The suggested delay is bounded for servicing controls; `step` never sleeps.
    Waiting { at: Timestamp, delay: Duration },
    /// This admitted fault's effect is held and has not been applied: either
    /// the arm was consumed, or due-only admission held an unarmed paused effect.
    /// Repeated calls while paused return the same position and fault.
    BeforeFault { at: Timestamp, fault: FaultRecord },
    /// A permanent stop was requested at a barrier. No machine has been crashed.
    Stopped { at: Timestamp },
    /// Further work is runnable or has a known future scheduling time.
    BarrierReached { at: Timestamp },
    /// Unfinished machines can make no progress without new external input.
    /// Frames in flight are pending work, so they prevent this outcome.
    Blocked { at: Timestamp },
    /// Oracle termination was committed: its main application produced an
    /// outcome or a successful Stop acknowledgment, or its machine exited.
    /// An application outcome reports ProgramExit while its guest may remain alive. A confirmed
    /// application Stop reports Shutdown without claiming that its machine exited.
    /// Other machines and queued work remain stopped. This result is permanent;
    /// committed events remain available through `Driver::drain_events`.
    Finished { at: Timestamp, reason: ExitReason },
}

#[cfg(test)]
mod external_work_tests;
#[cfg(test)]
mod network_tests;
#[cfg(test)]
mod queue_tests;
#[cfg(test)]
mod spawn_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod execution_tests;

#[cfg(test)]
pub(crate) mod fixture;

#[cfg(test)]
mod clock_fault_tests;
#[cfg(test)]
mod clock_tests;
#[cfg(test)]
mod control_failure_tests;
#[cfg(test)]
mod control_timer_tests;
#[cfg(test)]
mod evidence_tests;
#[cfg(test)]
mod oracle_correlation_tests;

mod persist;
