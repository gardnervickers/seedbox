//! Server-owned application preparation and execution. Platform discovery is
//! injected by the executable; clients never own worker lifetimes or leases.

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
    fs,
    io::Write,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
    #[error("simulation worker task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

pub struct ExecutionService {
    manager: Arc<Manager>,
    root: PathBuf,
    store: ContentStore,
    backends: Arc<dyn BackendProvider>,
    preparation_limit: Duration,
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
        })
    }

    /// Foreground cascading deletion: stop the frozen ownership tree, wait for
    /// execution cleanup, then remove dependents before their parents.
    pub fn delete(&self, id: &SimulationId) -> seedbox_application::Result<()> {
        let setup = |e: catalogue::Error| seedbox_application::Error::Setup(e.to_string());
        let ids = self.manager.begin_delete(id).map_err(setup)?;
        for id in ids {
            loop {
                match self.manager.describe(&id) {
                    Ok(resource) if resource.status.phase != Phase::Completed => {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                    Ok(_) | Err(catalogue::Error::NotFound(_)) => break,
                    Err(error) => return Err(setup(error)),
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
            self.manager.delete(&id).map_err(setup)?;
        }
        Ok(())
    }

    /// Block new branches before removing the completed simulation's capture.
    /// Repeated deletion retries cleanup using the retained attempt reference.
    pub fn delete_checkpoint(
        &self,
        id: &SimulationId,
        name: &str,
    ) -> seedbox_application::Result<()> {
        self.manager
            .delete_checkpoint(id, name)
            .map_err(|e| seedbox_application::Error::Setup(e.to_string()))?;
        let inputs: RetainedInputs = serde_json::from_reader(fs::File::open(
            self.root.join("inputs").join(format!("{id}.json")),
        )?)?;
        let attempt = inputs.attempt.ok_or_else(|| {
            seedbox_application::Error::Setup("checkpoint source has no execution attempt".into())
        })?;
        crate::checkpoint_cleanup::remove(&attempt, name).map_err(|e| {
            seedbox_application::Error::Setup(format!("checkpoint {name} cleanup: {e}"))
        })
    }

    /// Read final evidence using server-owned attempt references. Clients never
    /// need host run paths, and comparison never changes a simulation's verdict.
    pub fn compare(
        &self,
        baseline: &SimulationId,
        simulation: &SimulationId,
    ) -> seedbox_application::Result<seedbox_api::SimulationComparison> {
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
            let path = self.root.join("inputs").join(format!("{id}.json"));
            let file = fs::File::open(&path).map_err(|error| {
                seedbox_application::Error::Setup(format!(
                    "simulation {id} retained inputs: {error}"
                ))
            })?;
            let inputs: RetainedInputs = serde_json::from_reader(file)?;
            let attempt = inputs.attempt.ok_or_else(|| {
                seedbox_application::Error::Setup(format!(
                    "simulation {id} has no retained execution"
                ))
            })?;
            seedbox_application::orchestration::inspect(&attempt)
        };
        let first = record(baseline)?;
        let second = record(simulation)?;
        let comparison = seedbox_artifacts::record::compare(&first, &second);
        use seedbox_api::ComparisonStatus;
        let status = if first.identity != second.identity {
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
        let recovery = self.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Error> {
            for id in recovery
                .manager
                .read(|catalogue| Ok(catalogue.pending_deletions()))?
            {
                recovery
                    .delete(&id)
                    .map_err(|e| Error::Catalogue(catalogue::Error::Invalid(e.to_string())))?;
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
                result = tasks.join_next(), if !tasks.is_empty() => {
                    let error = match result {
                        Some(Ok(Err(error))) => Some(error),
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
                            tasks.spawn_blocking(move || service.execute(id));
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
                self.manager.update(&id, |s| {
                    s.status.oracle_exit_code = s.status.oracle_exit_code.or(exit);
                    if let Some(time) = report.and_then(|report| report.simulated_time_ns) {
                        s.status.simulated_time_ns = time;
                    }
                    s.status.termination = Some(termination);
                    s.status.phase = Phase::Completed;
                    s.status.stage = None;
                })?;
            }
            Err(error) => {
                self.manager.update(&id, |s| {
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
                        let digest = self.store.put(
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
        self.manager
            .update(id, |s| {
                s.spec.inputs = InputSource::Retained {
                    simulation: id.clone(),
                };
                s.status.phase = Phase::Running;
                s.status.stage = Some(Stage::Booting);
            })
            .map_err(|e| seedbox_application::Error::Execution(e.to_string()))?;
        let observations = crate::observations::Receiver::start(self.manager.clone(), id.clone())?;
        let mut orchestrator = Orchestrator::new(self.store.clone(), directory.join("attempts"));
        if let Some(checkpoint) = checkpoint {
            orchestrator = orchestrator.with_checkpoint(checkpoint);
        }
        orchestrator
            .with_observations(observations.path())
            .with_interventions(interventions)
            .run_prepared_observed(
                &spec.configuration,
                RunOptions {
                    seed: spec.seed,
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
                            s.status.oracle_exit_code = s.status.oracle_exit_code.or(exit);
                            if let Some(time) = report.and_then(|report| report.simulated_time_ns) {
                                s.status.simulated_time_ns = time;
                            }
                        })
                        .map_err(|e| seedbox_application::Error::Execution(e.to_string()))?;
                    Ok(())
                },
            )
    }
}

impl ExecutionService {
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
                .put(
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

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{SimulationSpec, SubmitRequest, TestVerdict};

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
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let resource = manager
            .submit(SubmitRequest {
                request_id: "timeout".into(),
                spec: SimulationSpec {
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
        assert!(
            !root
                .join("inputs")
                .join(format!("{}.json", resource.metadata.id))
                .exists()
        );
    }
}
