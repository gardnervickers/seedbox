//! Serializes resource mutations with status subscription. Execution happens
//! outside this lock; only durable lifecycle changes and their publication use it.

use crate::catalogue::{Catalogue, Error};
use crate::stream::{History, Payload, PublishError, Stream, Subscription};
use seedbox_api::{ControlReceipt, ControlRequest, ControlState, LogEntry};
use seedbox_api::{EventKind, SimulationEvent};
use seedbox_api::{ExecutionTermination, Phase, Simulation, SimulationId, SubmitRequest};
use seedbox_application::execution::CancellationToken;
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::sync::watch;

struct Live {
    worker: Option<Arc<Mutex<crate::worker::Worker>>>,
    timing: crate::timing::StageClock,
    playback: Option<(seedbox_api::PlaybackStatus, Instant)>,
    cancellation: CancellationToken,
    status: watch::Sender<Simulation>,
    events: Stream<SimulationEvent>,
    logs: BTreeMap<String, Stream<LogEntry>>,
    all_logs: Stream<LogEntry>,
    frames: Stream<FrameBatch>,
}

/// One observation's routed frames, published on the frame stream.
#[derive(Debug)]
pub struct FrameBatch {
    pub simulated_time_ns: u64,
    pub frames: Vec<seedbox_api::Frame>,
}

impl Payload for FrameBatch {
    fn payload_bytes(&self) -> usize {
        self.frames
            .iter()
            .map(|f| std::mem::size_of::<seedbox_api::Frame>() + f.from.len() + f.to.len())
            .sum()
    }
}

impl Payload for LogEntry {
    fn payload_bytes(&self) -> usize {
        match self {
            LogEntry::Bytes { machine, bytes, .. } => machine.len() + bytes.len(),
            LogEntry::MachineGap { machine, .. } => machine.len() + std::mem::size_of::<Self>(),
            _ => std::mem::size_of::<Self>(),
        }
    }
}

impl Payload for SimulationEvent {
    fn payload_bytes(&self) -> usize {
        std::mem::size_of::<Self>().saturating_add(match &self.event {
            EventKind::OverrideRejected {
                input_key, reason, ..
            } => input_key.len() + reason.len(),
            EventKind::ExecutionTerminated {
                termination: ExecutionTermination::Error { message },
            } => message.len(),
            EventKind::ControlChanged { control } => {
                serde_json::to_vec(control).map_or(usize::MAX, |bytes| bytes.len())
            }
            EventKind::FaultChanged {
                action,
                machine,
                plan,
                input_key,
                ..
            } => {
                action.len()
                    + input_key.as_ref().map_or(0, String::len)
                    + machine.as_ref().map_or(0, String::len)
                    + plan.as_ref().map_or(0, |plan| {
                        serde_json::to_vec(plan).map_or(usize::MAX, |bytes| bytes.len())
                    })
            }
            EventKind::PreparationProgress { machine, .. }
            | EventKind::MachineReady { machine, .. }
            | EventKind::MachineLifecycle { machine, .. }
            | EventKind::MachineExited { machine, .. } => machine.len(),
            _ => 0,
        })
    }
}

impl Live {
    fn new(
        resource: &Simulation,
        history: &Arc<History<LogEntry>>,
        event_history: &Arc<History<SimulationEvent>>,
    ) -> Self {
        Self {
            worker: None,
            all_logs: Stream::new(
                NonZeroUsize::MIN.saturating_add(255),
                NonZeroUsize::MIN.saturating_add(4095),
            )
            .with_history(history.clone()),
            timing: Default::default(),
            playback: None,
            logs: resource
                .spec
                .configuration
                .services
                .keys()
                .map(|name| {
                    (
                        name.clone(),
                        Stream::new(
                            NonZeroUsize::MIN.saturating_add(255),
                            NonZeroUsize::MIN.saturating_add(4095),
                        )
                        .with_history(history.clone()),
                    )
                })
                .collect(),
            cancellation: CancellationToken::default(),
            status: watch::channel(resource.clone()).0,
            frames: Stream::new(
                NonZeroUsize::MIN.saturating_add(255),
                NonZeroUsize::MIN.saturating_add(65535),
            ),
            events: Stream::new(
                NonZeroUsize::MIN.saturating_add(255),
                NonZeroUsize::MIN.saturating_add(8191),
            )
            .with_history(event_history.clone()),
        }
    }

    fn publish_event(&self, mut event: SimulationEvent) {
        // An oversized observation loses only its payload, never the live stream.
        // Durable control receipts remain available through resource inspection.
        match self.events.publish(event.clone()) {
            Err(PublishError::TooLarge { .. }) => {
                event.event = EventKind::SourceUnavailable;
                if self.events.publish(event).is_err() {
                    let _ = self.events.close();
                }
            }
            Err(_) => {
                let _ = self.events.close();
            }
            Ok(_) => {}
        }
    }

    fn project(&self, status: &mut seedbox_api::SimulationStatus, now: Instant) {
        self.timing.project(status, now);
        if let Some((sample, received)) = &self.playback {
            status.simulated_time_ns = status
                .simulated_time_ns
                .max(sample.position.simulated_time_ns);
            status.playback = Some(seedbox_api::PlaybackObservation {
                state: sample.clone(),
                age_ns: now
                    .saturating_duration_since(*received)
                    .as_nanos()
                    .min(u128::from(u64::MAX)) as u64,
            });
        }
    }

