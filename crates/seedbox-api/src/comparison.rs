use crate::SimulationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ComparisonStatus {
    Match,
    Mismatch,
    NotComparable,
}

/// Determinism evidence is separate from either simulation's oracle verdict.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulationComparison {
    pub baseline: SimulationId,
    pub simulation: SimulationId,
    pub status: ComparisonStatus,
    pub outcome_matches: bool,
    pub witness_matches: Option<bool>,
    pub full_state_matches: Option<bool>,
    pub fault_history_matches: bool,
}
