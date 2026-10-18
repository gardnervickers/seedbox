//! Self-contained resolved simulations. Only executable binary bytes use content references.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Asset(pub String);
impl TryFrom<String> for Asset {
    type Error = String;
    fn try_from(value: String) -> Result<Self, String> {
        let hash = value
            .strip_prefix("sha256:")
            .ok_or("expected sha256: binary content reference")?;
        if hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("expected sha256: followed by 64 lowercase hexadecimal digits".into());
        }
        Ok(Self(value))
    }
}
impl From<Asset> for String {
    fn from(value: Asset) -> String {
        value.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    #[serde(default)]
    pub name: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Clock {
    pub epoch_seconds: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Runtime {
    /// Inline backend launch contract and resolved semantic policies.
    pub execution: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Process {
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: String,
    pub user: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Volume {
    pub target: String,
    pub size_bytes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Image {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub digest: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Machine {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Complete boot archive, including runtime helpers and the resolved launch recipe.
    pub boot_archive: Asset,
    pub memory_bytes: u64,
    pub process: Process,
    pub volumes: BTreeMap<String, Volume>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anonymous_volumes: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Override {
    pub id: String,
    pub at_ns: u64,
    pub action: serde_json::Value,
    #[serde(default = "default_timeout_ns")]
    pub timeout_ns: u64,
}
pub const fn default_timeout_ns() -> u64 {
    5_000_000_000
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Start {
    Fresh,
    Checkpoint {
        manifest: serde_json::Value,
        assets: BTreeMap<String, Asset>,
    },
}
impl Start {
    /// A checkpoint already contains the effects of its consumed override prefix.
    /// Only the pending suffix can be changed when continuing that state.
    pub fn validate_overrides(&self, overrides: &[Override]) -> Result<(), String> {
        let Self::Checkpoint { manifest, .. } = self else {
            return Ok(());
        };
        let program = manifest
            .pointer("/state/world/input_program")
            .ok_or("checkpoint manifest is missing its override cursor")?;
        let inputs = program
            .get("inputs")
            .and_then(serde_json::Value::as_array)
            .ok_or("checkpoint manifest has invalid override inputs")?;
        let next = program
            .get("next")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n <= inputs.len())
            .ok_or("checkpoint manifest has an invalid override cursor")?;
        #[derive(Deserialize)]
        struct Consumed {
            key: String,
            at_ns: u64,
            action: serde_json::Value,
            #[serde(default = "default_timeout_ns")]
            timeout_ns: u64,
        }
        for (index, input) in inputs[..next].iter().enumerate() {
            let consumed: Consumed = serde_json::from_value(input.clone()).map_err(|e| {
                format!("checkpoint manifest has an invalid consumed override: {e}")
            })?;
            let unchanged = overrides.get(index).is_some_and(|input| {
                input.id == consumed.key
                    && input.at_ns == consumed.at_ns
                    && input.action == consumed.action
                    && input.timeout_ns == consumed.timeout_ns
            });
            if !unchanged {
                return Err(format!(
                    "checkpoint overrides must preserve consumed override {} at position {}; start fresh to edit or remove consumed overrides",
                    consumed.key, index + 1
                ));
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Simulator {
    #[serde(rename = "blocksPerProgressCall")]
    pub progress_budget: crate::progress::ProgressBudget,
    pub schedule: crate::Schedule,
    pub disk: crate::DiskOptions,
    /// The oracle exposes the world control API.
    pub oracle_control: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Definition {
    pub architecture: Architecture,
    pub seed: u64,
    pub clock: Clock,
    pub runtime: Runtime,
    pub machines: BTreeMap<String, Machine>,
    pub oracle: Machine,
    pub network: crate::Network,
    pub simulator: Simulator,
    pub start: Start,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<Override>,
}
impl Definition {
    /// Private adapters retain the oracle's stable machine name for addressing.
    pub const ORACLE: &'static str = "oracle";
    pub fn oracle_name(&self) -> &str {
        self.oracle.name.as_deref().unwrap_or(Self::ORACLE)
    }
    pub fn configuration(&self, name: &str) -> Result<crate::Simulation, String> {
        self.runtime.driver_backend()?;
        let mut services = BTreeMap::new();
        for (name, machine) in self
            .machines
            .iter()
            .map(|(n, m)| (n.as_str(), m))
            .chain(std::iter::once((self.oracle_name(), &self.oracle)))
        {
            services.insert(
                name.into(),
                crate::Service {
                    memory_mb: Some(
                        u32::try_from(machine.memory_bytes / (1024 * 1024)).unwrap_or(u32::MAX),
                    ),
                    source: crate::ImageSource::Registry {
                        reference: "resolved-binary-input".into(),
                    },
                    platform: match self.architecture {
                        Architecture::X86_64 => "linux/amd64",
                        Architecture::Aarch64 => "linux/arm64",
                    }
                    .into(),
                    entrypoint: Some(machine.process.argv.clone()),
                    command: Some(Vec::new()),
                    environment: machine
                        .process
                        .environment
                        .iter()
                        .map(|(k, v)| (k.clone(), Some(v.clone())))
                        .collect(),
                    working_dir: Some(machine.process.working_directory.clone()),
                    user: Some(machine.process.user.clone()),
                    configs: Vec::new(),
                    volumes: machine
                        .volumes
                        .iter()
                        .map(|(n, v)| crate::VolumeMount {
                            source: n.clone(),
                            target: v.target.clone(),
                        })
                        .collect(),
                    network: self.network.name.clone(),
                },
            );
        }
        Ok(crate::Simulation {
            name: name.into(),
            source: std::path::PathBuf::new(),
            services,
            configs: BTreeMap::new(),
            network: self.network.clone(),
            schedule: self.simulator.schedule.clone(),
            control: self.simulator.oracle_control.then(|| crate::Control {
                service: self.oracle_name().into(),
            }),
            limits: crate::Limits {
                simulated_time_ns: u64::MAX,
                wall_time_ns: u64::MAX,
            },
            check: crate::Check {
                service: self.oracle_name().into(),
                exit_code: 0,
            },
            seed: Some(self.seed),
            wall_clock_epoch: Some(self.clock.epoch_seconds),
            machine: crate::MachineOptions {
                memory_mb: u32::try_from(self.oracle.memory_bytes / (1024 * 1024))
                    .unwrap_or(u32::MAX),
            },
            disk: self.simulator.disk.clone(),
            simulator: crate::project::SimulatorDefaults {
                progress_budget: self.simulator.progress_budget,
            },
        })
    }
}

impl Runtime {
    pub fn driver_backend(&self) -> Result<Option<crate::backend::CpuBackend>, String> {
        crate::backend::CpuBackend::from_recorded_name(
            self.execution["backend"].as_str().unwrap_or_default(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(id: &str, at_ns: u64) -> Override {
        Override {
            id: id.into(),
            at_ns,
            action: json!({"kind": "heal", "handle": id}),
            timeout_ns: default_timeout_ns(),
        }
    }
    fn checkpoint() -> Start {
        Start::Checkpoint {
            manifest: json!({"state": {"world": {"input_program": {
                "inputs": [
                    {"key": "consumed", "at_ns": 10, "action": input("consumed", 10).action},
                    {"key": "pending", "at_ns": 20, "action": input("pending", 20).action}
                ], "next": 1,
                "rejections": [{"key": "consumed", "at_ns": 10, "reason": "missing handle"}]
            }}}}),
            assets: BTreeMap::new(),
        }
    }
    #[test]
    fn checkpoint_keeps_consumed_prefix_and_allows_pending_edits() {
        let start = checkpoint();
        let consumed = input("consumed", 10);
        assert!(start
            .validate_overrides(&[consumed.clone(), input("pending", 20)])
            .is_ok());
        assert!(start
            .validate_overrides(&[consumed.clone(), input("replacement", 30)])
            .is_ok());
        assert!(start
            .validate_overrides(std::slice::from_ref(&consumed))
            .is_ok());
        assert!(start
            .validate_overrides(&[])
            .unwrap_err()
            .contains("consumed override consumed"));
        assert!(start.validate_overrides(&[input("pending", 20)]).is_err());
        assert!(start
            .validate_overrides(&[input("earlier", 1), consumed.clone()])
            .is_err());
        for changed in [
            input("renamed", 10),
            input("consumed", 11),
            Override {
                action: json!({"kind": "other"}),
                ..consumed.clone()
            },
            Override {
                timeout_ns: 1,
                ..consumed.clone()
            },
        ] {
            assert!(start.validate_overrides(&[changed]).is_err());
        }
        assert!(Start::Fresh.validate_overrides(&[]).is_ok());
        assert!(Start::Fresh
            .validate_overrides(&[input("replacement", 1)])
            .is_ok());
    }
    #[test]
    fn checkpoint_requires_a_valid_override_cursor() {
        for program in [
            json!({}),
            json!({"inputs": [], "next": 1}),
            json!({"inputs": [], "next": -1}),
        ] {
            let start = Start::Checkpoint {
                manifest: json!({"state": {"world": {"input_program": program}}}),
                assets: BTreeMap::new(),
            };
            assert!(start.validate_overrides(&[]).is_err());
        }
    }
}
