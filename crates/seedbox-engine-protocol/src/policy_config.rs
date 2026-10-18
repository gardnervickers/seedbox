//! Network, control and checkpoint configuration shared by fault validation,
//! the canonical link policies and retained evidence.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const DEFAULT_LATENCY_NS: u64 = 1_000_000;

fn default_latency_ns() -> u64 {
    DEFAULT_LATENCY_NS
}

/// The global network policy and per-pair baseline overrides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default = "default_latency_ns")]
    pub latency_ns: u64,
    #[serde(default)]
    pub jitter_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
    /// Absent unless used, so a world without overrides is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkConfig>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            latency_ns: DEFAULT_LATENCY_NS,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: Vec::new(),
        }
    }
}

/// A directed baseline policy between two machines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkConfig {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    #[serde(default)]
    pub jitter_ns: u64,
    #[serde(default)]
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
}

/// The runtime controller: its machine, its reply latency and the service
/// placement that resolves service targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    pub machine: String,
    pub latency_ns: u64,
    pub placement: BTreeMap<String, String>,
}

/// The guest transport that carries runtime control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlChannel {
    #[serde(rename = "virtio-console-v1")]
    VirtioConsoleV1,
}

/// A runtime checkpoint and the branch run from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    pub at_ns: u64,
    pub branch_seed: u64,
    /// When present, the engine checks it against its own inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_identity: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_defaults_fill_omitted_keys_and_links_are_strict() {
        let network: NetworkConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(network, NetworkConfig::default());
        assert_eq!(
            serde_json::to_value(&network).unwrap()["latency_ns"],
            1_000_000
        );
        assert!(serde_json::to_value(&network)
            .unwrap()
            .get("links")
            .is_none());
        assert!(serde_json::from_str::<NetworkConfig>(
            r#"{"links":[{"from":"a","to":"b","latency_ns":1,"extra":0}]}"#
        )
        .is_err());
    }
}
