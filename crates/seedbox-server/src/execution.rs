//! Server-owned application preparation and execution. Platform discovery is
//! injected by the executable; clients never own worker lifetimes or leases.

mod bundles;
mod campaigns;
mod diagnostics;

use crate::{catalogue, manager::Manager};
use seedbox_api::{ExecutionTermination, InputSource, Phase, SimulationId, Stage};
use seedbox_application::{
    config::Simulation,
    execution::{
        CancellationToken, ExecutionBackend, PreparationBackend, PreparedInputs, StopReason,
    },
    orchestration::{Orchestrator, Run, RunOptions, RunProgress},
    preparation::PreparationContext,
    store::{ContentDigest, ContentStore},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinSet};

/// Prepared inputs survive failed preparation/execution. The attempt pointer
/// is persisted before its worker starts, so recovery can find durable actions.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RetainedInputs {
    #[serde(default)]
    checkpoint: Option<PathBuf>,
    prepared: PreparedInputs,
    interventions: Option<ContentDigest>,
    attempt: Option<PathBuf>,
}

pub struct Backends {
    pub preparation: Box<dyn PreparationBackend>,
    pub execution: Box<dyn ExecutionBackend>,
}

pub trait BackendProvider: Send + Sync + 'static {
    /// Resolve project backends cooperatively with stop and preparation-deadline cancellation.
    fn project(
        &self,
        configuration: &Simulation,
        store: &ContentStore,
        cancellation: &CancellationToken,
    ) -> seedbox_application::Result<Backends>;
    fn retained(
        &self,
        inputs: &PreparedInputs,
        store: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>>;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Catalogue(#[from] catalogue::Error),
    #[error("simulation storage: {0}")]
    Storage(#[from] std::io::Error),
    #[error("simulation evidence: {0}")]
    Evidence(#[from] seedbox_application::Error),
    #[error("simulation bundle: {0}")]
    Bundle(String),
    #[error("simulation worker task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub struct ExecutionService {
    manager: Arc<Manager>,
    root: PathBuf,
    store: ContentStore,
    backends: Arc<dyn BackendProvider>,
    preparation_limit: Duration,
    evidence: Mutex<()>,
    admission: Mutex<(usize, BTreeMap<tokio::task::Id, SimulationId>)>,
    jobs: Mutex<campaigns::Jobs>,
    findings_root: Option<PathBuf>,
}

impl ExecutionService {
    pub fn new(
        manager: Arc<Manager>,
        root: PathBuf,
        store: ContentStore,
        backends: Arc<dyn BackendProvider>,
    ) -> Result<Self, Error> {
        fs::create_dir_all(root.join("inputs"))?;
        fs::create_dir_all(root.join("runs"))?;
        Ok(Self {
            manager,
            root,
            store,
            backends,
            preparation_limit: crate::preparation_budget::DEFAULT_LIMIT,
            evidence: Mutex::new(()),
            jobs: Mutex::default(),
            findings_root: None,
            admission: Mutex::new((0, BTreeMap::new())),
        })
    }

    /// Reservations cover preparation, paused RAM and final worker cleanup.
    pub fn capacity(&self) -> Result<seedbox_api::ExecutionCapacity, Error> {
        let admission = self
            .admission
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let occupying: BTreeSet<_> = admission.1.values().cloned().collect();
        Ok(self.manager.capacity(admission.0, &occupying)?)
    }

    /// Foreground cascading deletion: stop the frozen ownership tree, wait for
    /// execution cleanup, then remove dependents before their parents.
    pub fn delete(&self, id: &SimulationId) -> Result<(), Error> {
        let _evidence = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let targets = self.manager.begin_delete(id)?;
        // Join control transport and journal I/O without holding the catalogue
        // mutex. Retirement prevents already cloned handles from starting later.
        for target in &targets {
            if let Some(worker) = &target.worker {
                worker
                    .lock()
                    .map_err(|_| Error::Catalogue(catalogue::Error::StorageFailed))?
                    .retire();
            }
        }
        for target in targets {
            let id = target.id;
            loop {
                match self.manager.describe(&id) {
                    Ok(resource) if resource.status.phase != Phase::Completed => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(_) | Err(catalogue::Error::NotFound(_)) => break,
                    Err(error) => return Err(error.into()),
                }
            }
            // Paths are derived from validated IDs, never from retained metadata.
            let directory = self.root.join("runs").join(id.as_str());
            match fs::remove_dir_all(directory) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                result => result?,
            }
            match fs::remove_file(self.root.join("inputs").join(format!("{id}.json"))) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                result => result?,
            }
            self.manager.delete(&id)?;
        }
        Ok(())
    }

    /// Block new branches before removing the completed simulation's capture.
    /// Repeated deletion retries cleanup using the retained attempt reference.
    pub fn delete_checkpoint(&self, id: &SimulationId, name: &str) -> Result<(), Error> {
        let _evidence = self
            .evidence
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        self.manager.delete_checkpoint(id, name)?;
        if self.manager.read(|catalogue| catalogue.is_imported(id))? {
            return Ok(());
        }
        let inputs: RetainedInputs = serde_json::from_reader(fs::File::open(
            self.root.join("inputs").join(format!("{id}.json")),
        )?)
        .map_err(std::io::Error::other)?;
        let attempt = inputs
            .attempt
            .ok_or_else(|| std::io::Error::other("checkpoint source has no execution attempt"))?;
        crate::checkpoint_cleanup::remove(&attempt, name).map_err(|e| {
            Error::Storage(std::io::Error::new(
                e.kind(),
                format!("checkpoint {name} cleanup: {e}"),
            ))
        })
    }

    /// Snapshot the atomically published semantic journal using owned paths.
    /// Reading never acquires the writer lock or mutates the source simulation.
    pub fn timeline(&self, id: &SimulationId) -> Result<seedbox_api::FaultTimeline, Error> {
        let (resource, history) = self.recorded_snapshot(id)?;
        let world = self.manager.world(id)?;
        Ok(crate::timeline::project(&resource, &history, &world))
    }

    /// A completed simulation's failure timeline from its retained evidence.
    /// `events` is its retained event history, less `gaps` lost records.
    pub fn explanation(
        &self,
        id: &SimulationId,
        events: &[seedbox_api::SimulationEvent],
        gaps: u64,
    ) -> Result<seedbox_api::Explanation, Error> {
        let resource = self.describe(id)?;
        if resource.status.phase != Phase::Completed {
            return Err(Error::Catalogue(catalogue::Error::Invalid(format!(
                "simulation {id} is not complete; explain a completed simulation"
            ))));
        }
        let world = self.manager.world(id)?;
        let record = self.run_record(id)?;
        Ok(crate::explain::project(
            id,
            &resource.spec.program,
            &world,
            events,
            gaps,
            record.as_ref().and_then(|r| r.report.as_ref()),
        ))
    }

    /// Return the immutable submitted inputs enriched with recorded external overrides.
    /// This is an export projection; the catalogue's admitted definition stays immutable.
    pub fn describe(&self, id: &SimulationId) -> Result<seedbox_api::Simulation, Error> {
        self.recorded_snapshot(id).map(|(resource, _)| resource)
    }

    fn recorded_snapshot(
        &self,
        id: &SimulationId,
    ) -> Result<
        (
            seedbox_api::Simulation,
            seedbox_engine_protocol::intervention::History,
        ),
        Error,
    > {
        let mut resource = self.manager.describe(id)?;
        let path = self.root.join("inputs").join(format!("{id}.json"));
        let history = match fs::File::open(&path) {
            Ok(file) => {
                let retained: RetainedInputs =
                    serde_json::from_reader(file).map_err(seedbox_application::Error::from)?;
                if let Some(attempt) = retained.attempt {
                    // Published attempts always have an explicit journal; missing
                    // input here is an error, never an empty timeline.
                    seedbox_artifacts::interventions::read(&attempt.join("interventions.json"))?
                } else if let Some(digest) = retained.interventions {
                    seedbox_engine_protocol::intervention::History::decode(
                        &self.store.read(&digest)?,
                    )
                    .map_err(seedbox_application::Error::Integrity)?
                } else {
                    seedbox_engine_protocol::intervention::History::default()
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                seedbox_engine_protocol::intervention::History::default()
            }
            Err(error) => return Err(error.into()),
        };
        resource.spec.program = crate::timeline::merge_external(&resource.spec.program, &history)?;
        Ok((resource, history))
    }

    /// Resolve an edited recording on the server. Clients select stable entries;
    /// the server owns retained input reuse and action scheduling semantics.
    pub fn derive_request(
        &self,
        id: &SimulationId,
        request: seedbox_api::DeriveRequest,
    ) -> Result<seedbox_api::SubmitRequest, Error> {
        let (resource, history) = self.recorded_snapshot(id)?;
        let world = self.manager.world(id)?;
        let timeline = crate::timeline::project(&resource, &history, &world);
        crate::timeline::derive(resource, timeline, request).map_err(Error::Catalogue)
    }

    /// Export checkpoint state and its binary closure independently of the source resource.
    pub fn checkpoint_definition(
        &self,
        id: &SimulationId,
        name: &str,
    ) -> Result<seedbox_api::Simulation, Error> {
        self.manager.captured_checkpoint(id, name)?;
        if let Some(definition) = self
            .manager
            .read(|catalogue| catalogue.imported_checkpoint(id, name))?
        {
            let mut resource = self.describe(id)?;
            resource.spec = seedbox_api::SimulationSpec::resolved(definition);
            return Ok(resource);
        }
        let mut resource = self.describe(id)?;
        let seedbox_api::InputSource::Resolved { definition } = &mut resource.spec.inputs else {
            return Err(Error::Evidence(seedbox_application::Error::Setup(
                "simulation has no resolved definition".into(),
            )));
        };
        let inputs: RetainedInputs = serde_json::from_reader(fs::File::open(
            self.root.join("inputs").join(format!("{id}.json")),
        )?)
        .map_err(std::io::Error::other)?;
        let attempt = inputs
            .attempt
            .ok_or_else(|| std::io::Error::other("checkpoint has no execution attempt"))?;
        let path = attempt
            .join("output")
            .join(seedbox_application::engine_adapter::MACHINE_OUTPUT_DIR)
            .join(format!("checkpoint-{name}.json"));
        definition.overrides = resource
            .spec
            .program
            .iter()
            .map(|input| seedbox_spec::definition::Override {
                id: input.key.clone(),
                at_ns: input.at_ns,
                action: input.action.clone(),
                timeout_ns: input.timeout_ns,
            })
            .collect();
        definition.start =
            seedbox_application::definition::capture_start(&path, &attempt, &self.store)?;
        let frozen = seedbox_artifacts::interventions::read(
            &attempt.join(format!("checkpoint-{name}-interventions.json")),
        )?;
        seedbox_artifacts::checkpoint::project(
            &mut definition.start,
            &definition.overrides,
            &frozen,
        )
        .map_err(|e| Error::Bundle(format!("checkpoint {name}: {e}")))?;
        resource
            .metadata
            .annotations
            .insert("derived-from".into(), id.to_string());
        Ok(resource)
    }

    /// Read final evidence using server-owned attempt references. Clients never
    /// need host run paths, and comparison never changes a simulation's verdict.
    /// A completed simulation's retained run record, if it has one.
    fn run_record(
        &self,
        id: &SimulationId,
    ) -> Result<Option<seedbox_artifacts::record::RunRecord>, Error> {
        if self.manager.read(|catalogue| catalogue.is_imported(id))? {
            return Ok(self.imported_snapshot(id)?.record);
        }
        let path = self.root.join("inputs").join(format!("{id}.json"));
        let file = match fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let inputs: RetainedInputs =
            serde_json::from_reader(file).map_err(std::io::Error::other)?;
        let Some(attempt) = inputs.attempt else {
            return Ok(None);
        };
        Ok(Some(seedbox_application::orchestration::inspect(&attempt)?))
    }

    pub fn compare(
        &self,
        baseline: &SimulationId,
        simulation: &SimulationId,
    ) -> seedbox_application::Result<seedbox_api::SimulationComparison> {
        let _evidence = self
            .evidence
            .lock()
            .map_err(|_| seedbox_application::Error::Setup("evidence lock failed".into()))?;
        let record = |id: &SimulationId| {
            let resource = self
                .manager
                .describe(id)
                .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?;
            if resource.status.phase != Phase::Completed {
                return Err(seedbox_application::Error::Setup(format!(
                    "simulation {id} is not complete"
                )));
            }
            self.run_record(id)
                .map_err(|e| seedbox_application::Error::Setup(e.to_string()))
        };
        let (Some(first), Some(second)) = (record(baseline)?, record(simulation)?) else {
            return Ok(seedbox_api::SimulationComparison {
                baseline: baseline.clone(),
                simulation: simulation.clone(),
                status: seedbox_api::ComparisonStatus::NotComparable,
                outcome_matches: false,
                witness_matches: None,
                full_state_matches: None,
                fault_history_matches: false,
            });
        };
        let comparison = seedbox_artifacts::record::compare(&first, &second);
        use seedbox_api::ComparisonStatus;
        let first_definition = self.portable_simulation(baseline)?.spec.inputs;
        let second_definition = self.portable_simulation(simulation)?.spec.inputs;
        let compatible = match (first_definition, second_definition) {
            (
                InputSource::Resolved { definition: first },
                InputSource::Resolved { definition: second },
            ) => bundles::semantic_definition(first)? == bundles::semantic_definition(second)?,
            _ => false,
        };
        let status = if !compatible {
            ComparisonStatus::NotComparable
        } else {
            match comparison.status {
                seedbox_artifacts::record::ReplayStatus::Match => ComparisonStatus::Match,
                seedbox_artifacts::record::ReplayStatus::Mismatch => ComparisonStatus::Mismatch,
                seedbox_artifacts::record::ReplayStatus::NotComparable => {
                    ComparisonStatus::NotComparable
                }
            }
        };
        Ok(seedbox_api::SimulationComparison {
            baseline: baseline.clone(),
            simulation: simulation.clone(),
            status,
            outcome_matches: comparison.outcome_matches,
            witness_matches: comparison.witness_matches,
            full_state_matches: comparison.full_state_matches,
            fault_history_matches: comparison.fault_history_matches,
        })
    }

    /// The owner waits for every active task during graceful shutdown. Dropping
    /// a CLI connection has no path to this cancellation token.
    pub async fn run(
        self: Arc<Self>,
        mut queue: mpsc::Receiver<SimulationId>,
        capacity: NonZeroUsize,
        shutdown: CancellationToken,
    ) -> Result<(), Error> {
        self.admission
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?
            .0 = capacity.get();
        let recovery = self.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            for id in recovery
                .manager
                .read(|catalogue| Ok(catalogue.pending_deletions()))?
            {
                recovery.delete(&id)?;
            }
            Ok(())
        })
        .await??;
        let mut tasks = JoinSet::new();
        let mut stopping = false;
        let mut cleanup_started = false;
        let mut failure = None;
        loop {
            stopping |= shutdown.is_cancelled();
            if stopping && !cleanup_started {
                cleanup_started = true;
                shutdown.cancel();
                queue.close();
                if let Err(error) = self.manager.cancel_active() {
                    failure.get_or_insert(Error::Catalogue(error));
                }
            }
            if stopping && tasks.is_empty() {
                return failure.map_or(Ok(()), Err);
            }
            tokio::select! {
                result = tasks.join_next_with_id(), if !tasks.is_empty() => {
                    let task_id = match &result {
                        Some(Ok((id, _))) => Some(*id),
                        Some(Err(error)) => Some(error.id()),
                        None => None,
                    };
                    if let Some(id) = task_id {
                        self.admission.lock().map_err(|_| catalogue::Error::StorageFailed)?.1.remove(&id);
                    }
                    let error = match result {
                        Some(Ok((_, Err(error)))) => Some(error),
                        Some(Err(error)) => Some(Error::Task(error)),
                        _ => None,
                    };
                    if let Some(error) = error {
                        failure.get_or_insert(error);
                        stopping = true;
                    }
                }
                id = queue.recv(), if !stopping && tasks.len() < capacity.get() => {
                    match id {
                        Some(id) => {
                            let service = self.clone();
                            let mut admission = self.admission.lock().map_err(|_| catalogue::Error::StorageFailed)?;
                            let admitted = id.clone();
                            let task = tasks.spawn_blocking(move || service.execute(id));
                            admission.1.insert(task.id(), admitted);
                        }
                        None => stopping = true,
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)), if !stopping => {}
            }
        }
    }

    fn execute(&self, id: SimulationId) -> Result<(), Error> {
        let Some((resource, cancellation)) = self.manager.begin(&id)? else {
            return Ok(());
        };
        let preparation_expired = Arc::new(AtomicBool::new(false));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.execute_inner(
                &id,
                &resource.spec,
                &cancellation,
                preparation_expired.clone(),
            )
        }))
        .unwrap_or_else(|_| {
            Err(seedbox_application::Error::Execution(
                "simulation worker panicked".into(),
            ))
        });
        // No completed resource may lose its final manager-owned control handle
        // while that connection still has transport or journal work in flight.
        self.manager.retire_worker(&id)?;
        let _ = self.timeline(&id)?;
        match result {
            Ok(run) => {
                let report = run.record.report.as_ref();
                // Ignore legacy record.outcome: it mixes evidence errors with
                // test outcomes and historically allowed configurable success codes.
                let exit = report
                    .and_then(|r| r.workload_exit.as_ref())
                    .filter(|e| e.service == resource.spec.configuration.check.service)
                    .map(|e| e.code);
                let termination = if let Some(message) = &run.record.error {
                    ExecutionTermination::Error {
                        message: message.clone(),
                    }
                } else {
                    match report.map(|r| &r.reason) {
                        Some(StopReason::Completed) if exit.is_some() => {
                            ExecutionTermination::OracleExited
                        }
                        Some(StopReason::Cancelled) => ExecutionTermination::Stopped,
                        Some(StopReason::WallTimeLimit) => ExecutionTermination::WallTimeLimit,
                        Some(StopReason::SimulatedTimeLimit) => {
                            ExecutionTermination::SimulatedTimeLimit
                        }
                        Some(StopReason::Quiescent) => ExecutionTermination::Quiescent,
                        _ => ExecutionTermination::Error {
                            message: run
                                .record
                                .error
                                .clone()
                                .or_else(|| report.and_then(|r| r.detail.clone()))
                                .unwrap_or_else(|| "execution ended without an oracle exit".into()),
                        },
                    }
                };
                if let Err(error) = self.register_anchor_captures(&id) {
                    eprintln!("simulation {id}: anchor captures: {error}");
                }
                let lost = report.is_none() && run.record.error.is_some();
                self.manager.update(&id, |s| {
                    merge_report_observation(
                        &mut s.status,
                        exit,
                        report.and_then(|r| r.simulated_time_ns),
                        report.and_then(|r| r.properties.as_ref()),
                    );
                    if lost {
                        // The final report, and any property evidence in it,
                        // never arrived: a passing oracle proves nothing alone.
                        s.status.properties = Some(lost_evidence());
                    }
                    s.status.termination = Some(termination);
                    s.status.phase = Phase::Completed;
                    s.status.stage = None;
                })?;
            }
            Err(error) => {
                self.manager.update(&id, |s| {
                    // An oracle that exited before the error left no final
                    // report: its property evidence is lost too.
                    if s.status.oracle_exit_code.is_some() && s.status.properties.is_none() {
                        s.status.properties = Some(lost_evidence());
                    }
                    s.status.phase = Phase::Completed;
                    s.status.stage = None;
                    s.status.termination = Some(if cancellation.is_cancelled() {
                        ExecutionTermination::Stopped
                    } else if preparation_expired.load(Ordering::Acquire) {
                        ExecutionTermination::PreparationTimeLimit
                    } else {
                        ExecutionTermination::Error {
                            message: error.to_string(),
                        }
                    });
                })?;
            }
        }
        Ok(())
    }

    fn execute_inner(
        &self,
        id: &SimulationId,
        spec: &seedbox_api::SimulationSpec,
        cancellation: &CancellationToken,
        preparation_expired: Arc<AtomicBool>,
    ) -> seedbox_application::Result<Run> {
        let mut resolved_spec = spec.clone();
        resolved_spec
            .resolve_project_architecture(if cfg!(target_arch = "aarch64") {
                seedbox_spec::definition::Architecture::Aarch64
            } else {
                seedbox_spec::definition::Architecture::X86_64
            })
            .map_err(seedbox_application::Error::Setup)?;
        let spec = &resolved_spec;
        let options = self
            .manager
            .describe(id)
            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?
            .options;
        let program = crate::catalogue::resolve_program(spec)
            .map_err(|error| seedbox_application::Error::Setup(error.to_string()))?;
        let budget = crate::preparation_budget::Budget::start(
            cancellation,
            preparation_expired,
            self.preparation_limit,
        )
        .map_err(|error| {
            seedbox_application::Error::Setup(format!("preparation deadline: {error}"))
        })?;
        let directory = self.root.join("runs").join(id.as_str());
        fs::create_dir(&directory)?;
        let (inputs, interventions, checkpoint, executor) = match &spec.inputs {
            InputSource::Resolved { definition } => {
                let inputs = seedbox_application::definition::prepared(definition, &self.store)?;
                let executor = self.backends.retained(&inputs, &self.store)?;
                let checkpoint = seedbox_application::definition::materialize_start(
                    definition,
                    &self.store,
                    &directory,
                )?;
                (inputs, None, checkpoint, executor)
            }
            InputSource::Project => {
                let backends = self.backends.project(
                    &spec.configuration,
                    &self.store,
                    &budget.cancellation,
                )?;
                backends.preparation.validate_runtime_inputs()?;
                backends.execution.preflight(None)?;
                let observer = self.manager.clone();
                let simulation = id.clone();
                let context = PreparationContext::new(
                    directory.join("preparation"),
                    budget.cancellation.clone(),
                )?
                .with_progress(move |machine, stage| {
                    use seedbox_application::preparation::PreparationStage as Source;
                    let stage = match stage {
                        Source::Image => seedbox_api::PreparationStage::Image,
                        Source::Guest => seedbox_api::PreparationStage::Guest,
                        Source::Ready => seedbox_api::PreparationStage::Ready,
                    };
                    observer.publish_engine_event(
                        &simulation,
                        0,
                        seedbox_api::EventKind::PreparationProgress {
                            machine: machine.to_owned(),
                            stage,
                        },
                    );
                });
                let inputs = backends.preparation.prepare_cancellable(
                    &spec.configuration,
                    &self.store,
                    &context,
                )?;
                (inputs, None, None, backends.execution)
            }
            InputSource::Retained { simulation }
            | InputSource::Prepared { simulation }
            | InputSource::Checkpoint { simulation, .. } => {
                let path = self.root.join("inputs").join(format!("{simulation}.json"));
                let file = fs::File::open(&path).map_err(|e| {
                    seedbox_application::Error::Setup(format!(
                        "retained inputs {}: {e}",
                        path.display()
                    ))
                })?;
                let retained: RetainedInputs = serde_json::from_reader(file)?;
                let (interventions, checkpoint) =
                    if let InputSource::Checkpoint { checkpoint, .. } = &spec.inputs {
                        let _captured = self
                            .manager
                            .captured_checkpoint(simulation, checkpoint)
                            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?;
                        let attempt = retained.attempt.as_ref().ok_or_else(|| {
                            seedbox_application::Error::Setup(
                                "checkpoint source has no execution attempt".into(),
                            )
                        })?;
                        let path = attempt
                            .join("output")
                            .join(seedbox_application::engine_adapter::MACHINE_OUTPUT_DIR)
                            .join(format!("checkpoint-{checkpoint}.json"));
                        if !path.is_file() {
                            return Err(seedbox_application::Error::Setup(format!(
                                "checkpoint {}: not found",
                                path.display()
                            )));
                        }
                        let history = seedbox_artifacts::interventions::read(
                            &attempt.join(format!("checkpoint-{checkpoint}-interventions.json")),
                        )?;
                        let digest = self.store.put_unsynced(
                            &history
                                .encode()
                                .map_err(seedbox_application::Error::Integrity)?,
                        )?;
                        (Some(digest), Some(path))
                    } else if matches!(spec.inputs, InputSource::Prepared { .. }) {
                        (None, None)
                    } else {
                        (
                            self.retained_history(&retained)?,
                            retained.checkpoint.clone(),
                        )
                    };
                let inputs = retained.prepared;
                let executor = self.backends.retained(&inputs, &self.store)?;
                (inputs, interventions, checkpoint, executor)
            }
        };
        let overrides: Vec<_> = spec
            .program
            .iter()
            .map(|p| seedbox_spec::definition::Override {
                id: p.key.clone(),
                at_ns: p.at_ns,
                action: p.action.clone(),
                timeout_ns: p.timeout_ns,
            })
            .collect();
        let seed = spec.seed.ok_or_else(|| {
            seedbox_application::Error::Integrity("unresolved submission seed".into())
        })?;
        let mut definition = if let InputSource::Resolved { definition } = &spec.inputs {
            let mut d = definition.as_ref().clone();
            d.seed = seed;
            d.overrides = overrides;
            d
        } else {
            seedbox_application::definition::resolve(&spec.configuration, &inputs, seed, overrides)?
        };
        // The first run and every resubmission use the same resolved private representation.
        // Source paths, image transport manifests and human metadata never enter experiment identity.
        let inputs = seedbox_application::definition::prepared(&definition, &self.store)?;
        // Resolving per-machine RAM chooses the oracle's RAM as the legacy
        // default. Bind execution to that canonical profile as well.
        let executor = if executor.profile() == inputs.profile {
            executor
        } else {
            self.backends.retained(&inputs, &self.store)?
        };
        definition.runtime.execution =
            seedbox_application::definition::execution_settings(&inputs.profile)?;
        for (name, machine) in &mut definition.machines {
            machine.boot_archive =
                seedbox_spec::definition::Asset(format!("sha256:{}", inputs.guests[name].hex()));
        }
        let oracle_name = definition.oracle_name().to_owned();
        definition.oracle.boot_archive = seedbox_spec::definition::Asset(format!(
            "sha256:{}",
            inputs.guests[&oracle_name].hex()
        ));
        let configuration = definition.configuration("");
        let mut retained = RetainedInputs {
            checkpoint: checkpoint.clone(),
            prepared: inputs.clone(),
            interventions: interventions.clone(),
            attempt: None,
        };
        save_inputs(&self.root.join("inputs"), id, &retained, false)?;
        if budget.finish() || cancellation.is_cancelled() {
            return Err(seedbox_application::Error::PreparationCancelled);
        }
        let engine = executor.engine().map(|engine| seedbox_api::EngineBuild {
            version: engine.simulator_version,
            revision: engine.revision,
        });
        self.manager
            .update(id, |s| {
                s.spec = seedbox_api::SimulationSpec::resolved(definition);
                s.status.phase = Phase::Running;
                s.status.stage = Some(Stage::Booting);
                s.status.engine = engine;
            })
            .map_err(|e| seedbox_application::Error::Execution(e.to_string()))?;
        let observations = crate::observations::Receiver::start(self.manager.clone(), id.clone())?;
        let mut orchestrator = Orchestrator::new(self.store.clone(), directory.join("attempts"));
        if let Some(checkpoint) = checkpoint {
            orchestrator = orchestrator.with_checkpoint(checkpoint);
        }
        orchestrator
            .with_start_paused(options.start_paused)
            .with_observations(observations.path())
            .with_interventions(interventions)
            .with_program(program)
            .run_prepared_observed(
                &configuration,
                RunOptions {
                    capture_anchors: options.capture_anchors.clone(),
                    seed: spec.seed,
                    wall_time_ns: options.wall_time_ns,
                    cancellation: cancellation.clone(),
                },
                inputs,
                executor.as_ref(),
                |progress| {
                    let report = match progress {
                        RunProgress::Starting { directory } => {
                            retained.attempt = Some(directory.to_owned());
                            save_inputs(&self.root.join("inputs"), id, &retained, true)?;
                            return self
                                .manager
                                .attach_worker(id, directory)
                                .map_err(|e| seedbox_application::Error::Execution(e.to_string()));
                        }
                        RunProgress::Finalizing { report } => report,
                    };
                    let exit = report
                        .and_then(|r| r.workload_exit.as_ref())
                        .filter(|e| e.service == spec.configuration.check.service)
                        .map(|e| e.code);
                    self.manager
                        .update(id, |s| {
                            s.status.phase = Phase::Finalizing;
                            s.status.stage = Some(Stage::Finalizing);
                            merge_report_observation(
                                &mut s.status,
                                exit,
                                report.and_then(|r| r.simulated_time_ns),
                                report.and_then(|r| r.properties.as_ref()),
                            );
                        })
                        .map_err(|e| seedbox_application::Error::Execution(e.to_string()))?;
                    Ok(())
                },
            )
    }
}

