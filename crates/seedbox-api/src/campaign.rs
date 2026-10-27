//! Campaigns: a seed range whose later members a planner chooses from the
//! results of earlier ones.
//!
//! Generation 0 is a seed range of roots: ordinary simulations that take
//! consecutive seeds from the spec's seed and declare the same anchor
//! inputs, keyed `campaign-anchor-<n>` in time order. Each later generation is planned only once the whole previous one
//! has completed, from every result so far in member order, so plans never
//! depend on execution capacity or completion order. A branch continues a
//! member from one of its anchor captures with a reseed, and may select
//! choices or place one fault after something its parent did. Like a seed
//! range, a campaign is not a separate execution resource: it is addressed
//! by its first root's simulation ID, and branched members name their parent
//! and checkpoint in their own metadata. Budgets are request options, never
//! part of a definition. No server runs campaigns yet: submission is refused.
//!
//! `POST /v1/campaigns` takes a [`CampaignRequest`] and returns its
//! [`CampaignStatus`]; `GET /v1/simulations/{id}/campaign` reads it and
//! `POST /v1/simulations/{id}/campaign/stop` stops planning and every
//! running member.
use crate::{ExecutionOptions, SimulationId, SimulationSpec, TestVerdict};
pub use seedbox_engine_protocol::input_program::Mutation;
use seedbox_engine_protocol::{
    fault_profile::FaultProfile, input_program::ChoiceOverride, property::Origin,
};
use serde::{Deserialize, Serialize};

/// Members a campaign may run, as many as a seed range.
pub const MAX_CAMPAIGN_MEMBERS: u32 = 4096;
/// Members per generation.
pub const MAX_BATCH: u32 = 64;
/// Candidate anchor times every root declares.
pub const MAX_ANCHORS: usize = 16;
/// Members kept as branch sources, each with its anchor captures.
pub const MAX_CORPUS: u32 = 1024;

/// The logical plan. Results depend on these and the seeds, never on how
/// many members run at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignPlan {
    /// Seeds the planner's own choices; the first root's seed when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planner_seed: Option<u64>,
    /// Members in total.
    pub budget: u32,
    /// Members per generation.
    pub batch: u32,
    /// Share of each later generation that starts fresh roots, per mille.
    pub fresh_permille: u32,
    /// Members kept as branch sources.
    pub corpus_limit: u32,
    /// Candidate anchor times every root declares, strictly increasing.
    pub anchors_ns: Vec<u64>,
}

