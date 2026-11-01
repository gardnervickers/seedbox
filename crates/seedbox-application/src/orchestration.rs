//! Run lifecycle, independent of how machines execute.

use crate::{config::Simulation, execution::*, store::ContentStore, Error, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

pub use seedbox_artifacts::record::*;

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// Optional active host-time budget. Never part of the experiment identity.
    pub wall_time_ns: Option<u64>,
    /// An explicit override wins over the resolved seed.
    pub seed: Option<u64>,
    pub cancellation: CancellationToken,
    /// Machine worker threads for this attempt. Performance only; never part
    /// of the experiment identity. `None` uses the executor's default.
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
    pub(crate) program: Vec<seedbox_engine_protocol::input_program::TimedInput>,
    pub(crate) store: ContentStore,
    pub(crate) runs: PathBuf,
    pub(crate) log_policy: LogPolicy,
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
            program: Vec::new(),
            store,
            runs: runs.into(),
            log_policy: LogPolicy::default(),
        }
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
                message: "provide a seed in RunOptions".into(),
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
                message: "provide a seed in RunOptions".into(),
            })?;
        let configuration = simulation.clone();
        let identity = RunIdentity {
            program: self.program.clone(),
            version: FORMAT_VERSION,
            seed,
            configuration,
            inputs,
        };
        self.execute(identity, executor, &options, progress)
    }

    fn execute(
        &self,
        identity: RunIdentity,
        executor: &dyn ExecutionBackend,
        options: &RunOptions,
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
            log_policy: self.log_policy.clone(),
            omitted_logs: BTreeMap::new(),
            outcome: Outcome::Incomplete,
            report: None,
            error: None,
            logs: BTreeMap::new(),
        };
        // A killed process leaves a durable INCOMPLETE record, never a PASS.
        write_record(&directory, &record)?;
        let output = directory.join("output");
        let execution = (|| -> Result<ExecutionReport> {
            executor.preflight_resume(
                self.resume_checkpoint.as_deref(),
                record.identity.configuration.control.as_ref(),
            )?;
            // Publish an explicit empty live journal before exposing this attempt.
            // Interactive admissions append here; replay inputs come from the program.
            seedbox_artifacts::interventions::Journal::initialize(
                &directory.join("interventions.json"),
            )?;
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
                    let path = if executable {
                        self.store.materialize_executable(digest, &inputs)?
                    } else {
                        self.store.materialize(digest, &inputs)?
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
                    properties: None,
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
                quantum_ns: record.identity.configuration.simulator.quantum_ns,
                start_paused: self.start_paused,
                resume_checkpoint: self.resume_checkpoint.as_deref(),
                intervention_history: Some(&directory.join("interventions.json")),
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
                seedbox_engine_protocol::input_program::merge_interventions(
                    &mut record.identity.program,
                    &history,
                )
                .map_err(Error::Integrity)?;
                record.run_id = self
                    .store
                    .put_unsynced(&serde_json::to_vec(&record.identity)?)?;
            }
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

/// The recorded outcome follows the verdict policy
/// (`seedbox_engine_core::properties::verdict`), with one precedence: a
/// violated property fails the run whatever else happened (a time limit,
/// cancellation, quiescence, an error or a missing oracle exit), as the
/// status verdict does; then the termination rules below apply, an unmet
/// requirement fails a run whose oracle exited, and lost property evidence
/// leaves a passing oracle's run incomplete.
fn classify(identity: &RunIdentity, report: &ExecutionReport) -> Result<Outcome> {
    use seedbox_engine_core::properties::{verdict, Findings, Verdict};
    let findings = report.properties.as_ref().map(Findings::from);
    if verdict(Verdict::Incomplete, false, true, findings) == Verdict::Failed {
        return Ok(Outcome::Fail);
    }
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
    // Property findings qualify the oracle's exit as the run's verdict does.
    Ok(
        match verdict(Verdict::of_exit(exit.code), true, true, findings) {
            Verdict::Passed => Outcome::Pass,
            Verdict::Failed => Outcome::Fail,
            Verdict::Incomplete | Verdict::Pending => Outcome::Incomplete,
        },
    )
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
        properties: None,
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
    fn a_violated_property_fails_the_recorded_outcome_whatever_else_happened() {
        use seedbox_engine_protocol::property::Assessment;
        #[derive(Clone, Copy, Debug)]
        enum Found {
            Violated,
            Unmet,
            Lost,
            Nothing,
        }
        let record = seedbox_artifacts::test_support::fixture();
        let base = record.report.clone().unwrap();
        let assessment = |found| {
            let mut assessment = Assessment::default();
            match found {
                Found::Violated => {
                    assessment
                        .violations
                        .push(seedbox_engine_protocol::property::Violation {
                            id: "p".into(),
                            kind: seedbox_engine_protocol::property::Kind::Always,
                            example: seedbox_engine_protocol::property::Example {
                                origin: seedbox_engine_protocol::property::Origin {
                                    machine: "workload".into(),
                                    incarnation: 0,
                                    session: 1,
                                    seq: 1,
                                    time_ns: 0,
                                },
                                details: None,
                                details_truncated: false,
                            },
                        })
                }
                Found::Unmet => assessment.unmet_requirements.push("p".into()),
                Found::Lost => assessment.lost_count = 1,
                Found::Nothing => {}
            }
            assessment
        };
        let classified = |reason, exit: Option<u8>, found: Option<Found>| {
            let mut report = base.clone();
            report.reason = reason;
            report.workload_exit = exit.map(|code| crate::execution::WorkloadExit {
                service: "workload".into(),
                code,
            });
            report.properties = found.map(assessment);
            classify(&record.identity, &report).ok()
        };
        use Found::*;
        use StopReason::*;
        for (reason, exit, found, expected) in [
            // A violation decides first, as the status verdict does.
            (SimulatedTimeLimit, None, Some(Violated), Outcome::Fail),
            (Quiescent, None, Some(Violated), Outcome::Fail),
            (Cancelled, Some(0), Some(Violated), Outcome::Fail),
            (Error, None, Some(Violated), Outcome::Fail),
            (Completed, None, Some(Violated), Outcome::Fail),
            (Completed, Some(0), Some(Violated), Outcome::Fail),
            // Otherwise the termination rules apply.
            (SimulatedTimeLimit, None, Some(Lost), Outcome::Timeout),
            (SimulatedTimeLimit, None, Some(Unmet), Outcome::Timeout),
            (Quiescent, None, Some(Nothing), Outcome::Incomplete),
            (Completed, None, Some(Unmet), Outcome::Incomplete),
            (Completed, Some(0), Some(Unmet), Outcome::Fail),
            (Completed, Some(3), Some(Unmet), Outcome::Fail),
            (Completed, Some(0), Some(Lost), Outcome::Incomplete),
            (Completed, Some(0), Some(Nothing), Outcome::Pass),
            (Completed, Some(0), None, Outcome::Pass),
            (Completed, Some(1), Some(Nothing), Outcome::Fail),
            // An oracle that could not decide leaves the run incomplete.
            (Completed, Some(3), None, Outcome::Incomplete),
        ] {
            assert_eq!(
                classified(reason.clone(), exit, found),
                Some(expected),
                "{reason:?} {exit:?} {found:?}"
            );
        }
        assert_eq!(classified(Error, None, Some(Nothing)), None);
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
