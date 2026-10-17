//! Retained service continuation for the `driver-world-v1` executable contract.
//!
//! Driver and native bodies are separate closed binary artifacts. This envelope
//! owns only service state and external factory authority; a digest establishes
//! integrity, not provenance or permission to execute an imported native image.
use crate::{
    host_control::BoundaryPosition,
    input_program::{InputAdmission, InputRejection, TimedInput},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
};

mod environment;
pub use environment::{Environment, GuestLimits, NamedVolume};
mod request;
pub use request::{
    Disk, Interface, Link, Machine, Network, Prepared, Process, Request, RuntimeBinding,
};
#[cfg(test)]
mod tests;

mod policy;
pub use policy::BackendPolicy;

pub const CONTRACT: &str = "driver-world-v1";
pub const ENVELOPE_VERSION: u32 = 3;
pub const MAX_ENVELOPE_BYTES: u64 = 16 * 1024 * 1024;

/// A materialized, immutable artifact. Paths are service-supplied authority,
/// never a field encoded inside a backend machine snapshot. Export replaces
/// `artifact` with `asset:...` through the existing Start capture machinery.
// TODO: Re-evaluate: the existing Start walker recognizes sha256 + artifact;
// retaining that shape avoids a second export or content-store protocol.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub sha256: String,
    pub length: u64,
    pub artifact: String,
}
impl Artifact {
    pub fn validate(&self) -> Result<(), String> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.artifact.is_empty()
            || self.artifact.len() > 4096
            || self.artifact.contains('\0')
        {
            return Err("invalid retained Driver artifact reference".into());
        }
        Ok(())
    }
}

/// Exact bootstrap authority for stateless future machine factories. A worker
/// executable/runtime closure is retained independently of native state bodies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "backend", rename_all = "snake_case", deny_unknown_fields)]
pub enum Inventory {
    Pv {
        /// Exact optional PV semantics, validated by the application/native codec.
        /// Older prepared inputs omit this and retain None, not Some(default).
        #[serde(default)]
        policy: Option<BackendPolicy>,
        executable: Artifact,
        kernel: Artifact,
        init: Artifact,
        command_line: Artifact,
        progress: Artifact,
        runtime: Artifact,
        formatter: Option<Artifact>,
        environments: BTreeMap<String, Environment>,
        /// Complete Linux boot bytes override paired application packaging.
        /// Missing in older retained worlds; those import the current boot.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        boot_archives: BTreeMap<String, Artifact>,
        blocks_per_progress_call: NonZeroU32,
        epoch_seconds: u32,
    },
    Tcg {
        /// Exact selected backend semantics. Missing in older prepared inputs
        /// means None, never Some(default); service requests explicitly select.
        #[serde(default)]
        policy: Option<BackendPolicy>,
        executable: Artifact,
        kernel: Artifact,
        init: Artifact,
        command_line: Artifact,
        firmware: Artifact,
        /// Relocatable host runtime closure, including its loader/dependencies.
        runtime: Artifact,
        formatter: Option<Artifact>,
        environments: BTreeMap<String, Environment>,
        /// Complete boot authority, retained for initial and future machines.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        boot_archives: BTreeMap<String, Artifact>,
        epoch_seconds: u32,
    },
}
impl Inventory {
    pub fn artifacts(&self) -> Vec<&Artifact> {
        let (mut artifacts, formatter, environments) = match self {
            Self::Pv {
                executable,
                kernel,
                init,
                command_line,
                progress,
                runtime,
                formatter,
                environments,
                ..
            } => (
                vec![executable, kernel, init, command_line, progress, runtime],
                formatter,
                environments,
            ),
            Self::Tcg {
                executable,
                kernel,
                init,
                command_line,
                firmware,
                runtime,
                formatter,
                environments,
                ..
            } => (
                vec![executable, kernel, init, command_line, firmware, runtime],
                formatter,
                environments,
            ),
        };
        let boots = match self {
            Self::Pv { boot_archives, .. } | Self::Tcg { boot_archives, .. } => boot_archives,
        };
        artifacts.extend(boots.values());
        artifacts.extend(formatter.iter());
        for environment in environments.values() {
            artifacts.extend(environment.configs.values());
        }
        artifacts
    }
    /// Selected semantics without defaulting an absent configuration. The
    /// application converts and validates this data before materialization.
    // TODO: Re-evaluate: PV and TCG retain the same wire policy schema while
    // actual factory/restore validation remains backend-owned, not protocol code.
    pub fn backend_policy(&self) -> Option<&BackendPolicy> {
        match self {
            Self::Pv { policy, .. } | Self::Tcg { policy, .. } => policy.as_ref(),
        }
    }
    /// Selected TCG semantics without defaulting an absent configuration.
    /// PV selection remains unavailable until its real worker policy hooks exist.
    // TODO: Re-evaluate: snapshot admission must compare the optional value,
    // not coerce legacy None into explicit named policy with sockets disabled.
    pub fn tcg_policy(&self) -> Option<&BackendPolicy> {
        match self {
            Self::Tcg { policy, .. } => policy.as_ref(),
            Self::Pv { .. } => None,
        }
    }
    pub fn validate(&self) -> Result<(), String> {
        for artifact in self.artifacts() {
            artifact.validate()?;
        }
        Ok(())
    }
}