impl ExecutionService {
    /// Register each `checkpoint-anchor-<key>.json` the engine published, with
    /// the intervention history frozen at its position, so it can be branched
    /// from like an interactive capture.
    fn register_anchor_captures(&self, id: &SimulationId) -> seedbox_application::Result<()> {
        let inputs: RetainedInputs = serde_json::from_reader(fs::File::open(
            self.root.join("inputs").join(format!("{id}.json")),
        )?)?;
        let Some(attempt) = inputs.attempt else {
            return Ok(());
        };
        let output = attempt
            .join("output")
            .join(seedbox_application::engine_adapter::MACHINE_OUTPUT_DIR);
        let Ok(entries) = fs::read_dir(&output) else {
            return Ok(());
        };
        let history = seedbox_artifacts::interventions::read(&attempt.join("interventions.json"))?;
        for entry in entries {
            let file_name = entry?.file_name();
            let Some(name) = file_name
                .to_str()
                .and_then(|n| n.strip_prefix("checkpoint-"))
                .and_then(|n| n.strip_suffix(".json"))
                .filter(|n| n.starts_with("anchor-") && !n.ends_with("-interventions"))
            else {
                continue;
            };
            #[derive(serde::Deserialize)]
            struct Saved {
                state: State,
            }
            #[derive(serde::Deserialize)]
            struct State {
                position: seedbox_engine_protocol::host_control::BoundaryPosition,
            }
            let saved: Saved = serde_json::from_reader(fs::File::open(output.join(&file_name))?)?;
            let position = saved.state.position;
            let mut frozen = history.clone();
            frozen
                .entries
                .retain(|entry| entry.position.boundary_index <= position.boundary_index);
            fs::write(
                attempt.join(format!("checkpoint-{name}-interventions.json")),
                frozen
                    .encode()
                    .map_err(seedbox_application::Error::Integrity)?,
            )?;
            self.manager
                .record_anchor_checkpoint(id, name, position)
                .map_err(|e| seedbox_application::Error::Execution(e.to_string()))?;
        }
        Ok(())
    }