    fn publish(&self, resource: &Simulation) {
        let previous = self.status.borrow().clone();
        let mut kinds = Vec::with_capacity(3);
        if previous.metadata.deletion_requested_at_unix_ms.is_none()
            && resource.metadata.deletion_requested_at_unix_ms.is_some()
        {
            kinds.push(EventKind::DeletionRequested);
        }
        if previous.status.termination != resource.status.termination {
            if let Some(termination) = &resource.status.termination {
                kinds.push(EventKind::ExecutionTerminated {
                    termination: termination.clone(),
                });
            }
        }

        if previous.status.phase != resource.status.phase
            || previous.status.stage != resource.status.stage
        {
            kinds.push(EventKind::LifecycleChanged {
                phase: resource.status.phase,
                stage: resource.status.stage,
            });
        }
        if !previous.status.stop_requested && resource.status.stop_requested {
            kinds.push(EventKind::StopRequested);
        }
        if previous.status.oracle_exit_code != resource.status.oracle_exit_code {
            if let Some(code) = resource.status.oracle_exit_code {
                kinds.push(EventKind::OracleExited { code });
            }
        }
        for event in kinds {
            self.publish_event(SimulationEvent {
                simulation: resource.metadata.id.clone(),
                revision: resource.metadata.revision,
                simulated_time_ns: (!matches!(
                    previous.status.phase,
                    Phase::Queued | Phase::Preparing
                ) || matches!(
                    resource.status.phase,
                    Phase::Running | Phase::Finalizing
                ))
                .then_some(resource.status.simulated_time_ns),
                position: None,
                event,
            });
        }
        self.status.send_replace(resource.clone());
    }
}

struct RetainedLogs {
    machines: BTreeMap<String, Stream<LogEntry>>,
    all: Stream<LogEntry>,
    events: Stream<SimulationEvent>,
}

struct State {
    log_history: Arc<History<LogEntry>>,
    /// Events share their own budget so a chatty log never evicts them.
    event_history: Arc<History<SimulationEvent>>,
    retained_logs: BTreeMap<SimulationId, RetainedLogs>,
    catalogue: Catalogue,
    live: BTreeMap<SimulationId, Live>,
    /// Observed worlds outlive their workers until the simulation is deleted.
    worlds: BTreeMap<SimulationId, crate::world::Tracker>,
    imported_worlds: BTreeMap<SimulationId, seedbox_api::WorldState>,
    results: BTreeMap<SimulationId, ResultEvidence>,
    accepting: bool,
}

pub struct Manager(Mutex<State>);

/// Completed evidence owned by the server session, independent of worker paths.
#[derive(Clone)]
pub(crate) struct ResultEvidence {
    pub definition: Box<seedbox_spec::definition::Definition>,
    pub record: seedbox_artifacts::record::RunRecord,
}

pub(crate) struct DeletionTarget {
    pub id: SimulationId,
    pub worker: Option<Arc<Mutex<crate::worker::Worker>>>,
}

impl State {
    fn persist(&mut self, mut resource: Simulation) -> Result<Simulation, Error> {
        // An operational control failure survives execution cancellation and
        // cleanup; only the execution owner decides when the resource completes.
        if let Some(termination @ ExecutionTermination::Error { .. }) = &self
            .catalogue
            .get(&resource.metadata.id)?
            .status
            .termination
        {
            resource.status.termination = Some(termination.clone());
        }
        let id = resource.metadata.id.clone();
        if let Some((sample, _)) = self.live.get(&id).and_then(|live| live.playback.as_ref()) {
            resource.status.simulated_time_ns = resource
                .status
                .simulated_time_ns
                .max(sample.position.simulated_time_ns);
        }
        let timing = self
            .live
            .get(&id)
            .map(|live| live.timing.transition(&mut resource.status, Instant::now()));
        let pending = if resource.status.phase == Phase::Completed {
            self.catalogue
                .pending_controls()
                .into_iter()
                .filter(|(simulation, _)| simulation == &id)
                .map(|(_, request)| request.request_id)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let resource = self.catalogue.update(resource)?;
        for request_id in pending {
            if let Some(control) = self.catalogue.control_receipt(&id, &request_id)?.cloned() {
                self.publish_control(&id, control)?;
            }
        }
        if let Some(live) = self.live.get_mut(&id) {
            if let Some(timing) = timing {
                live.timing = timing;
            }
            live.publish(&resource);
        }
        if resource.status.phase == Phase::Completed {
            if let Some(live) = self.live.remove(&id) {
                let _ = live.all_logs.close();
                let _ = live.events.close();
                for logs in live.logs.values() {
                    let _ = logs.close();
                }
                self.retained_logs.insert(
                    id.clone(),
                    RetainedLogs {
                        machines: live.logs,
                        all: live.all_logs,
                        events: live.events,
                    },
                );
            }
        }
        Ok(resource)
    }

    fn publish_control(&self, id: &SimulationId, control: ControlReceipt) -> Result<(), Error> {
        if let Some(live) = self.live.get(id) {
            let resource = self.catalogue.get(id)?;
            let position = match &control.state {
                ControlState::Acknowledged { status, .. } => Some(status.position),
                _ => None,
            };
            let event = SimulationEvent {
                simulation: id.clone(),
                revision: resource.metadata.revision,
                simulated_time_ns: position.map(|position| position.simulated_time_ns),
                position,
                event: EventKind::ControlChanged {
                    control: Box::new(
                        serde_json::to_value(control).expect("control receipt serializes"),
                    ),
                },
            };
            live.publish_event(event);
        }
        Ok(())
    }

    fn project(&self, mut resource: Simulation, now: Instant) -> Simulation {
        if let Some(live) = self.live.get(&resource.metadata.id) {
            live.project(&mut resource.status, now);
        }
        resource
    }
}

impl Manager {
    pub(crate) fn control_work(
        &self,
    ) -> Result<Vec<(SimulationId, Option<ControlRequest>)>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let pending = state.catalogue.pending_controls();
        Ok(state
            .live
            .iter()
            .filter(|(_, live)| {
                live.worker.is_some()
                    && !matches!(
                        live.status.borrow().status.termination,
                        Some(ExecutionTermination::Error { .. })
                    )
            })
            .map(|(id, _)| {
                (
                    id.clone(),
                    pending
                        .iter()
                        .find(|(simulation, _)| simulation == id)
                        .map(|(_, request)| request.clone()),
                )
            })
            .collect())
    }