/// Authored override position, not another scheduler. Feed due entries to the
/// Driver one at a time and drain due effects before advancing to the next.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Program {
    pub inputs: Vec<TimedInput>,
    pub next: usize,
    pub admissions: Vec<InputAdmission>,
    pub rejections: Vec<InputRejection>,
}
impl Program {
    pub fn validate(&self) -> Result<(), String> {
        crate::input_program::validate_program(&self.inputs, u64::MAX)?;
        if self.next > self.inputs.len()
            || self.admissions.len().checked_add(self.rejections.len()) != Some(self.next)
        {
            return Err("Driver input cursor differs from its history".into());
        }
        let mut admitted = self.admissions.iter().peekable();
        let mut rejected = self.rejections.iter().peekable();
        let mut previous = 0;
        for input in &self.inputs[..self.next] {
            if rejected.peek().is_some_and(|r| r.key == input.key) {
                let rejection = rejected.next().ok_or("missing input rejection")?;
                if rejection.at_ns != input.at_ns || rejection.reason.is_empty() {
                    return Err("invalid Driver input rejection evidence".into());
                }
            } else {
                let row = admitted.next().ok_or("missing Driver input admission")?;
                if row.key != input.key
                    || row.plan.at_ns != input.at_ns
                    || row.plan.action != input.action
                    || row.plan.id <= previous
                    || input.at_ns.checked_add(input.timeout_ns) != Some(row.plan.deadline_ns)
                {
                    return Err("Driver input admission differs from source".into());
                }
                previous = row.plan.id;
            }
        }
        if admitted.next().is_some() || rejected.next().is_some() {
            return Err("unconsumed Driver input history".into());
        }
        Ok(())
    }
    /// Relate the cursor to the committed position before native restoration.
    /// Pending entries may be due now but not in the past; consumed entries
    /// cannot be in the future. Errors name the inconsistent authored key.
    // TODO: Re-evaluate: authored history is service-owned, so this relationship
    // must be checked here rather than inferred from opaque native bodies.
    pub fn validate_at(&self, at: u64) -> Result<(), String> {
        self.validate()?;
        for (index, input) in self.inputs.iter().enumerate() {
            if (index < self.next && input.at_ns > at) || (index >= self.next && input.at_ns < at) {
                return Err(format!("override {} at {} disagrees with checkpoint position {at}; preserve the consumed prefix and place pending overrides at or after the checkpoint", input.key, input.at_ns));
            }
        }
        Ok(())
    }
    /// Freeze the consumed prefix; only pending entries can be edited on branch.
    pub fn replace_pending(&mut self, inputs: Vec<TimedInput>) -> Result<(), String> {
        self.validate()?;
        crate::input_program::validate_program(&inputs, u64::MAX)?;
        let prefix = &self.inputs[..self.next];
        let mut replacement = prefix.to_vec();
        for input in inputs {
            if let Some(consumed) = prefix.iter().find(|old| old.key == input.key) {
                if consumed != &input {
                    return Err(format!(
                        "cannot change applied override {} in a checkpoint",
                        input.key
                    ));
                }
            } else {
                replacement.push(input);
            }
        }
        crate::input_program::validate_program(&replacement, u64::MAX)?;
        self.inputs = replacement;
        Ok(())
    }
}