    fn retained_history(
        &self,
        retained: &RetainedInputs,
    ) -> seedbox_application::Result<Option<ContentDigest>> {
        let Some(attempt) = &retained.attempt else {
            return Ok(retained.interventions.clone());
        };
        let record = seedbox_application::orchestration::inspect(attempt)?;
        if let Some(digest) = record.identity.interventions {
            return Ok(Some(digest));
        }
        // An interrupted run may not have sealed its history yet. Every
        // published attempt has an explicit journal, including the empty case.
        let history = seedbox_artifacts::interventions::read(&attempt.join("interventions.json"))?;
        if history.entries.is_empty() {
            Ok(None)
        } else {
            self.store
                .put_unsynced(
                    &history
                        .encode()
                        .map_err(seedbox_application::Error::Integrity)?,
                )
                .map(Some)
        }
    }
}

fn save_inputs(
    directory: &Path,
    id: &SimulationId,
    inputs: &RetainedInputs,
    replace: bool,
) -> seedbox_application::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    file.write_all(&serde_json::to_vec(inputs)?)?;
    let path = directory.join(format!("{id}.json"));
    if replace {
        file.persist(path).map_err(|e| e.error)?;
    } else {
        file.persist_noclobber(path).map_err(|e| e.error)?;
    }
    Ok(())
}

