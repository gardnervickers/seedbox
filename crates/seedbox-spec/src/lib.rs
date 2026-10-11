//! Resolved configuration retained in run identities.
pub mod definition;
pub mod project;

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    pub service: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Simulation {
    pub name: String,
    pub source: PathBuf,
    pub services: BTreeMap<String, Service>,
    pub configs: BTreeMap<String, PathBuf>,
    pub network: Network,
    /// PV instrumentation semantics retained with the simulation.
    #[serde(default)]
    pub simulator: project::SimulatorDefaults,
    pub schedule: Schedule,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<Control>,
    pub limits: Limits,
    pub check: Check,
    pub seed: Option<u64>,
    /// Guest CLOCK_REALTIME at launch, in Unix seconds; None derives it from
    /// the prepared images' build times.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_epoch: Option<u32>,
    #[serde(default, skip_serializing_if = "MachineOptions::is_default")]
    pub machine: MachineOptions,
    #[serde(default, skip_serializing_if = "DiskOptions::is_default")]
    pub disk: DiskOptions,
}

/// Guest machine settings shared by every service; they change run identity and results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineOptions {
    /// Guest RAM per service, in MiB.
    pub memory_mb: u32,
    /// Arm clockevents through the time page (ABI 4, aimed at PV; Linux KVM only).
    pub page_timer: bool,
    /// Defer virtio-net TX kicks to deterministic polls (Linux KVM only).
    pub deferred_tx: bool,
}

impl Default for MachineOptions {
    fn default() -> Self {
        Self {
            memory_mb: 1024,
            page_timer: false,
            deferred_tx: false,
        }
    }
}

impl MachineOptions {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Disk crash semantics; they change run identity and results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskOptions {
    /// Keep a seeded subset of unflushed 512-byte sectors on crash.
    pub torn_writes: bool,
}

impl DiskOptions {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub source: ImageSource,
    pub platform: String,
    /// None inherits image defaults; Some([]) explicitly clears them.
    pub entrypoint: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    /// None removes an image variable when passthrough was not supplied.
    pub environment: BTreeMap<String, Option<String>>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub configs: Vec<ConfigMount>,
    /// Named volumes, kept on the service machine's retained disk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeMount>,
    pub network: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ImageSource {
    Registry {
        reference: String,
    },
    Build {
        context: PathBuf,
        dockerfile: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigMount {
    pub source: String,
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMount {
    /// A top-level `volumes` name.
    pub source: String,
    /// Absolute directory in the service's filesystem.
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub name: String,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<Link>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub simulated_time_ns: u64,
    pub wall_time_ns: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub timer_jitter_ns: u64,
    pub stop_tolerance_ns: u64,
    /// Bound on the seeded extra charge per PV progress call; zero is off.
    /// Authored as `simulator.progressJitter` in seedbox.yaml.
    #[serde(default, skip_serializing_if = "crate::project::is_zero")]
    pub progress_jitter_ns: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub service: String,
    pub exit_code: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn machine_defaults_remain_ordinary_configuration() {
        assert!(MachineOptions::default().is_default());
        assert!(DiskOptions::default().is_default());
        assert!(!MachineOptions {
            memory_mb: 512,
            ..Default::default()
        }
        .is_default());
    }
}
