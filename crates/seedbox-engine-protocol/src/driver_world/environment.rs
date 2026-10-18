//! Per-name factory authority, separate from MachineDefinition and Driver state.
//! Config bytes remain content artifacts rather than JSON byte arrays.
use super::Artifact;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, num::NonZeroU32};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NamedVolume {
    pub name: String,
    pub target: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestLimits {
    pub max_map_count: NonZeroU32,
    pub nofile: NonZeroU32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Environment {
    pub hostname: String,
    pub hosts: String,
    pub anonymous_volumes: Vec<String>,
    pub named_volumes: Vec<NamedVolume>,
    pub configs: BTreeMap<String, Artifact>,
    pub limits: GuestLimits,
    pub control_socket: bool,
}
