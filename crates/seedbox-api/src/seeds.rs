//! Seed-range submission and its member simulations; no separate execution resource.
use crate::{SimulationId, SimulationSpec, SimulationSummary};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeedRangeRequest {
    #[serde(default)]
    pub options: crate::ExecutionOptions,
    #[serde(default)]
    pub metadata: seedbox_spec::definition::Metadata,
    pub request_id: String,
    pub spec: SimulationSpec,
    pub count: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SeedRangeStatus {
    pub source: SimulationId,
    pub first_seed: u64,
    pub count: u32,
    pub members: Vec<SimulationSummary>,
    /// Members explicitly deleted after the range completed.
    #[serde(default)]
    pub deleted_members: u32,
    pub stop_requested: bool,
    pub error: Option<String>,
}
impl SeedRangeStatus {
    pub fn complete(&self) -> bool {
        (self.members.len() + self.deleted_members as usize == self.count as usize
            || self.stop_requested
            || self.error.is_some())
            && self
                .members
                .iter()
                .all(|member| member.status.phase == crate::Phase::Completed)
    }
}