// Committed observations may be ahead of a late final report. Neither the
// oracle's first exit nor the public timeline may move backwards during cleanup.
/// The assessment of a run whose final report was lost.
fn lost_evidence() -> seedbox_engine_protocol::property::Assessment {
    seedbox_engine_protocol::property::Assessment {
        status: seedbox_engine_protocol::property::AssessmentStatus::Incomplete,
        violated: 0,
        unmet: 0,
        evidence_complete: false,
        error_count: 1,
        errors: Vec::new(),
        properties: Vec::new(),
    }
}

fn merge_report_observation(
    status: &mut seedbox_api::SimulationStatus,
    exit: Option<u8>,
    time: Option<u64>,
    properties: Option<&seedbox_engine_protocol::property::Ledger>,
) {
    status.oracle_exit_code = status.oracle_exit_code.or(exit);
    if let Some(properties) = properties {
        status.properties = Some(properties.assessment());
    }
    if let Some(time) = time {
        status.simulated_time_ns = status.simulated_time_ns.max(time);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{SimulationSpec, SubmitRequest, TestVerdict};

    #[test]
    fn a_passing_oracle_whose_report_was_lost_is_incomplete() {
        let status = seedbox_api::SimulationStatus {
            phase: Phase::Completed,
            oracle_exit_code: Some(0),
            properties: Some(lost_evidence()),
            ..Default::default()
        };
        assert_eq!(status.verdict(), TestVerdict::Incomplete);
    }

    struct WaitingProvider(AtomicBool);
    impl BackendProvider for WaitingProvider {
        fn project(
            &self,
            _: &Simulation,
            _: &ContentStore,
            cancellation: &CancellationToken,
        ) -> seedbox_application::Result<Backends> {
            let limit = std::time::Instant::now() + Duration::from_secs(2);
            while !cancellation.is_cancelled() && std::time::Instant::now() < limit {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.0.store(cancellation.is_cancelled(), Ordering::Release);
            Err(seedbox_application::Error::PreparationCancelled)
        }
        fn retained(
            &self,
            _: &PreparedInputs,
            _: &ContentStore,
        ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
            panic!("the preparation timeout must not launch retained execution")
        }
    }

    #[tokio::test]
    async fn capacity_api_tracks_real_admission_and_releases_joined_tasks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let manager = Arc::new(Manager::new(
            catalogue::Catalogue::open(&root.join("catalogue")).unwrap(),
        ));
        let execution = Arc::new(
            ExecutionService::new(
                manager,
                root.into(),
                ContentStore::new(root.join("store")),
                Arc::new(WaitingProvider(AtomicBool::new(false))),
            )
            .unwrap(),
        );
        let socket = root.join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::default();
        let owner = tokio::spawn(crate::serve(
            listener,
            execution.manager.clone(),
            execution.clone(),
            NonZeroUsize::MIN,
            shutdown.clone(),
        ));
        let client = seedbox_client::Client::connect_to(&socket).unwrap();
        let mut configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        for service in configuration.services.values_mut() {
            service.platform.clear();
        }
        let request = |name: &str| SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: name.into(),
            fault_profile: None,
            spec: SimulationSpec {
                program: Vec::new(),
                start_paused: false,
                seed: Some(1),
                inputs: InputSource::Project,
                configuration: configuration.clone(),
            },
        };
        let first = client.submit(&request("first")).await.unwrap().metadata.id;
        let second = client.submit(&request("second")).await.unwrap().metadata.id;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let capacity = client.capacity().await.unwrap();
                if capacity.occupying.iter().any(|o| o.simulation == first) {
                    assert_eq!(capacity.slots, 1);
                    assert_eq!(capacity.occupying.len(), 1);
                    assert_eq!(capacity.queued.len(), 1);
                    assert_eq!(capacity.queued[0].simulation, second);
                    assert_eq!(
                        capacity.queued[0].reason,
                        seedbox_api::QueueReason::ResidentCapacity
                    );
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        client.stop(&first).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let capacity = client.capacity().await.unwrap();
                if capacity.occupying.iter().any(|o| o.simulation == second) {
                    assert_eq!(capacity.occupying.len(), 1);
                    assert!(capacity.queued.is_empty());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), owner)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(execution.capacity().unwrap().occupying.is_empty());
    }

    #[test]
    fn project_execution_uses_the_canonical_mixed_memory_profile() {
        use seedbox_application::{execution::*, image::Launch};
        #[derive(Clone)]
        struct FixtureBackend(PreparedInputs);
        impl PreparationBackend for FixtureBackend {
            fn prepare(
                &self,
                _: &Simulation,
                _: &ContentStore,
            ) -> seedbox_application::Result<PreparedInputs> {
                Ok(self.0.clone())
            }
            fn prepare_cancellable(
                &self,
                simulation: &Simulation,
                store: &ContentStore,
                _: &PreparationContext,
            ) -> seedbox_application::Result<PreparedInputs> {
                self.prepare(simulation, store)
            }
        }
        impl ExecutionBackend for FixtureBackend {
            fn profile(&self) -> ExecutionProfile {
                self.0.profile.clone()
            }
            fn engine(&self) -> Option<seedbox_application::execution::EngineDescriptor> {
                Some(seedbox_application::engine::kvm_descriptor(Some(
                    "fixture-revision-dirty".into(),
                )))
            }
            fn execute(
                &self,
                request: ExecutionRequest<'_>,
            ) -> seedbox_application::Result<ExecutionReport> {
                assert_eq!(request.prepared.profile, self.profile());
                assert_eq!(request.prepared.memory_mb("app"), 2048);
                assert_eq!(request.prepared.memory_mb("oracle"), 256);
                assert_eq!(request.prepared.memory_mb("helper"), 1024);
                Ok(ExecutionReport {
                    choices: None,
                    simulated_time_ns: Some(1),
                    host_wait_ns: 0,
                    reason: StopReason::Completed,
                    workload_exit: Some(WorkloadExit {
                        service: "oracle".into(),
                        code: 0,
                    }),
                    witness: Some(self.0.kernel.clone()),
                    full_state: None,
                    faults: vec![],
                    detail: None,
                    properties: None,
                })
            }
        }
        struct Provider(FixtureBackend);
        impl BackendProvider for Provider {
            fn project(
                &self,
                _: &Simulation,
                _: &ContentStore,
                _: &CancellationToken,
            ) -> seedbox_application::Result<Backends> {
                Ok(Backends {
                    preparation: Box::new(self.0.clone()),
                    execution: Box::new(self.0.clone()),
                })
            }
            fn retained(
                &self,
                inputs: &PreparedInputs,
                _: &ContentStore,
            ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
                Ok(Box::new(FixtureBackend(inputs.clone())))
            }
        }
        let temporary = tempfile::tempdir().unwrap();
        let store = ContentStore::new(temporary.path().join("store"));
        let digest = store.put(b"test-only-binary").unwrap();
        let configuration = seedbox_application::config::parse_for_architecture(
            "name: mixed\nmachines:\n  app: {image: example/app, memoryMiB: 2048}\n  helper: {image: example/helper}\noracle: {image: example/oracle, memoryMiB: 256}\n",
            Path::new("/project/seedbox.yaml"), &Default::default(),
            if cfg!(target_arch = "aarch64") { seedbox_application::guest::Architecture::Arm64 } else { seedbox_application::guest::Architecture::Amd64 },
        ).unwrap();
        let image = PreparedImage {
            manifest: digest.clone(),
            config: digest.clone(),
            layers: vec![],
            launch: Launch {
                argv: vec!["/app".into()],
                environment: BTreeMap::new(),
                working_dir: "/".into(),
                user: "".into(),
                volumes: vec![],
                disk_volumes: vec![],
            },
        };
        let inputs = PreparedInputs {
            wall_clock_epoch: Some(86400),
            kernel: digest.clone(),
            images: BTreeMap::from([
                ("app".into(), image.clone()),
                ("helper".into(), image.clone()),
                ("oracle".into(), image),
            ]),
            placement: BTreeMap::from([
                ("app".into(), "app".into()),
                ("helper".into(), "helper".into()),
                ("oracle".into(), "oracle".into()),
            ]),
            guests: BTreeMap::from([
                ("app".into(), digest.clone()),
                ("helper".into(), digest.clone()),
                ("oracle".into(), digest.clone()),
            ]),
            machine_memory_mb: BTreeMap::from([("app".into(), 2048), ("oracle".into(), 256)]),
            profile: ExecutionProfile {
                backend: Backend::Other("test".into()),
                witness_mode: WitnessMode::Exits,
                semantic_parameters: SemanticParameters::new(1024),
            },
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        let manager = Arc::new(Manager::new(catalogue::Catalogue::in_memory()));
        let service = ExecutionService::new(
            manager.clone(),
            temporary.path().join("execution"),
            store,
            Arc::new(Provider(FixtureBackend(inputs))),
        )
        .unwrap();
        let spec = SimulationSpec {
            configuration,
            inputs: InputSource::Project,
            seed: Some(1),
            program: vec![],
            start_paused: false,
        };
        let resource = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "mixed-memory".into(),
                fault_profile: None,
                spec,
            })
            .unwrap()
            .0;
        service.execute(resource.metadata.id.clone()).unwrap();
        let finished = manager.describe(&resource.metadata.id).unwrap();
        assert_eq!(finished.status.oracle_exit_code, Some(0));
        assert_eq!(
            finished.status.termination,
            Some(seedbox_api::ExecutionTermination::OracleExited)
        );
        // The executing engine's build is recorded as provenance.
        assert_eq!(
            finished.status.engine,
            Some(seedbox_api::EngineBuild {
                version: seedbox_application::engine::SIMULATOR_VERSION.into(),
                revision: Some("fixture-revision-dirty".into()),
            })
        );
    }

    #[test]
    fn final_report_cannot_regress_a_committed_oracle_observation() {
        let directory = tempfile::tempdir().unwrap();
        let manager =
            Manager::new(catalogue::Catalogue::open(&directory.path().join("catalogue")).unwrap());
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let id = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "late-report".into(),
                fault_profile: None,
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
        manager.publish_engine_event(
            &id,
            100,
            seedbox_api::EventKind::MachineExited {
                machine: "oracle".into(),
                incarnation: 0,
                code: 0,
            },
        );
        for phase in [Phase::Finalizing, Phase::Completed] {
            manager
                .update(&id, |s| {
                    merge_report_observation(&mut s.status, Some(7), Some(50), None);
                    s.status.phase = phase;
                })
                .unwrap();
            let status = manager.describe(&id).unwrap().status;
            assert_eq!(status.simulated_time_ns, 100);
            assert_eq!(status.oracle_exit_code, Some(0));
        }
    }

    #[test]
    fn deletion_drains_control_io_and_retires_stale_handles_before_removing_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let manager = Arc::new(Manager::new(
            catalogue::Catalogue::open(&root.join("catalogue")).unwrap(),
        ));
        let service = Arc::new(
            ExecutionService::new(
                manager.clone(),
                root.into(),
                ContentStore::new(root.join("store")),
                Arc::new(WaitingProvider(AtomicBool::new(false))),
            )
            .unwrap(),
        );
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let resource = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "delete-control-io".into(),
                fault_profile: None,
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
            })
            .unwrap();
        let attempt = root.join("runs").join(id.as_str()).join("attempt");
        fs::create_dir_all(&attempt).unwrap();
        manager.attach_worker(&id, &attempt).unwrap();
        let stale = manager.worker(&id).unwrap().unwrap();
        // Hold the same mutex that drive owns throughout RPC and checkpoint I/O.
        let active_operation = stale.lock().unwrap();
        let (finished, completion) = std::sync::mpsc::channel();
        let owner = service.clone();
        let deleting_id = id.clone();
        let deletion = std::thread::spawn(move || {
            finished.send(owner.delete(&deleting_id)).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !manager.describe(&id).unwrap().status.stop_requested {
            assert!(
                std::time::Instant::now() < deadline,
                "deletion never requested stop"
            );
            std::thread::yield_now();
        }
        // Execution can complete before the held control operation finishes.
        manager
            .update(&id, |resource| {
                resource.status.phase = Phase::Completed;
            })
            .unwrap();
        assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(attempt.is_dir());
        fs::write(attempt.join("checkpoint-history"), b"finished control I/O").unwrap();
        drop(active_operation);
        completion
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        deletion.join().unwrap();
        assert!(!attempt.exists());
        assert!(matches!(
            manager.describe(&id),
            Err(catalogue::Error::NotFound(_))
        ));
        let mut worker = stale.lock().unwrap();
        assert!(worker.is_retired());
        assert!(matches!(
            worker.apply(&seedbox_api::ControlRequest {
                request_id: "stale".into(),
                command: seedbox_api::ControlCommand::Checkpoint,
            }),
            Err(crate::worker::Error::Retired)
        ));
        assert!(worker.inspect().is_err());
        assert!(!attempt.exists());
    }

    #[test]
    fn completion_retires_control_handles_before_later_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let manager = Arc::new(Manager::new(
            catalogue::Catalogue::open(&root.join("catalogue")).unwrap(),
        ));
        let service = Arc::new(
            ExecutionService::new(
                manager.clone(),
                root.into(),
                ContentStore::new(root.join("store")),
                Arc::new(WaitingProvider(AtomicBool::new(false))),
            )
            .unwrap(),
        );
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let resource = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "delete-control-io".into(),
                fault_profile: None,
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
            })
            .unwrap();
        let attempt = root.join("runs").join(id.as_str()).join("attempt");
        fs::create_dir_all(&attempt).unwrap();
        manager.attach_worker(&id, &attempt).unwrap();
        let stale = manager.worker(&id).unwrap().unwrap();
        // Hold the same mutex that drive owns throughout RPC and checkpoint I/O.
        let active_operation = stale.lock().unwrap();
        let (finished, completion) = std::sync::mpsc::channel();
        let owner = manager.clone();
        let completing_id = id.clone();
        let finalization = std::thread::spawn(move || {
            // ExecutionService::execute uses this order after its receiver drains.
            owner.retire_worker(&completing_id).unwrap();
            owner
                .update(&completing_id, |resource| {
                    resource.status.phase = Phase::Completed;
                })
                .unwrap();
            finished.send(()).unwrap();
        });
        assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
        assert_eq!(manager.describe(&id).unwrap().status.phase, Phase::Running);
        fs::write(attempt.join("checkpoint-history"), b"finished control I/O").unwrap();
        drop(active_operation);
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        finalization.join().unwrap();
        assert_eq!(
            manager.describe(&id).unwrap().status.phase,
            Phase::Completed
        );
        assert!(manager.worker(&id).unwrap().is_none());
        // Deletion begins after completion has dropped Live and its worker handle.
        service.delete(&id).unwrap();
        assert!(!attempt.exists());
        assert!(matches!(
            manager.describe(&id),
            Err(catalogue::Error::NotFound(_))
        ));
        let mut worker = stale.lock().unwrap();
        assert!(worker.is_retired());
        assert!(matches!(
            worker.apply(&seedbox_api::ControlRequest {
                request_id: "stale".into(),
                command: seedbox_api::ControlCommand::Checkpoint,
            }),
            Err(crate::worker::Error::Retired)
        ));
        assert!(worker.inspect().is_err());
        assert!(!attempt.exists());
    }

    #[test]
    fn preparation_timeout_waits_for_cleanup_and_finishes_without_an_oracle_exit() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let manager = Arc::new(Manager::new(
            catalogue::Catalogue::open(&root.join("catalogue")).unwrap(),
        ));
        let provider = Arc::new(WaitingProvider(AtomicBool::new(false)));
        let mut service = ExecutionService::new(
            manager.clone(),
            root.into(),
            ContentStore::new(root.join("store")),
            provider.clone(),
        )
        .unwrap();
        service.preparation_limit = Duration::from_millis(10);
        let mut configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        for service in configuration.services.values_mut() {
            service.platform.clear();
        }
        let resource = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "timeout".into(),
                fault_profile: None,
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
        service.execute(resource.metadata.id.clone()).unwrap();
        assert!(provider.0.load(Ordering::Acquire));
        let completed = manager.describe(&resource.metadata.id).unwrap();
        assert_eq!(completed.status.phase, Phase::Completed);
        assert_eq!(
            completed.status.termination,
            Some(ExecutionTermination::PreparationTimeLimit)
        );
        assert_eq!(completed.status.verdict(), TestVerdict::Incomplete);
        assert!(!completed.status.stop_requested);
        assert!(completed.status.stage_timings[&Stage::Preparing].complete);
        assert!(!root
            .join("inputs")
            .join(format!("{}.json", resource.metadata.id))
            .exists());
    }
}
