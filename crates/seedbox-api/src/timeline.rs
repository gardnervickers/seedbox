//! Public timeline presentation. Action payloads are interpreted by the server.
use crate::{BoundaryPosition, SimulationId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TimelineOrigin {
    Recorded,
    Program,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimelineEntry {
    pub id: String,
    pub at_ns: u64,
    pub action: Value,
    pub origin: TimelineOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<BoundaryPosition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_id: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultTimeline {
    pub simulation: SimulationId,
    pub entries: Vec<TimelineEntry>,
}

/// Select source entries without reconstructing the server's execution inputs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeriveRequest {
    pub request_id: String,
    pub source_created_at_unix_ms: u64,
    pub included: Vec<String>,
    pub start_paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presentation_keeps_unknown_action_payloads() {
        let json = serde_json::json!({"simulation":"k7m2p9qx","entries":[{"id":"program:future","atNs":42,"origin":"program","action":{"action":"future_action","novel":{"value":true}}}]});
        let timeline: FaultTimeline = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(serde_json::to_value(timeline).unwrap(), json);
    }
}