/// Readiness is an ever latch for INITIAL machines, including the oracle.
/// Spawned machines do not enlarge this set; a reboot never clears it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    pub initial: Vec<String>,
    pub seen: Vec<String>,
    pub latched: bool,
}
impl Readiness {
    pub fn observe(&mut self, machine: &str) {
        if self.initial.iter().any(|name| name == machine)
            && !self.seen.iter().any(|name| name == machine)
        {
            self.seen.push(machine.to_owned());
            self.seen.sort();
        }
        self.latched |= self.seen.len() == self.initial.len();
    }
    pub fn validate(&self) -> Result<(), String> {
        if self.initial.is_empty()
            || self.seen.len() > self.initial.len()
            || !strictly_ordered(&self.initial)
            || !strictly_ordered(&self.seen)
            || self.seen.iter().any(|name| !self.initial.contains(name))
            || self.latched != (self.seen.len() == self.initial.len())
        {
            return Err("invalid Driver readiness latch".into());
        }
        Ok(())
    }
}
fn strictly_ordered(names: &[String]) -> bool {
    names.iter().all(|name| !name.is_empty()) && names.windows(2).all(|pair| pair[0] < pair[1])
}

/// Current service outcome continues; observer sessions and history do not.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    #[serde(skip)]
    pub session: PublicationSession,
    pub oracle_exit: Option<u8>,
}
/// Runtime-only observer bookkeeping. Intentionally has no serialization trait.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublicationSession {
    pub next_driver_sequence: u64,
    pub observation_records: u64,
    pub witness: [u8; 32],
    pub witness_records: u64,
    pub applied_faults: Vec<AppliedFault>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedFault {
    pub requested_at_ns: u64,
    pub applied_at_ns: u64,
    pub service: String,
    pub action: String,
    pub detail: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct World {
    pub seed: u64,
    /// All current names in Driver insertion order, oracle first. Restoring
    /// native manifests needs no public MachineId constructor or index getter.
    pub machine_names: Vec<String>,
    pub input_program: Program,
    pub intervention_next: usize,
    pub readiness: Readiness,
    pub publication: Publication,
    pub position: BoundaryPosition,
}
impl World {
    pub fn validate(&self) -> Result<(), String> {
        self.input_program
            .validate_at(self.position.simulated_time_ns)?;
        self.readiness.validate()?;
        let unique: BTreeSet<_> = self.machine_names.iter().collect();
        if self.machine_names.is_empty()
            || unique.len() != self.machine_names.len()
            || self.machine_names.iter().any(|name| name.is_empty())
            || self
                .readiness
                .initial
                .iter()
                .any(|name| !unique.contains(name))
        {
            return Err("invalid Driver service machine-name inventory".into());
        }
        if self.intervention_next > self.input_program.admissions.len() {
            return Err("Driver intervention cursor exceeds admitted inputs".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct State {
    pub world: World,
}

/// Published only after every referenced native body and Driver metadata file
/// is durably written. `state.world.input_program` retains the existing Start
/// override-validation contract. No branch seed exists: restoration is exact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub contract: String,
    #[serde(deserialize_with = "checkpoint_version")]
    pub version: u32,
    pub inventory: Inventory,
    pub driver: Artifact,
    /// Native bodies in the Driver's insertion order, independent of filenames.
    pub machines: Vec<Artifact>,
    pub state: State,
}
fn checkpoint_version<'de, D: serde::Deserializer<'de>>(decoder: D) -> Result<u32, D::Error> {
    let version = u32::deserialize(decoder)?;
    if version != ENVELOPE_VERSION {
        return Err(serde::de::Error::custom("checkpoint was made by an older or incompatible engine; replay with its retained engine or run fresh"));
    }
    Ok(version)
}
impl Checkpoint {
    pub fn validate(&self) -> Result<(), String> {
        if self.contract != CONTRACT || self.version != ENVELOPE_VERSION {
            return Err("unsupported Driver service checkpoint contract/version".into());
        }
        self.inventory.validate()?;
        self.driver.validate()?;
        if self.machines.is_empty() {
            return Err("invalid Driver checkpoint machine count".into());
        }
        if self.machines.len() != self.state.world.machine_names.len() {
            return Err("Driver native snapshot inventory has missing or unused entries".into());
        }
        for artifact in &self.machines {
            artifact.validate()?;
        }
        self.state.world.validate()
    }
}