    pub(crate) fn observe_playback(
        &self,
        id: &SimulationId,
        status: seedbox_api::PlaybackStatus,
    ) -> Result<(), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let mut became_ready = false;
        if let Some(live) = state.live.get_mut(id) {
            if live.playback.as_ref().is_none_or(|(previous, _)| {
                status.position.completed_rounds >= previous.position.completed_rounds
                    && (status.position.boundary_index > previous.position.boundary_index
                        || (status.position.boundary_index == previous.position.boundary_index
                            && status.position.simulated_time_ns
                                >= previous.position.simulated_time_ns))
            }) {
                became_ready = status.application_ready;
                live.playback = Some((status, Instant::now()));
            }
        }
        if became_ready {
            let mut resource = state.catalogue.get(id)?.clone();
            if resource.status.phase == Phase::Running
                && resource.status.stage == Some(seedbox_api::Stage::Booting)
            {
                resource.status.stage = Some(seedbox_api::Stage::Executing);
                state.persist(resource)?;
            }
        }
        Ok(())
    }

    pub(crate) fn delete_checkpoint(&self, id: &SimulationId, name: &str) -> Result<(), Error> {
        self.0
            .lock()
            .map_err(|_| Error::StorageFailed)?
            .catalogue
            .delete_checkpoint(id, name)
    }

    pub(crate) fn captured_checkpoint(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<seedbox_engine_protocol::host_control::CheckpointStatus, Error> {
        self.0
            .lock()
            .map_err(|_| Error::StorageFailed)?
            .catalogue
            .captured_checkpoint(id, request_id)
    }

    pub(crate) fn record_worker_control_failure(
        &self,
        id: &SimulationId,
        message: String,
    ) -> Result<(), Error> {
        {
            let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
            let mut resource = match state.catalogue.get(id) {
                Ok(resource) if resource.status.phase != Phase::Completed => resource.clone(),
                Ok(_) | Err(Error::NotFound(_)) => return Ok(()),
                Err(error) => return Err(error),
            };
            resource.status.termination = Some(ExecutionTermination::Error {
                message: format!("worker control: {message}"),
            });
            state.persist(resource)?;
            if let Some(live) = state.live.get(id) {
                live.cancellation.cancel();
            }
        }
        Ok(())
    }

    pub fn accept_control(
        &self,
        id: &SimulationId,
        request: ControlRequest,
    ) -> Result<ControlReceipt, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let existing = state
            .catalogue
            .control_receipt(id, &request.request_id)?
            .is_some();
        if !existing
            && (!state.accepting
                || state
                    .live
                    .get(id)
                    .and_then(|live| live.worker.as_ref())
                    .is_none())
        {
            return Err(Error::ControlUnavailable);
        }
        let (control, created) = state.catalogue.accept_control(id, request)?;
        if created {
            state.publish_control(id, control.clone())?;
        }
        Ok(control)
    }

    pub(crate) fn acknowledge_control(
        &self,
        id: &SimulationId,
        request_id: &str,
        receipt: ControlState,
    ) -> Result<(), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        // A transport reply may race completed-resource deletion.
        if matches!(state.catalogue.get(id), Err(Error::NotFound(_))) {
            return Ok(());
        }

        let pending = state
            .catalogue
            .control_receipt(id, request_id)?
            .is_some_and(|control| control.state == ControlState::Accepted);
        let control = state
            .catalogue
            .acknowledge_control(id, request_id, receipt)?;
        if pending {
            state.publish_control(id, control)?;
        }
        Ok(())
    }

    pub(crate) fn attach_worker(
        &self,
        id: &SimulationId,
        directory: &std::path::Path,
    ) -> Result<(), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        if state
            .catalogue
            .get(id)?
            .metadata
            .deletion_requested_at_unix_ms
            .is_some()
        {
            return Err(Error::ControlUnavailable);
        }
        let live = state
            .live
            .get_mut(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        if live.worker.is_some() {
            return Err(Error::Invalid("worker already attached".into()));
        }
        live.worker = Some(Arc::new(Mutex::new(crate::worker::Worker::new(
            directory.to_owned(),
        ))));
        Ok(())
    }

    /// Join outstanding control I/O before execution publishes completion.
    /// The manager mutex is released before waiting for the worker connection.
    pub(crate) fn retire_worker(&self, id: &SimulationId) -> Result<(), Error> {
        if let Some(worker) = self.worker(id)? {
            crate::worker::lock_for_retirement(&worker)
                .map_err(|_| Error::StorageFailed)?
                .retire();
        }
        Ok(())
    }

    pub(crate) fn worker(
        &self,
        id: &SimulationId,
    ) -> Result<Option<Arc<Mutex<crate::worker::Worker>>>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        Ok(state.live.get(id).and_then(|live| live.worker.clone()))
    }

    /// Call after acquiring the server catalogue lock. Current native workers
    /// are tied to their owner's pipe and cannot survive its disappearance.
    /// Queued submissions remain runnable; partially executed worlds do not.
    pub fn recover_interrupted(&self) -> Result<(), Error> {
        for id in self.read(|catalogue| catalogue.stopped_seed_members())? {
            self.stop(&id)?;
        }
        let ids = self.read(|catalogue| {
            Ok(catalogue
                .list()
                .filter(|s| !matches!(s.status.phase, Phase::Queued | Phase::Completed))
                .map(|s| s.metadata.id.clone())
                .collect::<Vec<_>>())
        })?;
        for id in ids {
            self.update(&id, |s| {
                s.status.phase = Phase::Completed;
                s.status.stage = None;
                s.status.termination = Some(ExecutionTermination::Interrupted);
            })?;
        }
        Ok(())
    }
    pub fn new(catalogue: Catalogue) -> Self {
        let log_history = Arc::new(History::new(64 * 1024 * 1024));
        let event_history = Arc::new(History::new(16 * 1024 * 1024));
        let live = catalogue
            .list()
            .filter(|s| s.status.phase != Phase::Completed)
            .map(|s| {
                (
                    s.metadata.id.clone(),
                    Live::new(s, &log_history, &event_history),
                )
            })
            .collect();
        Self(Mutex::new(State {
            catalogue,
            log_history,
            event_history,
            retained_logs: BTreeMap::new(),
            live,
            accepting: true,
            worlds: BTreeMap::new(),
            imported_worlds: BTreeMap::new(),
            results: BTreeMap::new(),
        }))
    }

    pub fn read<T>(&self, read: impl FnOnce(&Catalogue) -> Result<T, Error>) -> Result<T, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        read(&state.catalogue)
    }

    /// Durations reflect the query instant; their projection does not change
    /// the revision of the last durable lifecycle mutation.
    pub fn describe(&self, id: &SimulationId) -> Result<Simulation, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        Ok(state.project(state.catalogue.get(id)?.clone(), Instant::now()))
    }

    pub(crate) fn capacity(
        &self,
        slots: usize,
        occupying: &std::collections::BTreeSet<SimulationId>,
    ) -> Result<seedbox_api::ExecutionCapacity, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let now = Instant::now();
        Ok(crate::capacity::project(
            slots,
            occupying,
            state
                .catalogue
                .list()
                .map(|s| state.project(s.clone(), now)),
        ))
    }

    pub fn list(&self, after: Option<&SimulationId>) -> Result<seedbox_api::SimulationList, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let now = Instant::now();
        let mut entries = state
            .catalogue
            .list()
            .filter(|s| after.is_none_or(|id| s.metadata.id > *id));
        let items: Vec<_> = entries
            .by_ref()
            .take(100)
            .map(|s| {
                let mut status = s.status.clone();
                if let Some(live) = state.live.get(&s.metadata.id) {
                    live.project(&mut status, now);
                }
                seedbox_api::SimulationSummary {
                    metadata: s.metadata.clone(),
                    status,
                    oracle: s.spec.configuration.check.service.clone(),
                }
            })
            .collect();
        let next_cursor = entries
            .next()
            .and_then(|_| items.last().map(|s| s.metadata.id.clone()));
        Ok(seedbox_api::SimulationList { items, next_cursor })
    }

    pub(crate) fn begin_delete(&self, id: &SimulationId) -> Result<Vec<DeletionTarget>, Error> {
        let targets = {
            let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
            let ids = state.catalogue.begin_delete(id)?;
            let mut targets = Vec::with_capacity(ids.len());
            for id in ids {
                let worker = if let Some(live) = state.live.get(&id) {
                    live.publish(state.catalogue.get(&id)?);
                    live.worker.clone()
                } else {
                    None
                };
                targets.push(DeletionTarget { id, worker });
            }
            targets
        };
        for target in &targets {
            match self.stop(&target.id) {
                Err(Error::NotFound(_)) => {}
                result => {
                    result?;
                }
            }
        }
        Ok(targets)
    }

    pub(crate) fn delete(&self, id: &SimulationId) -> Result<(), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state.catalogue.delete(id)?;
        state.live.remove(id);
        state.retained_logs.remove(id);
        state.worlds.remove(id);
        state.imported_worlds.remove(id);
        state.results.remove(id);
        Ok(())
    }

    /// Retain a completed execution's resolved semantics and result before
    /// publishing completion. The execution owner supplies the sealed projection;
    /// comparison never rediscovers evidence through a worker's attempt paths.
    pub fn retain_result(
        &self,
        id: &SimulationId,
        definition: Box<seedbox_spec::definition::Definition>,
        record: seedbox_artifacts::record::RunRecord,
    ) -> Result<(), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        if resource.status.phase == Phase::Completed || state.results.contains_key(id) {
            return Err(Error::Invalid(
                "completed result evidence is immutable".into(),
            ));
        }
        state
            .results
            .insert(id.clone(), ResultEvidence { definition, record });
        Ok(())
    }

    pub(crate) fn result_evidence(
        &self,
        id: &SimulationId,
    ) -> Result<Option<ResultEvidence>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        if resource.status.phase != Phase::Completed {
            return Err(Error::Invalid(format!("simulation {id} is not complete")));
        }
        Ok(state.results.get(id).cloned())
    }

    pub(crate) fn import_bundle(
        &self,
        snapshot: &seedbox_artifacts::archive::Snapshot,
        logs: Vec<seedbox_api::LogDelivery>,
        machine_logs: BTreeMap<String, Vec<seedbox_api::LogDelivery>>,
        events: Vec<seedbox_api::EventDelivery>,
        prepare: impl FnOnce(&Simulation) -> Result<(), Error>,
    ) -> Result<Simulation, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        if !state.accepting {
            return Err(Error::Unavailable);
        }
        let history = state.log_history.clone();
        let event_history = state.event_history.clone();
        let mut restored = None;
        let resource = state.catalogue.import(snapshot, |resource| {
            let live = Live::new(resource, &history, &event_history);
            for delivery in logs {
                match delivery {
                    seedbox_api::LogDelivery::Entry { sequence, entry } => {
                        live.all_logs
                            .restore(sequence, Some(entry))
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                    seedbox_api::LogDelivery::Gap { last, .. } => {
                        live.all_logs
                            .restore(last, None)
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                    seedbox_api::LogDelivery::Closed => {}
                }
            }
            for (name, stream) in &live.logs {
                if let Some(deliveries) = machine_logs.get(name) {
                    for delivery in deliveries {
                        match delivery {
                            seedbox_api::LogDelivery::Entry { sequence, entry } => {
                                stream.restore(*sequence, Some(entry.clone()))
                            }
                            seedbox_api::LogDelivery::Gap { last, .. } => {
                                stream.restore(*last, None)
                            }
                            seedbox_api::LogDelivery::Closed => Ok(()),
                        }
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                } else {
                    stream
                        .publish(LogEntry::SourceUnavailable)
                        .map_err(|e| Error::Invalid(e.to_string()))?;
                }
            }
            for delivery in events {
                match delivery {
                    seedbox_api::EventDelivery::Event {
                        sequence,
                        mut observation,
                    } => {
                        observation.simulation = resource.metadata.id.clone();
                        live.events
                            .restore(sequence, Some(observation))
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                    seedbox_api::EventDelivery::Gap { last, .. } => {
                        live.events
                            .restore(last, None)
                            .map_err(|e| Error::Invalid(e.to_string()))?;
                    }
                    seedbox_api::EventDelivery::Closed => {}
                }
            }
            live.all_logs
                .close()
                .map_err(|e| Error::Invalid(e.to_string()))?;
            live.events
                .close()
                .map_err(|e| Error::Invalid(e.to_string()))?;
            for stream in live.logs.values() {
                stream.close().map_err(|e| Error::Invalid(e.to_string()))?;
            }
            prepare(resource)?;
            restored = Some(RetainedLogs {
                machines: live.logs,
                all: live.all_logs,
                events: live.events,
            });
            Ok(())
        })?;
        if let (seedbox_api::InputSource::Resolved { definition }, Some(record)) =
            (&resource.spec.inputs, &snapshot.record)
        {
            state.results.insert(
                resource.metadata.id.clone(),
                ResultEvidence {
                    definition: definition.clone(),
                    record: record.clone(),
                },
            );
        }
        let mut world = snapshot.world.clone();
        world.simulation = resource.metadata.id.clone();
        state
            .imported_worlds
            .insert(resource.metadata.id.clone(), world);
        state.retained_logs.insert(
            resource.metadata.id.clone(),
            restored.expect("prepared before publication"),
        );
        Ok(resource)
    }

    pub fn submit(&self, request: SubmitRequest) -> Result<(Simulation, bool), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        if !state.accepting {
            return Err(Error::Unavailable);
        }
        let (resource, created) = state.catalogue.submit(request)?;
        if created {
            let live = Live::new(&resource, &state.log_history, &state.event_history);
            state.live.insert(resource.metadata.id.clone(), live);
        }
        Ok((state.project(resource, Instant::now()), created))
    }

    pub fn submit_seeds(
        &self,
        request: seedbox_api::SeedRangeRequest,
    ) -> Result<(Simulation, bool), Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        if !state.accepting {
            return Err(Error::Unavailable);
        }
        let (resource, created) = state.catalogue.submit_seeds(request)?;
        if created {
            let live = Live::new(&resource, &state.log_history, &state.event_history);
            state.live.insert(resource.metadata.id.clone(), live);
        }
        Ok((resource, created))
    }

    pub fn advance_seeds(&self) -> Result<Option<SimulationId>, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        if !state.accepting {
            return Ok(None);
        }
        let Some((resource, created)) = state.catalogue.advance_seeds()? else {
            return Ok(None);
        };
        if created {
            let live = Live::new(&resource, &state.log_history, &state.event_history);
            state.live.insert(resource.metadata.id.clone(), live);
        }
        Ok((resource.status.phase == Phase::Queued).then_some(resource.metadata.id))
    }

    pub fn stop_seeds(&self, id: &SimulationId) -> Result<seedbox_api::SeedRangeStatus, Error> {
        let ids = self
            .0
            .lock()
            .map_err(|_| Error::StorageFailed)?
            .catalogue
            .stop_seeds(id)?;
        for member in ids {
            self.stop(&member)?;
        }
        self.read(|catalogue| catalogue.seed_range(id))
    }

    pub fn begin(
        &self,
        id: &SimulationId,
    ) -> Result<Option<(Simulation, CancellationToken)>, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let mut resource = match state.catalogue.get(id) {
            Ok(resource) => resource.clone(),
            Err(Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if resource.status.phase != Phase::Queued {
            return Ok(None);
        }
        resource.status.phase = Phase::Preparing;
        resource.status.stage = Some(seedbox_api::Stage::Preparing);
        let resource = state.persist(resource)?;
        let live = state.live.get(id).ok_or(Error::StorageFailed)?;
        Ok(Some((resource, live.cancellation.clone())))
    }

    /// Snapshot and subscription share the same lock as every publication.
    /// Intermediate revisions can be coalesced; the newest state cannot be lost.
    pub fn watch(&self, id: &SimulationId) -> Result<watch::Receiver<Simulation>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        match state.live.get(id) {
            Some(live) => Ok(live.status.subscribe()),
            None => Ok(watch::channel(state.catalogue.get(id)?.clone()).1),
        }
    }

    pub fn cancellation(&self, id: &SimulationId) -> Result<CancellationToken, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state
            .live
            .get(id)
            .map(|l| l.cancellation.clone())
            .ok_or_else(|| Error::NotFound(id.clone()))
    }

    pub(crate) fn observation_history_available(&self, id: &SimulationId) -> Result<bool, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state.catalogue.get(id)?;
        Ok(state.live.contains_key(id) || state.retained_logs.contains_key(id))
    }

    /// Live events after retained history; without `follow`, only the history.
    pub fn events(
        &self,
        id: &SimulationId,
        follow: bool,
    ) -> Result<Subscription<SimulationEvent>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state.catalogue.get(id)?;
        match state
            .live
            .get(id)
            .map(|live| &live.events)
            .or_else(|| state.retained_logs.get(id).map(|logs| &logs.events))
        {
            Some(events) => open(events, follow),
            None => Ok(Subscription::closed()),
        }
    }

    pub fn logs(
        &self,
        id: &SimulationId,
        machine: &str,
        follow: bool,
    ) -> Result<Subscription<LogEntry>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        if !resource.spec.configuration.services.contains_key(machine) {
            return Err(Error::Invalid(format!("unknown machine {machine}")));
        }
        match state
            .live
            .get(id)
            .and_then(|live| live.logs.get(machine))
            .or_else(|| {
                state
                    .retained_logs
                    .get(id)
                    .and_then(|logs| logs.machines.get(machine))
            }) {
            Some(logs) => open(logs, follow),
            None => Ok(Subscription::closed()),
        }
    }

    /// All machines share one delivery sequence in committed publication order.
    pub fn all_logs(
        &self,
        id: &SimulationId,
        follow: bool,
    ) -> Result<Subscription<LogEntry>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state.catalogue.get(id)?;
        match state
            .live
            .get(id)
            .map(|live| &live.all_logs)
            .or_else(|| state.retained_logs.get(id).map(|logs| &logs.all))
        {
            Some(logs) => open(logs, follow),
            None => Ok(Subscription::closed()),
        }
    }

    pub(crate) fn publish_engine_event(&self, id: &SimulationId, time: u64, event: EventKind) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if let EventKind::MachineExited { machine, code, .. } = &event {
            let Ok(resource) = state.catalogue.get(id) else {
                return;
            };
            if state.live.contains_key(id)
                && resource.spec.configuration.check.service == *machine
                && resource.spec.configuration.services.contains_key(machine)
                && resource.status.oracle_exit_code.is_none()
            {
                // This is a committed engine observation, independent of the
                // final report. Persist it before any later report/cleanup error.
                let mut resource = resource.clone();
                resource.status.oracle_exit_code = Some(*code);
                resource.status.simulated_time_ns = resource.status.simulated_time_ns.max(time);
                if state.persist(resource).is_err() {
                    if let Some(live) = state.live.get(id) {
                        live.cancellation.cancel();
                        let _ = live.events.close();
                    }
                    return;
                }
            }
        }
        let Some(live) = state.live.get(id) else {
            return;
        };
        let configuration = &state
            .catalogue
            .get(id)
            .map(|r| r.spec.configuration.clone());
        let event = match &event {
            EventKind::FaultChanged {
                machine: Some(machine),
                ..
            }
            | EventKind::MachineReady { machine, .. }
            | EventKind::MachineLifecycle { machine, .. }
            | EventKind::MachineExited { machine, .. }
                if !live.logs.contains_key(machine) =>
            {
                EventKind::SourceUnavailable
            }
            _ => event,
        };
        let revision = live.status.borrow().metadata.revision;
        if let Ok(configuration) = configuration {
            state
                .worlds
                .entry(id.clone())
                .or_insert_with(|| crate::world::Tracker::new(id.clone(), configuration))
                .event(time, &event);
        }
        let Some(live) = state.live.get(id) else {
            return;
        };
        let observation = SimulationEvent {
            simulation: id.clone(),
            revision,
            simulated_time_ns: (!matches!(event, EventKind::PreparationProgress { .. }))
                .then_some(time),
            position: None,
            event,
        };
        live.publish_event(observation);
    }

    pub(crate) fn publish_frames(
        &self,
        id: &SimulationId,
        time: u64,
        frames: Vec<seedbox_api::Frame>,
    ) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        if !state.live.contains_key(id) {
            return;
        }
        let Ok(configuration) = state
            .catalogue
            .get(id)
            .map(|r| r.spec.configuration.clone())
        else {
            return;
        };
        state
            .worlds
            .entry(id.clone())
            .or_insert_with(|| crate::world::Tracker::new(id.clone(), &configuration))
            .frames(time, &frames);
        if let Some(live) = state.live.get(id) {
            let _ = live.frames.publish(FrameBatch {
                simulated_time_ns: time,
                frames,
            });
        }
    }

    /// Fault forms and target choices from one consistent catalogue observation.
    pub fn fault_types(&self, id: &SimulationId) -> Result<seedbox_api::FaultCatalogue, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        let world = match state.worlds.get(id) {
            Some(world) => world.snapshot(),
            None => crate::world::Tracker::new(id.clone(), &resource.spec.configuration).snapshot(),
        };
        Ok(crate::fault_catalogue::discover(
            &world,
            resource.spec.configuration.network.latency_ns,
        ))
    }

    /// Explain a proposed action from one observation without worker admission.
    pub fn preview_fault(
        &self,
        id: &SimulationId,
        action: &seedbox_api::FaultAction,
    ) -> Result<seedbox_api::FaultPreview, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        let world = match state.worlds.get(id) {
            Some(world) => world.snapshot(),
            None => crate::world::Tracker::new(id.clone(), &resource.spec.configuration).snapshot(),
        };
        crate::fault_preview::preview(&world, &resource.spec.configuration, action)
    }

    /// The observed world, or the configured one before any observation.
    pub fn world(&self, id: &SimulationId) -> Result<seedbox_api::WorldState, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let resource = state.catalogue.get(id)?;
        if let Some(world) = state.imported_worlds.get(id) {
            return Ok(world.clone());
        }
        Ok(match state.worlds.get(id) {
            Some(world) => world.snapshot(),
            None => crate::world::Tracker::new(id.clone(), &resource.spec.configuration).snapshot(),
        })
    }

    pub fn frames(&self, id: &SimulationId) -> Result<Subscription<FrameBatch>, Error> {
        let state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        state.catalogue.get(id)?;
        match state.live.get(id) {
            Some(live) => live.frames.subscribe().map_err(|_| Error::StorageFailed),
            None => Ok(Subscription::closed()),
        }
    }

    pub(crate) fn publish_log(&self, id: &SimulationId, entry: LogEntry) {
        let Ok(mut state) = self.0.lock() else {
            return;
        };
        let Some(live) = state.live.get(id) else {
            return;
        };
        // Validate attribution before publishing to any subscriber.
        let entry = match &entry {
            LogEntry::Bytes { machine, .. } | LogEntry::MachineGap { machine, .. }
                if !live.logs.contains_key(machine) =>
            {
                LogEntry::SourceUnavailable
            }
            _ => entry,
        };
        if let Ok(configuration) = state
            .catalogue
            .get(id)
            .map(|r| r.spec.configuration.clone())
        {
            state
                .worlds
                .entry(id.clone())
                .or_insert_with(|| crate::world::Tracker::new(id.clone(), &configuration))
                .log(&entry);
        }
        let Some(live) = state.live.get(id) else {
            return;
        };
        if live.all_logs.publish(entry.clone()).is_err() {
            let _ = live.all_logs.publish(LogEntry::SourceUnavailable);
        }
        match &entry {
            LogEntry::Bytes { machine, .. } | LogEntry::MachineGap { machine, .. } => {
                if let Some(stream) = live.logs.get(machine) {
                    if stream.publish(entry).is_err() {
                        let _ = stream.publish(LogEntry::SourceUnavailable);
                    }
                }
            }
            _ => {
                for stream in live.logs.values() {
                    let _ = stream.publish(entry.clone());
                }
            }
        }
    }

    pub fn update(
        &self,
        id: &SimulationId,
        change: impl FnOnce(&mut Simulation),
    ) -> Result<Simulation, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let mut resource = state.catalogue.get(id)?.clone();
        change(&mut resource);
        state.persist(resource)
    }

    /// Stop is naturally idempotent. Completion can win the race, in which case
    /// the already final resource is returned and its verdict stays untouched.
    pub fn stop(&self, id: &SimulationId) -> Result<Simulation, Error> {
        let mut state = self.0.lock().map_err(|_| Error::StorageFailed)?;
        let mut resource = state.catalogue.get(id)?.clone();
        if resource.status.phase == Phase::Completed {
            return Ok(resource);
        }
        resource.status.stop_requested = true;
        if resource.status.phase == Phase::Queued {
            resource.status.phase = Phase::Completed;
            resource.status.termination = Some(ExecutionTermination::Stopped);
        }
        let resource = state.persist(resource)?;
        if let Some(live) = state.live.get(id) {
            live.cancellation.cancel();
        }
        Ok(resource)
    }

    pub fn cancel_active(&self) -> Result<(), Error> {
        let ids = {
            // Cleanup must still cancel workers after a panic poisoned state.
            let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
            state.accepting = false;
            for live in state.live.values() {
                live.cancellation.cancel();
            }
            state.live.keys().cloned().collect::<Vec<_>>()
        };
        for id in ids {
            self.stop(&id)?;
        }
        Ok(())
    }
}

