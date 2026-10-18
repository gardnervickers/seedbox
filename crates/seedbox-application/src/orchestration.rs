//! Run lifecycle and replay comparison, independent of how machines execute.

use crate::{config::Simulation, execution::*, store::ContentStore, Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub use seedbox_artifacts::record::*;

#[derive(Clone, Debug, Default)]
pub struct ReplayOptions {
    pub cancellation: CancellationToken,
    /// Semantic settings must still match. A new engine gives a new identity.
    pub allow_engine_change: bool,
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Optional active host-time budget. Never part of the experiment identity.
    pub wall_time_ns: Option<u64>,
    /// An explicit override wins over the resolved seed.
    pub seed: Option<u64>,
    pub cancellation: CancellationToken,
    /// Machine worker threads for this attempt. Performance only; never part
    /// of the experiment identity. `None` keeps the executor default.
    pub machine_workers: Option<u32>,
}

pub enum RunProgress<'a> {
    /// Materialized inputs are ready; execution will use this attempt directory.
    Starting { directory: &'a Path },
    /// Preserve any oracle report before fallible artifact finalization.
    Finalizing { report: Option<&'a ExecutionReport> },
}

#[derive(Clone, Debug)]
pub struct Orchestrator {
    pub(crate) start_paused: bool,
    pub(crate) resume_checkpoint: Option<PathBuf>,
    pub(crate) observations: Option<PathBuf>,
    pub(crate) replay_history: Option<crate::store::ContentDigest>,
    pub(crate) program: Vec<seedbox_engine_protocol::input_program::TimedInput>,
    pub(crate) store: ContentStore,
    pub(crate) runs: PathBuf,
    pub(crate) log_policy: LogPolicy,
    /// Where exploration attempts share one read-only copy of each input.
    pub(crate) shared_inputs: Option<PathBuf>,
}

impl Orchestrator {
    /// Hold execution at its first scheduler boundary until resumed by a client.
    pub fn with_start_paused(mut self, paused: bool) -> Self {
        self.start_paused = paused;
        self
    }

    /// Continue a saved engine position using the source simulation inputs.
    pub fn with_checkpoint(mut self, path: PathBuf) -> Self {
        self.resume_checkpoint = Some(path);
        self
    }

    pub fn with_observations(mut self, socket: PathBuf) -> Self {
        self.observations = Some(socket);
        self
    }

    pub fn new(store: ContentStore, runs: impl Into<PathBuf>) -> Self {
        Self {
            start_paused: false,
            resume_checkpoint: None,
            observations: None,
            replay_history: None,
            program: Vec::new(),
            store,
            runs: runs.into(),
            log_policy: LogPolicy::default(),
            shared_inputs: None,
        }
    }

    /// Reapply a verified retained history along with prepared machine inputs.
    pub fn with_interventions(mut self, history: Option<crate::store::ContentDigest>) -> Self {
        self.replay_history = history;
        self
    }

    pub fn with_log_policy(mut self, policy: LogPolicy) -> Self {
        self.log_policy = policy;
        self
    }

    pub fn with_program(
        mut self,
        program: Vec<seedbox_engine_protocol::input_program::TimedInput>,
    ) -> Self {
        self.program = program;
        self
    }

    /// Synchronous and runtime-independent: an async caller may use a blocking
    /// task/thread and cancel through the shared token. Calls own no global state.
    pub fn run(
        &self,
        simulation: &Simulation,
        options: RunOptions,
        preparer: &dyn PreparationBackend,
        executor: &dyn ExecutionBackend,
    ) -> Result<Run> {
        let seed = options
            .seed
            .or(simulation.seed)
            .ok_or_else(|| Error::Validation {
                path: "seed".into(),
                message: "provide a seed in RunOptions or x-seedbox.seed".into(),
            })?;
        preparer.validate_runtime_inputs()?;
        executor.preflight(None)?;
        // No preflight resource survives preparation.
        let inputs = preparer.prepare(simulation, &self.store)?;
        self.run_prepared(
            simulation,
            RunOptions {
                seed: Some(seed),
                ..options
            },
            inputs,
            executor,
        )
    }

    /// Execute a complete retained closure without consulting preparation sources.
    /// The ordinary execution path still verifies profile, placement and bytes.
    pub fn run_prepared(
        &self,
        simulation: &Simulation,
        options: RunOptions,
        inputs: PreparedInputs,
        executor: &dyn ExecutionBackend,
    ) -> Result<Run> {
        self.run_prepared_observed(simulation, options, inputs, executor, |_| Ok(()))
    }

    /// Publish attempt readiness and the report before fallible finalization. A
    /// server can durably preserve the oracle exit even if a later record write
    /// fails. Finalizing includes None when the backend failed without a report.
    /// A callback failure stops orchestration.
    pub fn run_prepared_observed(
        &self,
        simulation: &Simulation,
        options: RunOptions,
        inputs: PreparedInputs,
        executor: &dyn ExecutionBackend,
        progress: impl FnMut(RunProgress<'_>) -> Result<()>,
    ) -> Result<Run> {
        let seed = options
            .seed
            .or(simulation.seed)
            .ok_or_else(|| Error::Validation {
                path: "seed".into(),
                message: "provide a seed in RunOptions or x-seedbox.seed".into(),
            })?;
        let identity = RunIdentity {
            interventions: self.replay_history.clone(),
            program: self.program.clone(),
            version: FORMAT_VERSION,
            seed,
            configuration: simulation.clone(),
            inputs,
        };
        self.execute(identity, executor, &options, None, false, progress)
    }

    /// Fresh execution with the original seed and retained artifacts. There is
    /// intentionally no preparation backend argument: replay cannot rebuild.
    pub fn replay(
        &self,
        directory: &Path,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
    ) -> Result<Run> {
        self.replay_with_options(
            directory,
            executor,
            ReplayOptions {
                cancellation: cancellation.clone(),
                ..Default::default()
            },
        )
    }

    pub fn replay_with_options(
        &self,
        directory: &Path,
        executor: &dyn ExecutionBackend,
        options: ReplayOptions,
    ) -> Result<Run> {
        let baseline = inspect(directory)?;
        let requested = executor.profile();
        let mut identity = baseline.identity.clone();
        if options.allow_engine_change {
            // A new executable can use different argv without changing the
            // simulation's retained memory, timebase, faults or other settings.
            identity.inputs.profile.semantic_parameters.engine =
                requested.semantic_parameters.engine;
        }
        let mut run = self.execute(
            identity,
            executor,
            &RunOptions {
                cancellation: options.cancellation.clone(),
                ..Default::default()
            },
            Some(&baseline),
            options.allow_engine_change,
            |_| Ok(()),
        )?;
        if let Some(comparison) = &mut run.record.replay {
            if comparison.status == ReplayStatus::Mismatch {
                // Optional evidence must never replace the execution verdict.
                comparison.divergence = Some(
                    crate::diff::replay_divergence(directory, &run.directory).unwrap_or_else(
                        |error| crate::diff::DivergenceReport {
                            divergence: None,
                            limitations: vec![error.to_string()],
                        },
                    ),
                );
                if comparison.full_state_matches == Some(false) {
                    comparison.ram_pages = Some(
                        crate::diff::replay_ram_pages(directory, &run.directory).unwrap_or_else(
                            |error| crate::diff::RamPageReport {
                                machines: BTreeMap::new(),
                                limitations: vec![error.to_string()],
                            },
                        ),
                    );
                }
                write_record(&run.directory, &run.record)?;
            }
        }
        Ok(run)
    }

    fn execute(
        &self,
        identity: RunIdentity,
        executor: &dyn ExecutionBackend,
        options: &RunOptions,
        baseline: Option<&RunRecord>,
        allow_engine_change: bool,
        mut progress: impl FnMut(RunProgress<'_>) -> Result<()>,
    ) -> Result<Run> {
        let cancellation = &options.cancellation;
        let run_id = self.store.put_unsynced(&serde_json::to_vec(&identity)?)?;
        fs::create_dir_all(&self.runs)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{}-", &run_id.hex()[..16]))
            .tempdir_in(&self.runs)?
            .keep();
        let mut record = RunRecord {
            run_id,
            attempt: directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            identity,
            performance_parameters: performance_parameters(executor, options.machine_workers),
            allow_engine_change,
            log_policy: self.log_policy.clone(),
            omitted_logs: BTreeMap::new(),
            outcome: Outcome::Incomplete,
            report: None,
            error: None,
            logs: BTreeMap::new(),
            replay_of: baseline.map(|r| r.attempt.clone()),
            replay: None,
        };
        // A killed process leaves a durable INCOMPLETE record, never a PASS.
        write_record(&directory, &record)?;
        let output = directory.join("output");
        let execution = (|| -> Result<ExecutionReport> {
            executor.preflight_resume(
                self.resume_checkpoint.as_deref(),
                record.identity.configuration.control.as_ref(),
            )?;
            let replay_interventions = record
                .identity
                .interventions
                .as_ref()
                .map(|digest| {
                    let path = self.store.materialize(digest, &directory.join("inputs"))?;
                    seedbox_artifacts::interventions::read(&path)
                })
                .transpose()?;
            {
                // Publish an explicit empty history too: interrupted attempts
                // must not confuse missing input with no interventions.
                let mut journal = seedbox_artifacts::interventions::Journal::initialize(
                    &directory.join("interventions.json"),
                )?;
                if let Some(history) = &replay_interventions {
                    for entry in &history.entries {
                        journal.append(entry.clone())?;
                    }
                }
            }
            executor.validate_retained(&record.identity.inputs)?;
            validate_inputs(&record.identity, executor)?;
            fs::create_dir(&output)?;
            executor.preflight(Some(&output))?;
            let mut artifacts = BTreeMap::new();
            for digest in executor.runtime_inputs(&record.identity.inputs) {
                if !artifacts.contains_key(digest) {
                    let inputs = directory.join("inputs");
                    // The retained engine is the one host-executed input; guest
                    // inputs are read as bytes (see Runtime::select).
                    let executable = record
                        .identity
                        .inputs
                        .executable()
                        .is_ok_and(|engine| digest == engine);
                    let path = match (&self.shared_inputs, executable) {
                        (Some(shared), true) => self
                            .store
                            .materialize_linked_executable(digest, shared, &inputs)?,
                        (Some(shared), false) => {
                            self.store.materialize_linked(digest, shared, &inputs)?
                        }
                        (None, true) => self.store.materialize_executable(digest, &inputs)?,
                        (None, false) => self.store.materialize(digest, &inputs)?,
                    };
                    artifacts.insert(digest.clone(), path);
                }
            }
            if cancellation.is_cancelled() {
                return Ok(ExecutionReport {
                    host_wait_ns: 0,
                    simulated_time_ns: None,
                    reason: StopReason::Cancelled,
                    workload_exit: None,
                    witness: None,
                    full_state: None,
                    faults: vec![],
                    detail: Some("cancelled before execution".into()),
                });
            }
            let start = seedbox_engine_protocol::host_clock::Instant::now()?;
            progress(RunProgress::Starting {
                directory: &directory,
            })?;
            let limits = crate::config::Limits {
                simulated_time_ns: record.identity.configuration.limits.simulated_time_ns,
                wall_time_ns: options.wall_time_ns.unwrap_or(u64::MAX),
            };
            let mut report = executor.execute(ExecutionRequest {
                start_paused: self.start_paused,
                resume_checkpoint: self.resume_checkpoint.as_deref(),
                intervention_history: Some(&directory.join("interventions.json")),
                replay_interventions: replay_interventions.as_ref(),
                observations: self.observations.as_deref(),
                seed: record.identity.seed,
                program: &record.identity.program,
                limits: &limits,
                network: &record.identity.configuration.network,
                schedule: &record.identity.configuration.schedule,
                control: record.identity.configuration.control.as_ref(),
                check: &record.identity.configuration.check,
                prepared: &record.identity.inputs,
                artifacts: &artifacts,
                output: &output,
                cancellation,
                performance_parameters: &record.performance_parameters,
            })?;
            // A backend must enforce its own deadline so blocking calls terminate.
            // This additional guard prevents a late return from being a success.
            // Keep explicit backend/cleanup failures visible even when a
            // cancellation or deadline races with the completed report.
            apply_final_deadline(&mut report, cancellation, limits.wall_time_ns, || {
                Ok(start.elapsed()?)
            })?;
            Ok(report)
        })();
        progress(RunProgress::Finalizing {
            report: execution.as_ref().ok(),
        })?;
        match execution {
            Ok(report) => {
                match classify(&record.identity, &report) {
                    Ok(outcome) => record.outcome = outcome,
                    Err(error) => {
                        record.outcome = Outcome::Error;
                        record.error = Some(error.to_string());
                    }
                }
                record.report = Some(report);
            }
            Err(error) => {
                record.outcome = Outcome::Error;
                record.error = Some(error.to_string());
            }
        }
        // Preserve backend logs even when execute returned an error.
        if output.symlink_metadata().is_ok() {
            if let Err(error) = retain_logs(&output, &output, &self.store, &mut record, &mut 0) {
                record.outcome = Outcome::Error;
                record.error = Some(match record.error.take() {
                    Some(previous) => format!("{previous}; retaining logs: {error}"),
                    None => format!("retaining logs: {error}"),
                });
            }
        }
        // Seal semantic inputs independently of optional logs. The attempt path
        // stays stable while its completed identity includes interactive actions.
        if let Some(history) =
            seedbox_artifacts::interventions::read_optional(&directory.join("interventions.json"))?
        {
            if !history.entries.is_empty() {
                let digest = self
                    .store
                    .put_unsynced(&history.encode().map_err(Error::Integrity)?)?;
                if record
                    .identity
                    .interventions
                    .as_ref()
                    .is_some_and(|expected| expected != &digest)
                {
                    return Err(Error::Integrity(
                        "replayed intervention history changed during execution".into(),
                    ));
                }
                seedbox_engine_protocol::input_program::merge_interventions(
                    &mut record.identity.program,
                    &history,
                )
                .map_err(Error::Integrity)?;
                record.identity.interventions = None;
                record.run_id = self
                    .store
                    .put_unsynced(&serde_json::to_vec(&record.identity)?)?;
            }
        }
        if let Some(baseline) = baseline {
            record.replay = Some(compare(baseline, &record));
        }
        write_record(&directory, &record)?;
        Ok(Run { directory, record })
    }
}

fn validate_inputs(identity: &RunIdentity, executor: &dyn ExecutionBackend) -> Result<()> {
    crate::fault_inputs::validate_input_program(&identity.configuration, &identity.program)?;
    let inputs = &identity.inputs;
    let machines: BTreeSet<_> = inputs.placement.values().collect();
    if inputs
        .placement
        .keys()
        .ne(identity.configuration.services.keys())
        || machines.iter().copied().ne(inputs.guests.keys())
        || inputs.guests.keys().any(|name| name.is_empty())
    {
        return Err(Error::Integrity(
            "placement must map every service to exactly the prepared machines".into(),
        ));
    }
    if identity.version != FORMAT_VERSION {
        return Err(Error::Integrity("unsupported run format".into()));
    }
    if identity.inputs.profile != executor.profile() {
        return Err(Error::Integrity(
            "execution profile does not match the recorded backend/engine/settings".into(),
        ));
    }
    if identity
        .configuration
        .services
        .keys()
        .ne(identity.inputs.images.keys())
    {
        return Err(Error::Integrity(
            "prepared images must exactly match configured services".into(),
        ));
    }
    if !identity
        .configuration
        .services
        .contains_key(&identity.configuration.check.service)
        || identity.configuration.limits.simulated_time_ns == 0
    {
        return Err(Error::Integrity(
            "invalid check or limits in prepared simulation".into(),
        ));
    }
    Ok(())
}

fn classify(identity: &RunIdentity, report: &ExecutionReport) -> Result<Outcome> {
    let simulation = &identity.configuration;
    // A backend stopping early may not have a complete effect transcript.
    // Preserve its termination reason and its specific diagnostic first.
    match report.reason {
        StopReason::Cancelled | StopReason::WallTimeLimit | StopReason::Quiescent => {
            return Ok(Outcome::Incomplete)
        }
        StopReason::Error => {
            return Err(Error::Execution(
                report
                    .detail
                    .clone()
                    .unwrap_or("backend reported an error".into()),
            ))
        }
        StopReason::SimulatedTimeLimit => return Ok(Outcome::Timeout),
        StopReason::Completed => {}
    }
    for fault in &report.faults {
        if fault.applied_at_ns < fault.requested_at_ns
            || !(simulation.services.contains_key(&fault.service)
                || (fault.service.is_empty()
                    && matches!(
                        fault.action.as_str(),
                        "partition" | "shape" | "drop_connection" | "heal"
                    )))
        {
            return Err(Error::Execution(
                "fault effect must acknowledge a known service at or after its request time".into(),
            ));
        }
    }
    let Some(exit) = &report.workload_exit else {
        return Ok(Outcome::Incomplete);
    };
    if exit.service != simulation.check.service {
        return Err(Error::Execution(
            "result is for a different workload service".into(),
        ));
    }
    if report.witness.is_none() {
        return Err(Error::Execution(
            "completed workload is missing its execution witness".into(),
        ));
    }
    if identity.inputs.profile.witness_mode == WitnessMode::FullState && report.full_state.is_none()
    {
        return Err(Error::Execution(
            "requested full-state evidence is missing".into(),
        ));
    }
    if let Some(state) = &report.full_state {
        if state.keys().ne(identity.inputs.guests.keys()) {
            return Err(Error::Execution(
                "full-state digests must cover exactly the configured machines".into(),
            ));
        }
    }
    Ok(if exit.code == 0 {
        Outcome::Pass
    } else {
        Outcome::Fail
    })
}

fn retain_logs(
    base: &Path,
    directory: &Path,
    store: &ContentStore,
    record: &mut RunRecord,
    retained_bytes: &mut u64,
) -> Result<()> {
    if !fs::symlink_metadata(directory)?.file_type().is_dir() {
        return Err(Error::Integrity(
            "backend output must be a real directory".into(),
        ));
    }
    let entries = fs::read_dir(directory)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<BTreeSet<_>>>()?;
    for path in entries {
        let kind = fs::symlink_metadata(&path)?.file_type();
        if kind.is_dir() {
            retain_logs(base, &path, store, record, retained_bytes)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_str()
                .ok_or_else(|| Error::Integrity("non-UTF-8 log path".into()))?
                .to_owned();
            let size = fs::metadata(&path)?.len();
            if size > record.log_policy.max_file_bytes
                || size
                    > record
                        .log_policy
                        .max_total_bytes
                        .saturating_sub(*retained_bytes)
            {
                record.omitted_logs.insert(
                    relative,
                    format!(
                        "{size} bytes exceeds retention policy; file remains in attempt output"
                    ),
                );
            } else {
                record
                    .logs
                    .insert(relative, store.put_file_unsynced(&path)?);
                *retained_bytes += size;
            }
        } else {
            return Err(Error::Integrity(format!(
                "backend output is not a regular file/directory: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

// Keep the fallible elapsed read lazy: an explicit error or cancellation must
// retain its existing precedence without consulting the clock at all.
fn apply_final_deadline(
    report: &mut ExecutionReport,
    cancellation: &CancellationToken,
    wall_time_ns: u64,
    elapsed: impl FnOnce() -> Result<std::time::Duration>,
) -> Result<()> {
    if report.reason != StopReason::Error {
        if cancellation.is_cancelled() {
            report.reason = StopReason::Cancelled;
        } else if elapsed()?
            .saturating_sub(std::time::Duration::from_nanos(report.host_wait_ns))
            .as_nanos()
            >= u128::from(wall_time_ns)
        {
            report.reason = StopReason::WallTimeLimit;
        }
    }
    Ok(())
}

/// Applies a per-run machine-worker override to an executor that has the setting.
fn performance_parameters(
    executor: &dyn ExecutionBackend,
    machine_workers: Option<u32>,
) -> BTreeMap<String, String> {
    let mut parameters = executor.performance_parameters();
    if let Some(workers) = machine_workers {
        if let Some(value) = parameters.get_mut("SEEDBOX_MACHINE_WORKERS") {
            *value = workers.to_string();
        }
    }
    parameters
}

#[cfg(test)]
fn incomplete(reason: crate::execution::StopReason) -> crate::execution::ExecutionReport {
    crate::execution::ExecutionReport {
        reason,
        workload_exit: None,
        detail: None,
        witness: None,
        full_state: None,
        host_wait_ns: 0,
        simulated_time_ns: None,
        faults: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_test_time::ControlledClock;
    use std::time::Duration;

    #[test]
    fn final_deadline_excludes_accounted_wait_and_keeps_the_exact_boundary() {
        let elapsed = ControlledClock::default();
        elapsed.advance_by(Duration::from_millis(150)).unwrap();
        for (waited_ms, expected) in [
            (150, StopReason::Completed),
            (51, StopReason::Completed),
            (50, StopReason::WallTimeLimit),
            (0, StopReason::WallTimeLimit),
            (200, StopReason::Completed),
        ] {
            let waited = ControlledClock::new(Duration::from_millis(waited_ms));
            let mut report = incomplete(StopReason::Completed);
            report.host_wait_ns = waited.now().as_nanos().try_into().unwrap();
            report.detail = Some("preserved backend report".into());
            apply_final_deadline(
                &mut report,
                &CancellationToken::default(),
                100_000_000,
                || Ok(elapsed.now()),
            )
            .unwrap();
            assert_eq!(report.reason, expected, "accounted wait: {waited_ms}ms");
            assert_eq!(report.detail.as_deref(), Some("preserved backend report"));
            assert_eq!(report.host_wait_ns, waited_ms * 1_000_000);
        }
    }

    #[test]
    fn final_deadline_preserves_error_and_cancellation_without_sampling_elapsed() {
        for (reason, cancelled, expected) in [
            (StopReason::Error, false, StopReason::Error),
            (StopReason::Error, true, StopReason::Error),
            (StopReason::Completed, true, StopReason::Cancelled),
            (StopReason::WallTimeLimit, true, StopReason::Cancelled),
        ] {
            let cancellation = CancellationToken::default();
            if cancelled {
                cancellation.cancel();
            }
            let mut report = incomplete(reason);
            report.detail = Some("preserved backend report".into());
            report.host_wait_ns = 123;
            apply_final_deadline(&mut report, &cancellation, 0, || {
                panic!("error/cancellation must not sample the elapsed clock")
            })
            .unwrap();
            assert_eq!(report.reason, expected);
            assert_eq!(report.detail.as_deref(), Some("preserved backend report"));
            assert_eq!(report.host_wait_ns, 123);
        }
    }

    #[test]
    fn final_deadline_propagates_elapsed_failure_without_rewriting_the_report() {
        let mut report = incomplete(StopReason::Completed);
        let error = apply_final_deadline(
            &mut report,
            &CancellationToken::default(),
            100_000_000,
            || Err(Error::Execution("elapsed sample failed".into())),
        )
        .unwrap_err();
        assert!(error.to_string().contains("elapsed sample failed"));
        assert_eq!(report.reason, StopReason::Completed);
    }
}
