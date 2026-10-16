//! Typed, in-memory stopped-boundary continuation. No persistence format.
use super::*;
mod codec;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod controller_tests;

/// An independently owned, stopped simulation capture.
///
/// This retains all Driver-owned deterministic state and undrained publications,
/// plus each machine's opaque snapshot. It does not retain worker threads,
/// contexts, factories, wall-clock anchors or history already drained by the host.
/// Captures can be borrowed repeatedly after the source Driver is destroyed;
/// machine snapshots need not implement Clone or a serialization trait.
/// This type is not a persistence/export format.
// TODO: Re-evaluate: machine captures cannot retain private scheduler state.
// An opaque aggregate supplies production continuation without exposing fields
// or expanding the Machine contract with Driver or persistence requirements.
pub struct Checkpoint<S> {
    now: Timestamp,
    rng: SeededRng,
    quantum_ns: u64,
    quantum_end: Timestamp,
    machines: Vec<Entry<S>>,
    events: BTreeMap<Timestamp, VecDeque<Scheduled>>,
    observations: ObservationState,
    boundary_index: u64,
    completed_quanta: u64,
    quantum_committed: bool,
    playback: Playback,
    oracle: MachineId,
    finished: Option<(Timestamp, ExitReason)>,
    oracle_stops: BTreeSet<(u64, NonZeroU32)>,
    control: control::Control,
    inputs: BTreeMap<InputKey, LiveInput>,
    trigger_marker: Option<Timestamp>,
    definitions: Vec<MachineDefinition>,
    network: network::Network,
}

impl<S> Checkpoint<S> {
    /// Captured guest-control authority and latency. None is the SDK contract;
    /// an explicit absent controller disables all guest callers.
    pub fn controller_setup(&self) -> Option<ControllerSetup> {
        self.control.controller
    }

    /// Captured Driver identities and declared names, in machine insertion order.
    /// Anonymous machines added through the low-level SDK have no name. This
    /// borrows only the retained control ledger: it never touches native
    /// snapshots or requires them to implement Clone or a serialization trait.
    // TODO: Re-evaluate: external restore adapters need to validate their name
    // bindings before native callbacks; the service envelope is not authority
    // for the independently captured Driver ledger.
    pub fn machine_names(&self) -> impl ExactSizeIterator<Item = (MachineId, Option<&str>)> {
        self.control.machine_names()
    }
}

/// A failed streaming capture, preserving the machine or mapper error type.
/// No partial checkpoint is returned. External mapper side effects remain owned
/// by the caller; this error does not imply rollback of files or publication.
#[derive(Debug)]
pub enum CheckpointMapError<C, E> {
    /// Boundary validation or machine snapshot capture failed.
    Capture(Error<C>),
    /// The caller's snapshot mapper failed.
    Map(E),
}

impl<C: fmt::Display, E: fmt::Display> fmt::Display for CheckpointMapError<C, E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Capture(error) => write!(f, "checkpoint capture: {error}"),
            Self::Map(error) => write!(f, "checkpoint snapshot mapping: {error}"),
        }
    }
}

impl<C: std::error::Error + 'static, E: std::error::Error + 'static> std::error::Error
    for CheckpointMapError<C, E>
{
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Capture(error) => Some(error),
            Self::Map(error) => Some(error),
        }
    }
}

impl<T> Entry<T> {
    /// Copy only stopped bookkeeping, supplying the independently owned machine.
    fn with_machine<U>(&self, machine: U) -> Entry<U> {
        Entry {
            machine: Some(machine),
            rng: self.rng.clone(),
            lifecycle: self.lifecycle.clone(),
            time: self.time,
            position: self.position,
            timer: self.timer,
            reached: self.reached,
            stopped: self.stopped,
            inbox: self.inbox.clone(),
            staged: self.staged.clone(),
            last_ended_published: self.last_ended_published,
        }
    }
}

