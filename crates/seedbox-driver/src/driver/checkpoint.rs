//! Typed, in-memory stopped-boundary continuation. No persistence format.
use super::*;
mod codec;

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod controller_tests;

/// An independently owned, stopped simulation capture.
///
/// This retains all Driver-owned deterministic state,
/// plus each machine's opaque snapshot. It does not retain worker threads,
/// contexts, factories, wall-clock anchors or any observation history, buffers,
/// capacities, omissions or stream cursors. Restore starts a fresh, empty
/// observer session; capture leaves the source session intact.
/// Captures can be borrowed repeatedly after the source Driver is destroyed;
/// machine snapshots need not implement Clone or a serialization trait.
/// This type is not a persistence/export format.
// Machine captures cannot retain private scheduler state. This opaque
// aggregate supplies continuation without exposing fields or expanding the
// Machine contract with Driver or persistence requirements.
pub struct Checkpoint<S> {
    now: Timestamp,
    rng: SeededRng,
    quantum_ns: u64,
    quantum_end: Timestamp,
    machines: Vec<Entry<S>>,
    events: BTreeMap<Timestamp, VecDeque<Scheduled>>,
    boundary_index: u64,
    completed_quanta: u64,
    quantum_committed: bool,
    stopped: bool,
    playback: playback::PlaybackIntent,
    oracle: MachineId,
    finished: Option<(Timestamp, ExitReason)>,
    oracle_stops: BTreeSet<(u64, NonZeroU32)>,
    control: control::Control,
    inputs: BTreeMap<InputKey, LiveInput>,
    trigger_marker: Option<Timestamp>,
    definitions: Vec<MachineDefinition>,
    network: network::Network,
    properties: seedbox_engine_core::properties::Ledger,
}

impl<S> Checkpoint<S> {
    /// The immutable simulated timeslice retained by this capture.
    // Restore adapters validate their requested timeslice against this before
    // native work; envelope metadata alone cannot establish agreement.
    pub fn quantum(&self) -> Duration {
        Duration::from_nanos(self.quantum_ns)
    }

    /// Captured guest-control authority and latency. None is the SDK contract;
    /// an explicit absent controller disables all guest callers.
    pub fn controller_setup(&self) -> Option<ControllerSetup> {
        self.control.controller
    }

    /// Captured Driver identities and declared names, in machine insertion order.
    /// Anonymous machines added through the low-level SDK have no name. This
    /// borrows only the retained control ledger: it never touches native
    /// snapshots or requires them to implement Clone or a serialization trait.
    // Restore adapters validate their name bindings against this before native
    // callbacks; the service envelope is not authority for the captured ledger.
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
    /// same results as new supplied with the same SeededRng algorithm. A plain
    /// [`NetworkConfig`] uses default queue policies without frame observation;
    /// a [`NetworkSetup`] supplies validated baseline/queue policies and opt-in
    /// bounded frame observation, and invalid prepared endpoint membership fails
    /// before context/factory work.
    // An arbitrary RngStream has no capture contract; owning the seeded stream
    // enables exact continuation without changing RngStream, guessing its
    // algorithm, or recording/replaying random requests. `new` keeps that
    // generality for scripted test and example streams only.
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
        network: impl Into<NetworkSetup>,
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

    /// Capture the current valid stopped boundary without executing simulation work.
    ///
    /// Paused, held-before-fault, permanently stopped and completed simulations
    /// retain their logical state; capture never resumes them. The capture owns
    /// exact scheduling, control, network and RNG state. Observation
    /// history and external construction authority remain the caller's concern.
    ///
    /// Drivers constructed with an arbitrary RNG through [`Self::new`] return
    /// [`Error::CheckpointUnsupported`] before invoking any machine. A poisoned
    /// Driver rejects capture. If any machine capture fails, no whole capture is
    /// returned; partial captures are dropped and the valid source remains at
    /// the same simulation state, as required by [`Machine::snapshot`]. Capture
    /// does not flush, drain or republish observations and is not persistence.
    // Only the Driver owns the exact private continuation state; capturing it
    // here avoids rounded reconstruction from observations or a second
    // scheduler above Machine.
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
    /// The result owns the same exact continuing state as
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
    // Mapping inside the capture loop lets caller-owned codecs stream each
    // snapshot instead of retaining every native RAM image until capture ends,
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
            boundary_index: self.execution.boundary_index,
            completed_quanta: self.execution.completed_quanta,
            quantum_committed: self.execution.quantum_committed,
            stopped: self.execution.stopped,
            playback: self.playback.checkpoint(),
            oracle: self.execution.oracle,
            finished: self.execution.finished,
            oracle_stops: self.execution.oracle_stops.clone(),
            control: self.execution.control.clone(),
            inputs: self.execution.inputs.clone(),
            trigger_marker: self.execution.trigger_marker,
            definitions: self.execution.definitions.clone(),
            network: self.execution.network.clone(),
            properties: self.execution.properties.clone(),
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
    /// Observation history is not restored. Service input-program cursors must be retained
    /// separately by the host; this operation alone is not checkpoint export.
    // Opaque backend restore callbacks supply native resources without a
    // common Machine constructor. Building a new Driver makes partial restore
    // failure cleanable without mutating a live Driver in place.
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
                stopped: checkpoint.stopped,
                oracle: checkpoint.oracle,
                finished: checkpoint.finished,
                oracle_stops: checkpoint.oracle_stops.clone(),
                control: checkpoint.control.clone(),
                inputs: checkpoint.inputs.clone(),
                trigger_marker: checkpoint.trigger_marker,
                engine: None,
                definitions: checkpoint.definitions.clone(),
                network: checkpoint.network.clone(),
                properties: checkpoint.properties.clone(),
            },
            workers,
            observations: ObservationState::default(),
            playback: Playback::from_intent(checkpoint.playback.clone()),
            factory: Box::new(factory),
        })
    }
}
