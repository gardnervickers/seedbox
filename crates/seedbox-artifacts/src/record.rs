//! Retained run records, identities and manifest persistence.
use crate::{config::Simulation, execution::*, store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};
pub const FORMAT_VERSION: u32 = 2;
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
    /// Authored inputs; fault IDs are assigned only when admitted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub program: Vec<seedbox_engine_protocol::input_program::TimedInput>,
    /// Content-addressed semantic history, sealed after interactive execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interventions: Option<ContentDigest>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run_id: ContentDigest,
    /// Distinct executions of the same identity never share output paths.
    pub attempt: String,
    pub identity: RunIdentity,
    pub performance_parameters: BTreeMap<String, String>,
    pub log_policy: LogPolicy,
    pub omitted_logs: BTreeMap<String, String>,
    pub outcome: Outcome,
    pub report: Option<ExecutionReport>,
    pub error: Option<String>,
    pub logs: BTreeMap<String, ContentDigest>,
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

pub fn inspect(directory: &Path) -> Result<RunRecord> {
    inspect_bytes(&fs::read(directory.join("manifest.json"))?)
}

/// Validate a retained manifest without consulting its original directory.
pub fn inspect_bytes(bytes: &[u8]) -> Result<RunRecord> {
    let manifest: Manifest = serde_json::from_slice(bytes)?;
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

pub fn write_record(directory: &Path, record: &RunRecord) -> Result<()> {
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

/// Compare retained result evidence; callers separately establish input compatibility.
pub fn compare(baseline: &RunRecord, fresh: &RunRecord) -> ReplayComparison {
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
    }
}
