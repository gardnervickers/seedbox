//! Counterfactual analysis of a failed simulation: how often its failure
//! recurs when its later draws or one of its faults change. Rates describe
//! runs of the source's program under derived seeds, never a proven cause.
use crate::{CampaignPhase, ExecutionOptions, SimulationId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalysisRequest {
    pub request_id: String,
    /// Runs per arm, each under its own derived seeds.
    pub samples: u32,
    /// Time arms, evenly spaced before the source ended.
    pub times: u32,
    /// Add an arm per external fault, run without it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub faults: bool,
    /// The property whose violation counts; the oracle failure when absent
    /// and the source failed its oracle, else its first violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    pub options: ExecutionOptions,
}

/// The most runs per arm.
pub const MAX_ANALYSIS_SAMPLES: u32 = 64;
/// The most time arms.
pub const MAX_ANALYSIS_TIMES: u32 = 32;
/// The most runs one analysis may submit.
pub const MAX_ANALYSIS_RUNS: u32 = 512;

/// What an arm changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AnalysisArmKind {
    /// Every draw varies.
    Baseline,
    /// Draws before `atNs` stay the source's; every later draw varies.
    #[serde(rename_all = "camelCase")]
    ReseedFrom { at_ns: u64 },
    /// One external fault is removed, with its teardown and heals; every
    /// draw varies.
    #[serde(rename_all = "camelCase")]
    Without { keys: Vec<String>, summary: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AnalysisArm {
    pub kind: AnalysisArmKind,
    /// Its runs under derived seeds.
    pub runs: Vec<SimulationId>,
    /// Runs that completed with a verdict.
    pub completed: u32,
    /// Completed runs that failed the same way as the source.
    pub failed: u32,
    /// Completed runs that failed another way.
    pub other: u32,
    /// Runs with no verdict: incomplete evidence or no completion.
    pub incomplete: u32,
    /// 95% Wilson interval of `failed` of `completed`, in per mille.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_per_mille: Option<[u32; 2]>,
    /// A fault arm's run under the source's own seeds: the source minus
    /// that fault and nothing else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact: Option<SimulationId>,
    /// Whether the exact run failed the same way, once it completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_failed: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Analysis {
    pub id: String,
    pub source: SimulationId,
    /// What counts as the same failure: a property ID or `oracle`.
    pub target: String,
    pub phase: CampaignPhase,
    /// The source's simulated end time.
    pub end_ns: u64,
    pub arms: Vec<AnalysisArm>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Rerun a completed simulation from a fresh start with one command run in a
/// machine at a moment: the run is the source's up to then, so the command
/// observes the state the source had.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecRequest {
    pub request_id: String,
    pub machine: String,
    pub at_ns: u64,
    pub argv: Vec<String>,
    pub options: ExecutionOptions,
}