/// Subscribe to history and live records, or to retained history alone.
fn open<T: crate::stream::Payload>(
    stream: &Stream<T>,
    follow: bool,
) -> Result<Subscription<T>, Error> {
    if follow {
        stream.subscribe()
    } else {
        stream.snapshot()
    }
    .map_err(|_| Error::StorageFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{
        BoundaryPosition, InputSource, PlaybackRate, PlaybackStatus, SimulationSpec, Stage,
        SubmitRequest,
    };

    #[test]
    fn log_tracking_keeps_configured_attribution_and_survives_completion() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Manager::new(Catalogue::open(&directory.path().join("catalogue")).unwrap());
        let configuration = seedbox_application::config::parse(
            "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let id = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "world-logs".into(),
                spec: SimulationSpec {
                    program: Vec::new(),
                    start_paused: false,
                    seed: Some(1),
                    inputs: InputSource::Project,
                    configuration,
                },
            })
            .unwrap()
            .0
            .metadata
            .id;
        manager.begin(&id).unwrap().unwrap();
        for machine in ["oracle", "unknown"] {
            manager.publish_log(
                &id,
                LogEntry::MachineGap {
                    machine: machine.into(),
                    incarnation: 1,
                    simulated_time_ns: 42,
                    records: 1,
                },
            );
        }
        manager.publish_engine_event(
            &id,
            43,
            EventKind::MachineReady {
                machine: "unknown".into(),
                incarnation: 1,
            },
        );
        let world = manager.world(&id).unwrap();
        assert_eq!(world.machines.len(), 1);
        assert_eq!(world.machines[0].incarnation, 1);
        assert_eq!(world.simulated_time_ns, 43);
        manager
            .update(&id, |s| s.status.phase = Phase::Completed)
            .unwrap();
        assert_eq!(manager.world(&id).unwrap(), world);
        manager.delete(&id).unwrap();
        assert!(manager.world(&id).is_err());
    }

    #[tokio::test]
    async fn oversized_observations_keep_event_stream_and_durable_receipts_available() {
        use crate::stream::Delivery;
        let directory = tempfile::tempdir().unwrap();
        let manager = Manager::new(Catalogue::open(&directory.path().join("catalogue")).unwrap());
        let configuration = seedbox_application::config::parse(
            "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let id = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "oversized-events".into(),
                spec: SimulationSpec {
                    program: Vec::new(),
                    start_paused: false,
                    seed: Some(1),
                    inputs: InputSource::Project,
                    configuration,
                },
            })
            .unwrap()
            .0
            .metadata
            .id;
        manager.begin(&id).unwrap().unwrap();
        manager
            .update(&id, |s| {
                s.status.phase = Phase::Running;
            })
            .unwrap();
        let mut events = manager.events(&id, true).unwrap();
        // Skip the retained history so this reader sees only what follows.
        let backlog = {
            let mut history = manager.events(&id, false).unwrap();
            let mut count = 0;
            while let Delivery::Record(_) = history.recv().await {
                count += 1;
            }
            count
        };
        for _ in 0..backlog {
            events.recv().await;
        }
        let request = ControlRequest {
            request_id: "step".into(),
            command: seedbox_api::ControlCommand::Step,
        };
        manager
            .0
            .lock()
            .unwrap()
            .catalogue
            .accept_control(&id, request)
            .unwrap();
        let receipt = ControlState::Acknowledged {
            status: PlaybackStatus {
                position: BoundaryPosition {
                    boundary_index: 1,
                    completed_rounds: 1,
                    simulated_time_ns: 10,
                },
                rate: PlaybackRate::Paused,
                achieved_rate: None,
                stepping: true,
                before_next_fault: false,
                terminal: false,
                application_ready: true,
            },
            error: None,
            intervention: None,
            checkpoint: None,
            detail: Some("x".repeat(9000)),
        };
        manager
            .acknowledge_control(&id, "step", receipt.clone())
            .unwrap();
        assert_eq!(
            manager
                .read(|c| Ok(c.control_receipt(&id, "step")?.unwrap().state.clone()))
                .unwrap(),
            receipt
        );
        let Delivery::Record(record) = events.recv().await else {
            panic!("oversized control closed stream")
        };
        assert_eq!(record.payload.event, EventKind::SourceUnavailable);
        manager.publish_engine_event(
            &id,
            10,
            EventKind::ExecutionTerminated {
                termination: ExecutionTermination::Error {
                    message: "x".repeat(9000),
                },
            },
        );
        let Delivery::Record(record) = events.recv().await else {
            panic!("oversized engine event closed stream")
        };
        assert_eq!(record.payload.event, EventKind::SourceUnavailable);
        manager
            .update(&id, |s| {
                s.status.termination = Some(ExecutionTermination::Error {
                    message: "x".repeat(9000),
                });
            })
            .unwrap();
        let Delivery::Record(record) = events.recv().await else {
            panic!("oversized lifecycle event closed stream")
        };
        assert_eq!(record.payload.event, EventKind::SourceUnavailable);
        let mut newcomer = manager.events(&id, true).unwrap();
        let ready = EventKind::MachineReady {
            machine: "oracle".into(),
            incarnation: 0,
        };
        manager.publish_engine_event(&id, 11, ready.clone());
        let Delivery::Record(record) = events.recv().await else {
            panic!("later event unavailable")
        };
        assert_eq!(record.payload.event, ready);
        // A late subscriber replays retained history before the live event.
        let mut replayed = Vec::new();
        loop {
            let Delivery::Record(record) = newcomer.recv().await else {
                panic!("later event unavailable")
            };
            if record.payload.event == ready {
                break;
            }
            replayed.push(record.payload.event.clone());
        }
        assert!(replayed.contains(&EventKind::SourceUnavailable));
        // Without following, the history ends instead of waiting for more.
        let mut history = manager.events(&id, false).unwrap();
        let mut count = 0;
        while let Delivery::Record(_) = history.recv().await {
            count += 1;
        }
        assert_eq!(count, replayed.len() + 1);
        assert!(!manager.cancellation(&id).unwrap().is_cancelled());
    }

    #[test]
    fn committed_oracle_exit_survives_missing_final_report_and_catalogue_restart() {
        for code in [0, 7] {
            let directory = tempfile::tempdir().unwrap();
            let catalogue = directory.path().join("catalogue");
            let manager = Manager::new(Catalogue::open(&catalogue).unwrap());
            let configuration = seedbox_application::config::parse(
                "name: test\nmachines:\n  other:\n    image: example/other\noracle:\n  image: example/oracle\n",
                std::path::Path::new("/project/seedbox.yaml"),
                &Default::default(),
            )
            .unwrap();
            let resource = manager
                .submit(SubmitRequest {
                    metadata: Default::default(),
                    options: Default::default(),
                    request_id: "oracle-observation".into(),
                    spec: SimulationSpec {
                        program: Vec::new(),
                        start_paused: false,
                        seed: Some(1),
                        inputs: InputSource::Project,
                        configuration,
                    },
                })
                .unwrap()
                .0;
            let id = resource.metadata.id;
            manager.begin(&id).unwrap().unwrap();
            manager
                .update(&id, |resource| {
                    resource.status.phase = Phase::Running;
                    resource.status.stage = Some(Stage::Executing);
                })
                .unwrap();
            manager.publish_engine_event(
                &id,
                10,
                EventKind::MachineExited {
                    machine: "other".into(),
                    incarnation: 0,
                    code: 99,
                },
            );
            assert_eq!(manager.describe(&id).unwrap().status.oracle_exit_code, None);
            manager.publish_engine_event(
                &id,
                20,
                EventKind::MachineExited {
                    machine: "oracle".into(),
                    incarnation: 0,
                    code,
                },
            );
            assert_eq!(
                manager.describe(&id).unwrap().status.oracle_exit_code,
                Some(code)
            );
            // Match execution's error finalization when no report could be parsed.
            manager
                .update(&id, |resource| {
                    resource.status.phase = Phase::Completed;
                    resource.status.stage = None;
                    resource.status.termination = Some(ExecutionTermination::Error {
                        message: "malformed worker report".into(),
                    });
                })
                .unwrap();
            let completed = manager.describe(&id).unwrap();
            assert_eq!(
                completed.status.verdict(),
                if code == 0 {
                    seedbox_api::TestVerdict::Passed
                } else {
                    seedbox_api::TestVerdict::Failed
                }
            );
            assert!(matches!(
                completed.status.termination,
                Some(ExecutionTermination::Error { .. })
            ));
            drop(manager);
            let reopened = Manager::new(Catalogue::open(&catalogue).unwrap());
            assert_eq!(reopened.describe(&id).unwrap(), completed);
        }
    }

    #[test]
    fn committed_readiness_closes_boot_stage_once_and_ignores_stale_samples() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Manager::new(Catalogue::open(&directory.path().join("catalogue")).unwrap());
        let configuration = seedbox_application::config::parse(
            "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let resource = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "readiness".into(),
                spec: SimulationSpec {
                    program: Vec::new(),
                    start_paused: false,
                    seed: Some(1),
                    inputs: InputSource::Project,
                    configuration,
                },
            })
            .unwrap()
            .0;
        let id = &resource.metadata.id;
        manager.begin(id).unwrap().unwrap();
        manager
            .update(id, |resource| {
                resource.status.phase = Phase::Running;
                resource.status.stage = Some(Stage::Booting);
            })
            .unwrap();
        let sample = |round, ready| PlaybackStatus {
            position: BoundaryPosition {
                boundary_index: round,
                completed_rounds: round,
                simulated_time_ns: round * 10,
            },
            rate: PlaybackRate::Unlimited,
            achieved_rate: None,
            stepping: false,
            before_next_fault: false,
            terminal: false,
            application_ready: ready,
        };
        manager.observe_playback(id, sample(2, false)).unwrap();
        manager.observe_playback(id, sample(1, true)).unwrap();
        assert_eq!(
            manager.describe(id).unwrap().status.stage,
            Some(Stage::Booting)
        );
        manager.observe_playback(id, sample(3, true)).unwrap();
        let executing = manager.describe(id).unwrap();
        assert_eq!(executing.status.stage, Some(Stage::Executing));
        assert!(executing.status.stage_timings[&Stage::Booting].complete);
        let mut transition = sample(4, true);
        transition.position.completed_rounds = 3;
        transition.position.simulated_time_ns = 29;
        manager.observe_playback(id, transition.clone()).unwrap();
        {
            let state = manager.0.lock().unwrap();
            assert_eq!(state.live[id].playback.as_ref().unwrap().0, transition);
        }
        manager.observe_playback(id, sample(3, false)).unwrap();
        {
            let state = manager.0.lock().unwrap();
            assert_eq!(state.live[id].playback.as_ref().unwrap().0, transition);
        }
        manager.observe_playback(id, sample(4, true)).unwrap();
        assert_eq!(
            manager.describe(id).unwrap().metadata.revision,
            executing.metadata.revision
        );
    }
}
