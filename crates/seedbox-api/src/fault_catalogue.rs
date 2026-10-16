//! Server-owned fault forms. Schemas describe the action object accepted by
//! `injectFault`; clients need no compiled list of action variants.
use crate::SimulationId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultCatalogue {
    pub simulation: SimulationId,
    pub faults: Vec<FaultDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultDescriptor {
    pub name: String,
    pub category: FaultCategory,
    pub title: String,
    pub description: String,
    /// JSON Schema draft 2020-12 for a complete action object. `x-unit`
    /// annotates physical units without changing JSON Schema validation.
    pub schema: serde_json::Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FaultCategory {
    Network,
    Machine,
    Application,
    Clock,
    Storage,
    /// Heal can restore a network, machine, clock or storage policy.
    Recovery,
}

impl FaultCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Network => "Network",
            Self::Machine => "Machine",
            Self::Application => "Application",
            Self::Clock => "Clock",
            Self::Storage => "Storage",
            Self::Recovery => "Recovery",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultPreviewRequest {
    pub action: crate::FaultAction,
}

/// Expected targets from a server observation, never an admission guarantee.
/// Preview neither prepares a fault nor allocates a fault ID. Submission is
/// independently validated against the engine state at its boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultPreview {
    pub simulation: SimulationId,
    pub revision: u64,
    pub simulated_time_ns: u64,
    pub category: FaultCategory,
    pub machines: Vec<String>,
    pub links: Vec<FaultPreviewLink>,
    pub expected_effect: String,
    /// Human-readable facts from the observation; unknown state stays explicit.
    pub observations: Vec<String>,
    pub preconditions: Vec<String>,
    pub limitations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultPreviewLink {
    pub from: String,
    pub to: String,
}