impl CampaignPlan {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_CAMPAIGN_MEMBERS).contains(&self.budget) {
            return Err(format!(
                "campaign budget must be 1 to {MAX_CAMPAIGN_MEMBERS} members"
            ));
        }
        if !(1..=MAX_BATCH).contains(&self.batch) {
            return Err(format!("campaign batch must be 1 to {MAX_BATCH} members"));
        }
        if self.fresh_permille > 1000 {
            return Err("campaign fresh share is at most 1000 per mille".into());
        }
        if !(1..=MAX_CORPUS).contains(&self.corpus_limit) {
            return Err(format!("campaign corpus holds 1 to {MAX_CORPUS} members"));
        }
        if self.anchors_ns.len() > MAX_ANCHORS || self.anchors_ns.windows(2).any(|w| w[0] >= w[1]) {
            return Err(format!(
                "campaign anchors are at most {MAX_ANCHORS} strictly increasing times"
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignRequest {
    /// Applied to every member.
    #[serde(default)]
    pub options: ExecutionOptions,
    #[serde(default)]
    pub metadata: seedbox_spec::definition::Metadata,
    pub request_id: String,
    /// The roots' simulation; roots take consecutive seeds from its seed.
    pub spec: SimulationSpec,
    /// Faults to generate into each root's program with its own seed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_profile: Option<FaultProfile>,
    pub plan: CampaignPlan,
    /// Property detail fields that split one property's findings into groups.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<String>,
    /// An earlier campaign on this server, by its first root, whose findings
    /// label these as new or observed again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<SimulationId>,
}

/// What a member runs. Every plan is a complete recipe: its program follows
/// from the plan and its ancestors' programs alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MemberPlan {
    /// A fresh simulation with this seed and the plan's anchors.
    Root {
        seed: u64,
        /// The fault profile entries, by index, this root samples from
        /// (swarm testing); all of them when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile_entries: Option<Vec<u32>>,
    },
    /// Continue member `parent` from its capture at anchor input `anchor`,
    /// reseeded, with choice inputs for occurrences after the anchor and at
    /// most one mutation. Its added inputs take the campaign's reserved keys
    /// (`campaign-reseed-<m>`, `campaign-choice-<m>-<j>`, `mutate-<m>`). A
    /// lineage holds at most `MAX_TRIGGERS / MUTATION_TRIGGERS` mutations,
    /// less the program's own triggers.
    Branch {
        parent: u32,
        anchor: String,
        reseed: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        choices: Vec<ChoiceOverride>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mutation: Option<Box<Mutation>>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignMember {
    /// Assigned in plan order before dispatch.
    pub ordinal: u32,
    pub generation: u32,
    pub plan: MemberPlan,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulation: Option<SimulationId>,
    /// The member's verdict once complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<TestVerdict>,
    /// Features no earlier member, in ordinal order, showed.
    pub novelty: u32,
    pub in_corpus: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One property that some member violated or required without evaluating.
/// Runs keep only property failures, so a property every member held is
/// absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignProperty {
    pub id: String,
    /// Members whose own evaluations violated it: with the completed members,
    /// the observed campaign rate, not a population estimate.
    pub members_violating: u32,
    /// Members that required it and never evaluated it.
    pub members_unmet: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignAssessment {
    pub status: crate::PropertyAssessmentStatus,
    pub members_completed: u32,
    /// Members whose own verdict was incomplete.
    pub members_incomplete: u32,
    pub members_failed: u32,
    pub properties: Vec<CampaignProperty>,
}

/// Against the baseline campaign's findings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FindingLabel {
    New,
    ObservedAgain,
}

/// One kind of failure: a violated property (or `oracle`), split by any
/// `groupBy` detail values. Its representative is the earliest member, by
/// ordinal, that showed it; reproduce it with that simulation's own inputs
/// (`seedbox checkpoint --cold` for a branch).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Finding {
    /// The property ID (or `oracle`) and any grouping values.
    pub group: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub property: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<FindingLabel>,
    pub representative: SimulationId,
    pub ordinal: u32,
    /// Ordinals from the root to the representative's parent.
    pub ancestry: Vec<u32>,
    /// Every member that showed it.
    pub members: Vec<u32>,
    /// Where the representative first failed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_failure: Option<Origin>,
    /// The representative's property evidence was incomplete.
    pub evidence_incomplete: bool,
}

/// A campaign as its first root's range, like a seed range's status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CampaignStatus {
    /// The first root.
    pub source: SimulationId,
    pub plan: CampaignPlan,
    /// Generations planned so far.
    pub generations: u32,
    /// Distinct features seen so far.
    pub features: u32,
    pub corpus: u32,
    pub members: Vec<CampaignMember>,
    pub assessment: CampaignAssessment,
    pub findings: Vec<Finding>,
    /// Baseline groups this campaign did not show.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub not_observed: Vec<String>,
    pub stop_requested: bool,
    pub error: Option<String>,
}

impl CampaignStatus {
    /// The budget is spent, a stop was requested or planning failed, and
    /// every member that started has completed.
    pub fn complete(&self) -> bool {
        (self.members.len() as u32 == self.plan.budget
            || self.stop_requested
            || self.error.is_some())
            && self.members.iter().all(|member| {
                member.error.is_some()
                    || member
                        .verdict
                        .is_some_and(|verdict| verdict != TestVerdict::Pending)
            })
    }
}

#[cfg(test)]
mod tests;
