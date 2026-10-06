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

const FORMAT_VERSION: u32 = 1;

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
    pub run_id: ContentDigest,
    /// Distinct executions of the same identity never share output paths.
    pub attempt: String,
    pub identity: RunIdentity,
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
}

impl Orchestrator {
    pub fn new(store: ContentStore, runs: impl Into<PathBuf>) -> Self {
        Self {
            store,
            runs: runs.into(),
        }
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
        self.execute(identity, executor, &options.cancellation, None)
    }

    /// Fresh execution with the original seed and retained artifacts. There is
    /// intentionally no preparation backend argument: replay cannot rebuild.
    pub fn replay(
        &self,
        directory: &Path,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
    ) -> Result<Run> {
        let baseline = inspect(directory)?;
        self.execute(
            baseline.identity.clone(),
            executor,
            cancellation,
            Some(&baseline),
        )
    }

    fn execute(
        &self,
        identity: RunIdentity,
        executor: &dyn ExecutionBackend,
        cancellation: &CancellationToken,
        baseline: Option<&RunRecord>,
    ) -> Result<Run> {
        let run_id = self.store.put(&serde_json::to_vec(&identity)?)?;
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
                    detail: Some("cancelled before execution".into()),
                });
            }
            let start = Instant::now();
            let mut report = executor.execute(ExecutionRequest {
                seed: record.identity.seed,
                limits: &record.identity.configuration.limits,
                network: &record.identity.configuration.network,
                check: &record.identity.configuration.check,
                prepared: &record.identity.inputs,
                artifacts: &artifacts,
                output: &output,
                cancellation,
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
                record.outcome = Outcome::Error;
                record.error = Some(error.to_string());
            }
        }
        // Preserve backend logs even when execute returned an error.
        if output.symlink_metadata().is_ok() {
            if let Err(error) = retain_logs(&output, &output, &self.store, &mut record.logs) {
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
    Ok(match report.reason {
        StopReason::SimulatedTimeLimit | StopReason::WallTimeLimit => Outcome::Timeout,
        StopReason::Error => {
            return Err(Error::Execution(
                report
                    .detail
                    .clone()
                    .unwrap_or("backend reported an error".into()),
            ))
        }
        StopReason::Cancelled | StopReason::Quiescent => Outcome::Incomplete,
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
                if state.keys().ne(simulation.services.keys()) {
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
    let comparable = matches!(baseline.outcome, Outcome::Pass | Outcome::Fail)
        && matches!(fresh.outcome, Outcome::Pass | Outcome::Fail);
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
    logs: &mut BTreeMap<String, ContentDigest>,
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
            retain_logs(base, &path, store, logs)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_str()
                .ok_or_else(|| Error::Integrity("non-UTF-8 log path".into()))?
                .to_owned();
            logs.insert(relative, store.put_file(&path)?);
        } else {
            return Err(Error::Integrity(format!(
                "backend output is not a regular file/directory: {}",
                path.display()
            )));
        }
    }
    Ok(())
}
