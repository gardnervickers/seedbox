//! Explicit retained worker inputs, materialized by the application adapter.
//! Host paths appear only at this private launch boundary. Factories never
//! discover assets through environment variables or invoke builders.
use super::{Artifact, Inventory};
use crate::input_program::TimedInput;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: String,
    pub user: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Disk {
    Zero { bytes: u64 },
    Image { contents: Artifact },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Interface {
    pub mac: [u8; 6],
    pub address: std::net::Ipv4Addr,
    pub prefix_len: u8,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    pub name: String,
    pub memory_bytes: u64,
    pub filesystem: Artifact,
    pub process: Process,
    pub disk: Option<Disk>,
    pub interface: Option<Interface>,
    /// Crashes this machine is designed to have; they pass the default
    /// `seedbox.no_crashing_processes` property instead of violating it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_crashes: Vec<seedbox_spec::project::ExpectedCrash>,
    /// Out-of-memory kills here pass `seedbox.no_out_of_memory_kills`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub expected_out_of_memory_kills: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    pub bandwidth_bps: Option<u64>,
    pub queue_bound_ns: Option<u64>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u32,
    #[serde(default)]
    pub bandwidth_bps: Option<u64>,
    #[serde(default)]
    pub queue_bound_ns: Option<u64>,
    #[serde(default)]
    pub links: Vec<Link>,
}
/// Retained preparation output. Bootstrap authority and future definitions are
/// supplied for fresh construction; restored Driver definitions remain owned by
/// the closed Driver checkpoint and are not recreated from observations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub inventory: Inventory,
    pub oracle: Machine,
    pub machines: Vec<Machine>,
    pub spawnable: Vec<Machine>,
    pub quantum_ns: u64,
    pub network: Network,
}
/// Private launch-only binding to a matching immutable runtime already owned by
/// the parent. The parent retains its Inputs guard until this worker and all
/// native descendants have stopped. This is never checkpoint data; a retained
/// archive with different content must be acquired independently.
// Process boundaries cannot transfer an Arc; explicit paths
// borrow the parent session lifetime rather than re-extracting the same package.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeBinding {
    pub archive: Artifact,
    pub guest_assets: PathBuf,
    pub qemu: Option<PathBuf>,
}
impl RuntimeBinding {
    /// Require the same content identity and length as retained factory authority.
    /// Paths may differ between sessions; a different archive is never substituted.
    pub fn validate_archive(&self, archive: &Artifact) -> Result<(), String> {
        if self.archive.sha256 != archive.sha256 || self.archive.length != archive.length {
            return Err("shared runtime differs from the retained factory archive; acquire that retained archive rather than the startup/default runtime".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub prepared: Prepared,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_binding: Option<RuntimeBinding>,
    pub seed: u64,
    pub program: Vec<TimedInput>,
    pub simulated_time_ns: u64,
    pub output: PathBuf,
    pub machine_workers: usize,
    pub start_paused: bool,
    pub full_state: bool,
    pub host_control: PathBuf,
    pub observations: Option<PathBuf>,
    pub resume_checkpoint: Option<PathBuf>,
}