/// Typed captures have no external decoder or public mutation. Check the
/// stopped structural invariants before any user callback; nested policy state
/// is copied intact, not reconstructed from presentation events.
fn validate<T>(
    now: Timestamp,
    quantum_ns: u64,
    quantum_end: Timestamp,
    oracle: MachineId,
    machines: &[Entry<T>],
    control: &control::Control,
    network: &network::Network,
) -> Result<(), &'static str> {
    if quantum_ns == 0 || quantum_end < now {
        return Err("invalid quantum boundary");
    }
    if oracle.0 >= machines.len() {
        return Err("missing oracle");
    }
    if !control.checkpoint_size(machines.len()) || !network.checkpoint_size(machines.len()) {
        return Err("machine identity tables differ");
    }
    for entry in machines {
        if entry.machine.is_none() || entry.rng.is_none() {
            return Err("machine or RNG is still owned by a worker");
        }
        // Paused, blocked and exited machines may lag shared time so far that
        // mapping `now` into their time base is no longer representable. No run
        // requires that conversion yet; capture must not reject a valid source.
        if entry
            .last_ended_published
            .is_some_and(|boot| boot > entry.lifecycle.boot())
        {
            return Err("retired boot exceeds committed lifecycle ordinal");
        }
        if entry.reached > now || entry.time.anchor() > now.as_nanos() {
            return Err("invalid stopped machine position");
        }
    }
    Ok(())
}

