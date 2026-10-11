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
    pub title: String,
    pub description: String,
    /// JSON Schema draft 2020-12 for a complete action object. `x-unit`
    /// annotates physical units without changing JSON Schema validation.
    pub schema: serde_json::Value,
}
