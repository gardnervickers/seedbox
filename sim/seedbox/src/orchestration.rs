//! Run lifecycle and replay comparison, independent of how machines execute.

use crate::{
    config::Simulation,
    execution::*,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::Instant,
};

const FORMAT_VERSION: u32 = 2;

#[derive(Clone, Debug, Default)]
pub struct ReplayOptions {
    pub cancellation: CancellationToken,
    /// Semantic settings must still match. A new engine gives a new identity.
    pub allow_engine_change: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogPolicy {
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
}

impl Default for LogPolicy {
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// An explicit override wins over x-seedbox.seed. Absence of both is an error.
    pub seed: Option<u64>,
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Outcome {
    Pass,
    Fail,
    Error,
    Timeout,
    Incomplete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReplayStatus {
    Match,
    Mismatch,
    NotComparable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplayComparison {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_witnesses_match: Option<bool>,
    pub status: ReplayStatus,
    pub outcome_matches: bool,
    pub witness_matches: Option<bool>,
    pub full_state_matches: Option<bool>,
    pub fault_history_matches: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunIdentity {
    pub version: u32,
    pub seed: u64,
    pub configuration: Simulation,
    pub inputs: PreparedInputs,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_kind: Option<FailureKind>,
    pub run_id: ContentDigest,
    /// Distinct executions of the same identity never share output paths.
    pub attempt: String,
    pub identity: RunIdentity,
    pub performance_parameters: BTreeMap<String, String>,
    pub allow_engine_change: bool,
    pub log_policy: LogPolicy,
    pub omitted_logs: BTreeMap<String, String>,
    pub outcome: Outcome,
    pub report: Option<ExecutionReport>,
    pub error: Option<String>,
    pub logs: BTreeMap<String, ContentDigest>,
    pub replay_of: Option<String>,
    pub replay: Option<ReplayComparison>,
}

/// A checksum protects all recorded results as well as the input identity.
/// This detects corruption; it is not an authenticity signature.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    sha256: ContentDigest,
    record: RunRecord,
}

#[derive(Clone, Debug)]
pub struct Run {
    pub directory: PathBuf,
    pub record: RunRecord,
}

#[derive(Clone, Debug)]
pub struct Orchestrator {
    store: ContentStore,
    runs: PathBuf,
    log_policy: LogPolicy,
}

impl Orchestrator {
    pub fn new(store: ContentStore, runs: impl Into<PathBuf>) -> Self {
        Self {
            store,
            runs: runs.into(),
            log_policy: LogPolicy::default(),
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
        // Preparation always happens before any execution or host reservation.
        let inputs = preparer.prepare(simulation, &self.store)?;
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
        let mut identity = baseline.identity.clone();
        if options.allow_engine_change {
            identity.inputs.profile.engine = executor.profile().engine;
        }
        self.execute(
            identity,
            executor,
            &options.cancellation,
            Some(&baseline),
            options.allow_engine_change,
        )
    }

    fn execute(
        &self,
        identity: RunIdentity,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
        baseline: Option<&RunRecord>,
        allow_engine_change: bool,
    ) -> Result<Run> {
        let run_id = self.store.put(&serde_json::to_vec(&identity)?)?;
        fs::create_dir_all(&self.runs)?;
        let directory = tempfile::Builder::new()
            .prefix(&format!("{}-", &run_id.hex()[..16]))
            .tempdir_in(&self.runs)?
            .keep();
        let mut record = RunRecord {
            failure_kind: None,
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
        let execution = (|| -> Result<ExecutionReport> {
            validate_inputs(&record.identity, executor)?;
            let mut artifacts = BTreeMap::new();
            for digest in record.identity.inputs.digests() {
                if !artifacts.contains_key(digest) {
                    artifacts.insert(
                        digest.clone(),
                        self.store.materialize(digest, &directory.join("inputs"))?,
                    );
                }
            }
            fs::create_dir(&output)?;
            if cancellation.is_cancelled() {
                return Ok(ExecutionReport {
                    reason: StopReason::Cancelled,
                    workload_exit: None,
                    witness: None,
                    full_state: None,
                    faults: vec![],
                    diagnostics: None,
                    detail: Some("cancelled before execution".into()),
                });
            }
            let start = Instant::now();
            let mut report = executor.execute(ExecutionRequest {
                seed: record.identity.seed,
                limits: &record.identity.configuration.limits,
                network: &record.identity.configuration.network,
                schedule: &record.identity.configuration.schedule,
                check: &record.identity.configuration.check,
                prepared: &record.identity.inputs,
                artifacts: &artifacts,
                output: &output,
                cancellation,
                performance_parameters: &record.performance_parameters,
            })?;
            // A backend must enforce its own deadline so blocking calls terminate.
            // This additional guard prevents a late return from being a success.
            if cancellation.is_cancelled() {
                report.reason = StopReason::Cancelled;
            } else if start.elapsed().as_nanos()
                >= u128::from(record.identity.configuration.limits.wall_time_ns)
            {
                report.reason = StopReason::WallTimeLimit;
            }
            Ok(report)
        })();
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
                record.failure_kind = match &error {
                    Error::LeaseBusy(_) => Some(FailureKind::LeaseBusy),
                    Error::Overshoot(_) => Some(FailureKind::Overshoot),
                    _ => None,
                };
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
    let image_services = identity
        .configuration
        .services
        .iter()
        .filter_map(|(name, service)| {
            (!matches!(
                service.source,
                crate::config::ImageSource::BootArchive { .. }
            ))
            .then_some(name)
        });
    if image_services.ne(inputs.images.keys()) {
        return Err(Error::Integrity(
            "prepared images must match the OCI services".into(),
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
    for fault in &report.faults {
        if fault.applied_at_ns < fault.requested_at_ns
            || !simulation.services.contains_key(&fault.service)
        {
            return Err(Error::Execution(
                "fault effect must acknowledge a known service at or after its request time".into(),
            ));
        }
    }
    Ok(match report.reason {
        StopReason::SimulatedTimeLimit => Outcome::Timeout,
        StopReason::Error => {
            return Err(Error::Execution(
                report
                    .detail
                    .clone()
                    .unwrap_or("backend reported an error".into()),
            ))
        }
        StopReason::WallTimeLimit | StopReason::Cancelled | StopReason::Quiescent => {
            Outcome::Incomplete
        }
        StopReason::Completed => {
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
            if identity.inputs.profile.witness_mode == WitnessMode::FullState
                && report.full_state.is_none()
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
            if exit.code == simulation.check.exit_code {
                Outcome::Pass
            } else {
                Outcome::Fail
            }
        }
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
    let machine_witnesses_match = baseline
        .report
        .as_ref()
        .and_then(|r| r.diagnostics.as_ref())
        .map(|a| {
            fresh
                .report
                .as_ref()
                .and_then(|r| r.diagnostics.as_ref())
                .is_some_and(|b| a.machine_witnesses == b.machine_witnesses)
        });
    let fault_history_matches = fault_history_matches
        && baseline
            .report
            .as_ref()
            .and_then(|r| r.diagnostics.as_ref())
            .is_none_or(|a| {
                fresh
                    .report
                    .as_ref()
                    .and_then(|r| r.diagnostics.as_ref())
                    .is_some_and(|b| a.legacy_faults == b.legacy_faults)
            });
    let status = if !comparable {
        ReplayStatus::NotComparable
    } else if outcome_matches
        && machine_witnesses_match != Some(false)
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
        machine_witnesses_match,
        status,
        outcome_matches,
        witness_matches,
        full_state_matches,
        fault_history_matches,
    }
}

pub fn inspect(directory: &Path) -> Result<RunRecord> {
    let manifest: Manifest = serde_json::from_slice(&fs::read(directory.join("manifest.json"))?)?;
    let record = manifest.record;
    if ContentDigest::of(&serde_json::to_vec(&record)?) != manifest.sha256
        || ContentDigest::of(&serde_json::to_vec(&record.identity)?) != record.run_id
    {
        return Err(Error::Integrity("manifest checksum mismatch".into()));
    }
    if record.identity.version != FORMAT_VERSION {
        return Err(Error::Integrity("unsupported run format".into()));
    }
    Ok(record)
}

fn write_record(directory: &Path, record: &RunRecord) -> Result<()> {
    let manifest = Manifest {
        sha256: ContentDigest::of(&serde_json::to_vec(record)?),
        record: record.clone(),
    };
    let mut temp = tempfile::NamedTempFile::new_in(directory)?;
    temp.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    temp.as_file().sync_all()?;
    temp.persist(directory.join("manifest.json"))
        .map_err(|e| e.error)?;
    Ok(())
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
                record.logs.insert(relative, store.put_file(&path)?);
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    LeaseBusy,
    Overshoot,
}
