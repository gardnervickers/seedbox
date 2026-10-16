//! Guided campaigns, their findings, and input shrinking. A campaign plans
//! ordinary member simulations; its budgets are request options, never part
//! of a simulation definition.
use crate::{ExecutionOptions, SimulationId, SimulationSpec};
use serde::{Deserialize, Serialize};

/// A campaign's logical plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignPlan {
    /// Seeds the planner's choices.
    pub seed: u64,
    /// Member simulations in total.
    pub budget: u32,
    /// Members per generation; each generation folds before the next.
    pub batch: u32,
    /// Share of later generations that start fresh roots, per mille.
    pub fresh_permille: u32,
    /// Members kept as branch sources.
    pub corpus_limit: u32,
    /// Candidate anchor times every root declares, ascending.
    pub anchors_ns: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignRequest {
    pub request_id: String,
    pub metadata: seedbox_spec::definition::Metadata,
    /// The root simulation; roots take consecutive seeds from its seed.
    pub spec: SimulationSpec,
    /// Applied to every member.
    pub options: ExecutionOptions,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_profile: Option<seedbox_spec::fault_profile::FaultProfile>,
    pub plan: CampaignPlan,
    /// Detail fields that split one property's findings into groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<String>,
    /// Where findings are exported on the server host; defaults to the
    /// state directory's `findings/<campaign>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_dir: Option<String>,
    /// An earlier campaign's findings directory to label findings against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CampaignPhase {
    Running,
    Completed,
}

/// Why a campaign ended. Exhausting the logical budget is success whatever
/// its members found; cancellation and failure are not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "camelCase", deny_unknown_fields)]
pub enum CampaignCompletion {
    Exhausted,
    Cancelled,
    Failed { message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum MemberPlan {
    Root {
        seed: u64,
        /// The fault profile entries a swarm root kept, when it sampled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kinds: Option<Vec<u32>>,
    },
    Branch {
        parent: u32,
        anchor: String,
        reseed: u64,
        choices: u32,
        /// The fault it placed after something its parent did, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mutation: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignMember {
    pub ordinal: u32,
    pub generation: u32,
    pub plan: MemberPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<SimulationId>,
    /// The member's verdict once complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<crate::TestVerdict>,
    /// Features no earlier member showed.
    pub novelty: u32,
    pub in_corpus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One property across the campaign. Counts are each member's own
/// evaluations: a branch does not count again what it inherited from its
/// ancestors' prefix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignProperty {
    pub id: String,
    pub kind: seedbox_engine_protocol::property::Kind,
    pub status: seedbox_engine_protocol::property::PropertyStatus,
    pub passes: u64,
    pub failures: u64,
    /// Members that evaluated it themselves.
    pub members_evaluated: u32,
    /// Members whose own evaluations violated it. With the completed members,
    /// this is the observed campaign rate, not a population estimate.
    pub members_violating: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignAssessment {
    pub status: seedbox_engine_protocol::property::AssessmentStatus,
    pub members_completed: u32,
    /// Members whose own verdict was an error, timeout or incomplete.
    pub members_incomplete: u32,
    pub members_failed: u32,
    pub properties: Vec<CampaignProperty>,
}

/// A deterministic position: which member, machine and incarnation, and
/// the simulated time a record was committed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Moment {
    pub ordinal: u32,
    pub simulation: SimulationId,
    pub machine: String,
    pub incarnation: u32,
    pub time_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppliedFault {
    pub action: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub machine: String,
    pub applied_at_ns: u64,
}

/// Against an explicitly selected baseline's findings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingLabel {
    New,
    ObservedAgain,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub enum ExportState {
    Pending,
    /// Synced to its directory: durable.
    Published {
        path: String,
    },
    Failed {
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Finding {
    /// The property ID (or `oracle`) and any grouping detail values.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<FindingLabel>,
    /// The earliest member, by ordinal, that showed it.
    pub representative: SimulationId,
    pub ordinal: u32,
    /// Ancestor ordinals from the root to the representative's parent.
    pub ancestry: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failing: Option<Moment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passing: Option<Moment>,
    /// Every member that showed it.
    pub members: Vec<u32>,
    pub faults: Vec<AppliedFault>,
    /// The representative's evidence was incomplete.
    pub evidence_incomplete: bool,
    pub export: ExportState,
    pub reproduction: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Campaign {
    pub id: String,
    pub name: String,
    pub created_at_unix_ms: u64,
    pub plan: CampaignPlan,
    pub phase: CampaignPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CampaignCompletion>,
    pub generations: u32,
    pub features: u32,
    pub corpus: u32,
    pub members: Vec<CampaignMember>,
    pub assessment: CampaignAssessment,
    pub findings: Vec<Finding>,
    pub findings_dir: String,
    /// Baseline groups this campaign did not observe.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_observed: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignSummary {
    pub id: String,
    pub name: String,
    pub phase: CampaignPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<CampaignCompletion>,
    pub budget: u32,
    pub completed: u32,
    pub findings: u32,
}

/// One member's own violation of one group, for queries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Encounter {
    pub group: String,
    pub moment: Moment,
    pub ancestry: Vec<u32>,
    pub faults: Vec<AppliedFault>,
    /// False when the member's evidence was incomplete, so earlier history
    /// may be missing.
    pub history_complete: bool,
}

/// Filters for a campaign's encounters; every given filter must hold.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FindingQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    /// The failing machine or a fault's target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    /// An applied fault action anywhere in the member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_ns: Option<u64>,
    /// A member ordinal that must be an ancestor (or the member itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ancestor: Option<u32>,
    /// A fault action applied within `within_ns` before the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preceded_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub within_ns: Option<u64>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EncounterPage {
    pub encounters: Vec<Encounter>,
    pub total: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    /// Some member's evidence was incomplete or still running.
    pub incomplete: bool,
}

pub const MAX_PAGE: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShrinkRequest {
    pub request_id: String,
    /// Probes the shrink may run.
    pub max_probes: u32,
    /// The property whose violation must persist; the oracle failure when
    /// absent and the source failed its oracle, else its first violation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    pub options: ExecutionOptions,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ShrinkProbe {
    pub simulation: SimulationId,
    pub inputs: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduced: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ShrinkOutcome {
    /// The unchanged program did not fail the same way.
    NotReproduced,
    /// Smaller, but the probe budget ran out before 1-minimality.
    Reduced,
    /// 1-minimal under these transformations: no single remaining input
    /// (with its dependents) can be removed and still reproduce the failure.
    Minimal,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Shrink {
    pub id: String,
    pub source: SimulationId,
    /// What must persist: a property ID or `oracle`.
    pub target: String,
    pub phase: CampaignPhase,
    pub inputs: u32,
    pub removable: u32,
    pub probes: Vec<ShrinkProbe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ShrinkOutcome>,
    /// The kept input keys, in program order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    /// The probe that ran exactly the kept inputs and still failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reproduction: Option<SimulationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
