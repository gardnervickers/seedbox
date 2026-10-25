use crate::{ApiVersion, Metadata, SimulationId, SimulationStatus};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
    /// The request is well formed, but no server or engine carries it out
    /// yet (HTTP 501).
    NotImplemented,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub api_version: ApiVersion,
    /// Diagnostic process identity for the local server, never authorization.
    pub process_id: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationSummary {
    pub metadata: Metadata,
    pub status: SimulationStatus,
    pub oracle: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SimulationList {
    pub items: Vec<SimulationSummary>,
    pub next_cursor: Option<SimulationId>,
}