fn validate_publications(
    events: &[Event],
    next: u64,
    position: Position,
) -> Result<(), &'static str> {
    let count = u64::try_from(events.len()).map_err(|_| "publication length overflow")?;
    let first = next
        .checked_sub(count)
        .ok_or("invalid publication cursor")?;
    for (event, sequence) in events.iter().zip(first..next) {
        if event.sequence != sequence
            || event.position.boundary_index > position.boundary_index
            || event.position.completed_quanta > position.completed_quanta
            || event.position.at > position.at
            || event.at > position.at
        {
            return Err("invalid publication suffix");
        }
    }
    Ok(())
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    /// Construct a capturable Driver using the existing deterministic seeded stream.
    ///
    /// All construction, seed-assignment and execution semantics are those of
    /// [`Self::new`]; the simulation RNG is owned concretely so its exact byte
    /// cursor can be captured. The same seed and subsequent inputs produce the
    /// same results as new supplied with the same SeededRng algorithm.
    // TODO: Re-evaluate: an arbitrary RngStream has no capture contract. Owning
    // the existing seeded stream enables exact continuation without changing
    // RngStream, guessing its algorithm, or recording/replaying random requests.
    #[allow(clippy::too_many_arguments)]
    pub fn new_seeded(
        start: Timestamp,
        quantum: Duration,
        workers: usize,
        seed: u64,
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
            SimulationRng::Seeded(SeededRng::new(seed)),
            context,
            oracle,
            machines,
            spawnable,
            network.into(),
            factory,
        )
    }

    /// Construct with validated baseline/queue policies and opt-in bounded
    /// frame observation. Other construction and RNG semantics match new_seeded;
    /// invalid prepared endpoint membership fails before context/factory work.
    // TODO: Re-evaluate: full network setup belongs to the Driver, not factories
    // or a changed default on the existing NetworkConfig constructor.
    #[allow(clippy::too_many_arguments)]
    pub fn new_seeded_with_network(
        start: Timestamp,
        quantum: Duration,
        workers: usize,
        seed: u64,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        oracle: MachineDefinition,
        machines: Vec<MachineDefinition>,
        spawnable: Vec<MachineDefinition>,
        network: NetworkSetup,
        factory: impl FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
    ) -> Result<Self, Error<M::Error>> {
        Self::construct(
            start,
            quantum,
            workers,
            SimulationRng::Seeded(SeededRng::new(seed)),
            context,
            oracle,
            machines,
            spawnable,
            network,
            factory,
        )
    }

    /// Capture the current valid stopped boundary without executing simulation work.
    ///
    /// Paused, held-before-fault, permanently stopped and completed simulations
    /// retain their logical state; capture never resumes them. The capture owns
    /// exact scheduling, control, network, RNG and publication state. Drained
    /// history and external construction authority remain the caller's concern.
    ///
    /// Drivers constructed with an arbitrary RNG through [`Self::new`] return
    /// [`Error::CheckpointUnsupported`] before invoking any machine. A poisoned
    /// Driver rejects capture. If any machine capture fails, no whole capture is
    /// returned; partial captures are dropped and the valid source remains at
    /// the same simulation state, as required by [`Machine::snapshot`]. Capture
    /// does not flush, drain or republish observations and is not persistence.
    // TODO: Re-evaluate: only the Driver owns the exact private continuation
    // state. Capturing it here avoids rounded reconstruction from observations
    // or a second scheduler above Machine.
    pub fn checkpoint(&mut self) -> Result<Checkpoint<M::Snapshot>, Error<M::Error>> {
        match self.checkpoint_map(Ok::<_, std::convert::Infallible>) {
            Ok(checkpoint) => Ok(checkpoint),
            Err(CheckpointMapError::Capture(error)) => Err(error),
            Err(CheckpointMapError::Map(never)) => match never {},
        }
    }

    /// Capture and immediately map each machine snapshot at the current boundary.
    ///
    /// All validity checks used by [`Self::checkpoint`] precede the first capture
    /// or mapper call. Machines are captured in insertion order; each successful
    /// snapshot moves into `map` exactly once, before the next machine is captured.
    /// The Driver retains only the mapped results, not earlier native snapshots.
    /// A mapper that serializes and drops its input can therefore limit retained
    /// backend snapshots to one. A snapshot may itself allocate; mapped results
    /// may retain snapshots, and Rust metadata/future definitions still take space.
    /// This is not a bound on whole-process memory or an allocation-free operation.
    ///
    /// The result owns the same exact continuation and publication state as
    /// `checkpoint`, without Clone or serialization bounds on snapshots. No guest
    /// runs, RNG draws, factory calls, admission or source execution changes occur.
    /// Paused, held, stopped and completed boundaries are preserved, not resumed.
    ///
    /// Capture and mapper failures are distinguished by [`CheckpointMapError`].
    /// Either failure stops further captures/mappings and drops partial mapped
    /// results and owned capture bookkeeping; no partial checkpoint is returned
    /// and there is no retry. A capture error preserves the source under
    /// [`Machine::snapshot`]'s contract; a mapper error leaves the source at the
    /// same boundary. This does not permit resuming a failed [`Machine::run`].
    /// External I/O side effects, rollback and manifest publication belong entirely
    /// to the caller: this operation neither publishes nor removes their artifacts.
    // TODO: Re-evaluate: mapping after checkpoint retains every native RAM image.
    // Mapping inside the capture loop enables caller-owned streaming codecs
    // without expanding Machine or imposing a resource admission ceiling.
    pub fn checkpoint_map<S, E>(
        &mut self,
        mut map: impl FnMut(M::Snapshot) -> Result<S, E>,
    ) -> Result<Checkpoint<S>, CheckpointMapError<M::Error, E>> {
        let capture = CheckpointMapError::Capture;
        self.check_valid().map_err(capture)?;
        let SimulationRng::Seeded(rng) = &self.execution.rng else {
            return Err(capture(Error::CheckpointUnsupported));
        };
        validate(
            self.now(),
            self.execution.quantum_ns,
            self.execution.quantum_end,
            self.execution.oracle,
            &self.execution.machines,
            &self.execution.control,
            &self.execution.network,
        )
        .map_err(|error| capture(Error::InvalidCheckpoint(error)))?;
        validate_publications(
            &self.observations.published,
            self.observations.next_sequence,
            self.position(),
        )
        .map_err(|error| capture(Error::InvalidCheckpoint(error)))?;
        let rng = rng.clone();
        let machines = self
            .execution
            .machines
            .iter_mut()
            .enumerate()
            .map(|(index, entry)| {
                let snapshot = entry
                    .machine
                    .as_mut()
                    .expect("validated stopped machine")
                    .snapshot()
                    .map_err(|source| {
                        capture(Error::Machine {
                            id: MachineId(index),
                            source,
                        })
                    })?;
                let mapped = map(snapshot).map_err(CheckpointMapError::Map)?;
                Ok(entry.with_machine(mapped))
            })
            .collect::<Result<_, CheckpointMapError<M::Error, E>>>()?;
        Ok(Checkpoint {
            now: self.now(),
            rng,
            machines,
            quantum_ns: self.execution.quantum_ns,
            quantum_end: self.execution.quantum_end,
            events: self.execution.events.clone(),
            observations: self.observations.clone(),
            boundary_index: self.execution.boundary_index,
            completed_quanta: self.execution.completed_quanta,
            quantum_committed: self.execution.quantum_committed,
            playback: self.playback.checkpoint(),
            oracle: self.execution.oracle,
            finished: self.execution.finished,
            oracle_stops: self.execution.oracle_stops.clone(),
            control: self.execution.control.clone(),
            inputs: self.execution.inputs.clone(),
            trigger_marker: self.execution.trigger_marker,
            definitions: self.execution.definitions.clone(),
            network: self.execution.network.clone(),
        })
    }

    /// Construct a new stopped Driver by borrowing an independently owned capture.
    ///
    /// Worker count may differ from the source. Contexts are constructed and
    /// destroyed on their worker threads. Structural validation precedes all
    /// callbacks. The restore callback is called once per snapshot in identity
    /// order and must reconstruct it without executing guest work or changing
    /// the capture. No initial machine factory is called and no seed is drawn.
    /// On failure, temporary machines are dropped and workers are joined; the
    /// source and capture are unchanged. Playback keeps its logical gates and
    /// pending step target but starts a new host pacing anchor.
    ///
    /// The caller supplies matching backend executables/resources through the
    /// restore callback, and matching future construction/configuration authority
    /// through the future factory. In particular, a stateful FnMut factory must
    /// be supplied at its captured logical construction position, not restarted
    /// at its original initial-call count. Contexts/factories must not introduce
    /// host timing or worker identity into simulation behavior. These external
    /// obligations are not serialized or validated by this typed capture.
    /// Already-drained history and service input-program cursors must be retained
    /// separately by the host; this operation alone is not checkpoint export.
    // TODO: Re-evaluate: opaque backend restore callbacks supply native resources
    // without adding a common Machine constructor. New ownership makes partial
    // restoration failure cleanable without mutating a live Driver in place.
    pub fn from_checkpoint(
        checkpoint: &Checkpoint<M::Snapshot>,
        workers: usize,
        context: impl Fn() -> Result<M::Context, M::Error> + Send + Sync + 'static,
        mut restore: impl FnMut(&M::Snapshot) -> Result<M, M::Error>,
        factory: impl FnMut(&MachineDefinition) -> Result<M, M::Error> + Send + 'static,
    ) -> Result<Self, Error<M::Error>> {
        if workers == 0 {
            return Err(Error::InvalidWorkerCount);
        }
        validate(
            checkpoint.now,
            checkpoint.quantum_ns,
            checkpoint.quantum_end,
            checkpoint.oracle,
            &checkpoint.machines,
            &checkpoint.control,
            &checkpoint.network,
        )
        .map_err(Error::InvalidCheckpoint)?;
        validate_publications(
            &checkpoint.observations.published,
            checkpoint.observations.next_sequence,
            Position {
                boundary_index: checkpoint.boundary_index,
                completed_quanta: checkpoint.completed_quanta,
                at: checkpoint.now,
            },
        )
        .map_err(Error::InvalidCheckpoint)?;
        let workers = Workers::new(workers, context)?;
        let machines = checkpoint
            .machines
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let machine = restore(entry.machine.as_ref().expect("validated snapshot"))
                    .map_err(|source| Error::Machine {
                        id: MachineId(index),
                        source,
                    })?;
                Ok(entry.with_machine(machine))
            })
            .collect::<Result<_, Error<M::Error>>>()?;
        Ok(Self {
            execution: ExecutionState {
                clock: Arc::new(SimulationClock::new(checkpoint.now.as_nanos())),
                rng: SimulationRng::Seeded(checkpoint.rng.clone()),
                quantum_ns: checkpoint.quantum_ns,
                quantum_end: checkpoint.quantum_end,
                machines,
                events: checkpoint.events.clone(),
                boundary_index: checkpoint.boundary_index,
                completed_quanta: checkpoint.completed_quanta,
                quantum_committed: checkpoint.quantum_committed,
                failed: false,
                oracle: checkpoint.oracle,
                finished: checkpoint.finished,
                oracle_stops: checkpoint.oracle_stops.clone(),
                control: checkpoint.control.clone(),
                inputs: checkpoint.inputs.clone(),
                trigger_marker: checkpoint.trigger_marker,
                definitions: checkpoint.definitions.clone(),
                network: checkpoint.network.clone(),
            },
            workers,
            observations: checkpoint.observations.clone(),
            playback: checkpoint.playback.checkpoint(),
            factory: Box::new(factory),
        })
    }
}
