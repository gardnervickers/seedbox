//! Run lifecycle and replay comparison, independent of how machines execute.

use crate::{
    config::Simulation,
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::Instant,
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
    /// An explicit override wins over x-seedbox.seed. Absence of both is an error.
    pub seed: Option<u64>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug)]
pub struct Orchestrator {
    pub(crate) store: ContentStore,
    pub(crate) runs: PathBuf,
    pub(crate) log_policy: LogPolicy,
    /// Where exploration attempts share one read-only copy of each input.
    pub(crate) shared_inputs: Option<PathBuf>,
}

impl Orchestrator {
    pub fn new(store: ContentStore, runs: impl Into<PathBuf>) -> Self {
        Self {
            store,
            runs: runs.into(),
            log_policy: LogPolicy::default(),
            shared_inputs: None,
        }
    }

    pub fn with_log_policy(mut self, policy: LogPolicy) -> Self {
        self.log_policy = policy;
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
        let seed = options
            .seed
            .or(simulation.seed)
            .ok_or_else(|| Error::Validation {
                path: "seed".into(),
                message: "provide a seed in RunOptions or x-seedbox.seed".into(),
            })?;
        let identity = RunIdentity {
            version: FORMAT_VERSION,
            seed,
            configuration: simulation.clone(),
            inputs,
        };
        self.execute(identity, executor, &options.cancellation, None, false)
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
        // Refuse settings drift before creating an attempt or executing anything.
        let requested = executor.profile();
        drift(&baseline.identity.inputs.profile, &requested)?;
        let mut identity = baseline.identity.clone();
        if options.allow_engine_change {
            identity.inputs.profile.engine = requested.engine;
            // A new executable can use different argv without changing the
            // simulation's retained memory, timebase, faults or other settings.
            identity.inputs.profile.semantic_parameters.engine =
                requested.semantic_parameters.engine;
        }
        let mut run = self.execute(
            identity,
            executor,
            &options.cancellation,
            Some(&baseline),
            options.allow_engine_change,
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
        cancellation: &CancellationToken,
        baseline: Option<&RunRecord>,
        allow_engine_change: bool,
    ) -> Result<Run> {
        let mut pending = self.begin(identity, executor, allow_engine_change, baseline)?;
        let execution = match pending.ready(executor, cancellation) {
            Ok(None) => {
                let started = Instant::now();
                let execution = executor.execute(pending.request(cancellation));
                guard(execution, cancellation, started, 1, &pending.record)
            }
            Ok(Some(early)) => Ok(early),
            Err(error) => Err(error),
        };
        self.finish(pending, execution, baseline)
    }

    /// Run seeds of one prepared simulation through one backend call, one
    /// orchestrator (and so one run directory) per seed. A backend that boots
    /// once and forks each seed may serve them all from one boot; each run is
    /// recorded and replays exactly as if it ran alone.
    pub fn run_prepared_forked(
        attempts: &[(Orchestrator, u64)],
        simulation: &Simulation,
        inputs: &PreparedInputs,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
    ) -> Vec<Result<Run>> {
        let mut pending = Vec::new();
        let mut results: Vec<Option<Result<Run>>> = Vec::new();
        for (orchestrator, seed) in attempts {
            let identity = RunIdentity {
                version: FORMAT_VERSION,
                seed: *seed,
                configuration: simulation.clone(),
                inputs: inputs.clone(),
            };
            match orchestrator.begin(identity, executor, false, None) {
                Ok(mut run) => {
                    let early = run.ready(executor, cancellation);
                    results.push(None);
                    pending.push((results.len() - 1, orchestrator, run, early));
                }
                Err(error) => results.push(Some(Err(error))),
            }
        }
        let started = Instant::now();
        let batch: Vec<usize> = pending
            .iter()
            .enumerate()
            .filter(|(_, (_, _, _, early))| matches!(early, Ok(None)))
            .map(|(i, _)| i)
            .collect();
        let requests = batch
            .iter()
            .map(|&i| pending[i].2.request(cancellation))
            .collect();
        let mut executed = executor.execute_forked(requests).into_iter();
        let count = batch.len() as u64;
        for (i, (slot, orchestrator, run, early)) in pending.into_iter().enumerate() {
            let execution = match early {
                Ok(Some(report)) => Ok(report),
                Err(error) => Err(error),
                Ok(None) if batch.contains(&i) => {
                    let execution = executed.next().unwrap_or_else(|| {
                        Err(Error::Execution(
                            "execution backend returned too few forked results".into(),
                        ))
                    });
                    guard(execution, cancellation, started, count, &run.record)
                }
                Ok(None) => Err(Error::Integrity("forked attempt was not dispatched".into())),
            };
            results[slot] = Some(orchestrator.finish(run, execution, None));
        }
        results
            .into_iter()
            .map(|result| {
                result.unwrap_or_else(|| Err(Error::Integrity("forked attempt lost".into())))
            })
            .collect()
    }

    /// Create the run directory and its durable INCOMPLETE record.
    fn begin(
        &self,
        identity: RunIdentity,
        executor: &dyn ExecutionBackend,
        allow_engine_change: bool,
        baseline: Option<&RunRecord>,
    ) -> Result<Pending> {
        let run_id = self.store.put_unsynced(&serde_json::to_vec(&identity)?)?;
        fs::create_dir_all(&self.runs)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{}-", &run_id.hex()[..16]))
            .tempdir_in(&self.runs)?
            .keep();
        let record = RunRecord {
            run_id,
            attempt: directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            identity,
            performance_parameters: executor.performance_parameters(),
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
        Ok(Pending {
            store: self.store.clone(),
            directory,
            record,
            output,
            artifacts: BTreeMap::new(),
            shared_inputs: self.shared_inputs.clone(),
        })
    }

    /// Record the execution result, retain logs and compare with a baseline.
    fn finish(
        &self,
        pending: Pending,
        execution: Result<ExecutionReport>,
        baseline: Option<&RunRecord>,
    ) -> Result<Run> {
        let Pending {
            directory,
            mut record,
            output,
            ..
        } = pending;
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
        if let Some(baseline) = baseline {
            record.replay = Some(compare(baseline, &record));
        }
        write_record(&directory, &record)?;
        Ok(Run { directory, record })
    }
}

/// A run whose directory and INCOMPLETE record exist, before execution.
struct Pending {
    store: ContentStore,
    directory: PathBuf,
    record: RunRecord,
    output: PathBuf,
    artifacts: BTreeMap<ContentDigest, PathBuf>,
    shared_inputs: Option<PathBuf>,
}

impl Pending {
    /// Validate and materialize inputs. `Some` is a report that ends the run
    /// without executing it.
    fn ready(
        &mut self,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
    ) -> Result<Option<ExecutionReport>> {
        executor.validate_retained(&self.record.identity.inputs)?;
        validate_inputs(&self.record.identity, executor)?;
        fs::create_dir(&self.output)?;
        executor.preflight(Some(&self.output))?;
        for digest in executor.runtime_inputs(&self.record.identity.inputs) {
            if !self.artifacts.contains_key(digest) {
                let inputs = self.directory.join("inputs");
                let path = match &self.shared_inputs {
                    Some(shared) => self.store.materialize_linked(digest, shared, &inputs)?,
                    None => self.store.materialize(digest, &inputs)?,
                };
                self.artifacts.insert(digest.clone(), path);
            }
        }
        if cancellation.is_cancelled() {
            return Ok(Some(ExecutionReport {
                reason: StopReason::Cancelled,
                workload_exit: None,
                witness: None,
                full_state: None,
                faults: vec![],
                detail: Some("cancelled before execution".into()),
            }));
        }
        Ok(None)
    }

    fn request<'a>(&'a self, cancellation: &'a CancellationToken) -> ExecutionRequest<'a> {
        let identity = &self.record.identity;
        ExecutionRequest {
            seed: identity.seed,
            limits: &identity.configuration.limits,
            network: &identity.configuration.network,
            schedule: &identity.configuration.schedule,
            control: identity.configuration.control.as_ref(),
            check: &identity.configuration.check,
            prepared: &identity.inputs,
            artifacts: &self.artifacts,
            output: &self.output,
            cancellation,
            performance_parameters: &self.record.performance_parameters,
        }
    }
}

/// A backend must enforce its own deadline so blocking calls terminate. This
/// additional guard prevents a late return from being a success. `runs`
/// seeds executed together share `runs` times one run's wall-time limit.
fn guard(
    execution: Result<ExecutionReport>,
    cancellation: &CancellationToken,
    started: Instant,
    runs: u64,
    record: &RunRecord,
) -> Result<ExecutionReport> {
    let mut report = execution?;
    if cancellation.is_cancelled() {
        report.reason = StopReason::Cancelled;
    } else if started.elapsed().as_nanos()
        >= u128::from(record.identity.configuration.limits.wall_time_ns) * u128::from(runs.max(1))
    {
        report.reason = StopReason::WallTimeLimit;
    }
    Ok(report)
}

fn validate_inputs(identity: &RunIdentity, executor: &dyn ExecutionBackend) -> Result<()> {
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
        || identity.configuration.limits.wall_time_ns == 0
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
    Ok(if exit.code == simulation.check.exit_code {
        Outcome::Pass
    } else {
        Outcome::Fail
    })
}

fn compare(baseline: &RunRecord, fresh: &RunRecord) -> ReplayComparison {
    let outcome_matches = baseline.outcome == fresh.outcome;
    let evidence = |record: &RunRecord| {
        record.report.as_ref().is_some_and(|r| {
            r.witness.is_some()
                && (record.identity.inputs.profile.witness_mode != WitnessMode::FullState
                    || r.full_state
                        .as_ref()
                        .is_some_and(|s| s.keys().eq(record.identity.inputs.guests.keys())))
        })
    };
    let comparable = matches!(
        baseline.outcome,
        Outcome::Pass | Outcome::Fail | Outcome::Timeout
    ) && matches!(
        fresh.outcome,
        Outcome::Pass | Outcome::Fail | Outcome::Timeout
    ) && evidence(baseline)
        && evidence(fresh);
    let (witness_matches, full_state_matches, fault_history_matches, result_matches) =
        match (&baseline.report, &fresh.report) {
            (Some(a), Some(b)) => (
                a.witness
                    .as_ref()
                    .zip(b.witness.as_ref())
                    .map(|(a, b)| a == b),
                match (&a.full_state, &b.full_state) {
                    (Some(a), Some(b)) => Some(a == b),
                    (None, None) => None,
                    _ => Some(false),
                },
                a.faults == b.faults,
                a.workload_exit == b.workload_exit && a.reason == b.reason,
            ),
            _ => (None, None, false, false),
        };
    let status = if !comparable {
        ReplayStatus::NotComparable
    } else if outcome_matches
        && result_matches
        && witness_matches == Some(true)
        && full_state_matches != Some(false)
        && fault_history_matches
    {
        ReplayStatus::Match
    } else {
        ReplayStatus::Mismatch
    };
    ReplayComparison {
        status,
        outcome_matches,
        witness_matches,
        full_state_matches,
        fault_history_matches,
        ram_pages: None,
        divergence: None,
    }
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

/// Refuse, before any attempt exists, a KVM executor whose result-changing
/// settings differ from the recorded run, naming a TX-mode difference first,
/// then the first other one. The executable descriptor may differ; it only
/// selects argv.
fn drift(recorded: &ExecutionProfile, requested: &ExecutionProfile) -> Result<()> {
    if recorded.backend != Backend::Kvm {
        return Ok(());
    }
    let (was, now) = (
        &recorded.semantic_parameters,
        &requested.semantic_parameters,
    );
    let differences = was.differences(now);
    let first = if differences.contains(&"deferred_tx") {
        "deferred_tx"
    } else {
        let Some(&first) = differences.first() else {
            return Ok(());
        };
        first
    };
    Err(Error::Integrity(match first {
        "deferred_tx" => "executor TX mode does not match the recorded --deferred-tx; use an executor with the recorded TX mode".into(),
        setting => format!(
            "executor {setting} does not match the recorded run; use an executor with the recorded settings"
        ),
    }))
}
