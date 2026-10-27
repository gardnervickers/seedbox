//! Human-authored `seedbox.yaml`, before local paths and image defaults resolve.
//!
//! This format deliberately excludes execution options and retained recordings.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, num::NonZeroU32, path::PathBuf};

/// Application machines and the independently declared oracle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Project {
    pub name: String,
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
    pub oracle: Machine,
    #[serde(default)]
    pub network: NetworkDefaults,
    #[serde(default)]
    pub simulator: SimulatorDefaults,
}

/// Launch inputs shared by application machines and the oracle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "MachineFields"
)]
pub struct Machine {
    /// Guest RAM in MiB; omission uses the backend default.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(rename = "memoryMiB")]
    pub memory_mib: Option<NonZeroU32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    pub environment: BTreeMap<String, Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    pub files: Vec<InitialFile>,
    pub volumes: BTreeMap<String, Volume>,
    /// Processes on this machine may die of a core-dumping signal or be
    /// killed for running out of memory. Without it, any such death of any
    /// process on the machine fails the run.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_crashes: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineFields {
    #[serde(rename = "memoryMiB")]
    memory_mib: Option<NonZeroU32>,
    image: Option<String>,
    build: Option<Build>,
    entrypoint: Option<Vec<String>>,
    command: Option<Vec<String>>,
    #[serde(default)]
    environment: BTreeMap<String, Option<String>>,
    working_directory: Option<String>,
    user: Option<String>,
    #[serde(default)]
    files: Vec<InitialFile>,
    #[serde(default)]
    volumes: BTreeMap<String, Volume>,
    #[serde(default)]
    allow_crashes: bool,
}

impl TryFrom<MachineFields> for Machine {
    type Error = String;

    fn try_from(fields: MachineFields) -> Result<Self, Self::Error> {
        if fields.image.is_some() == fields.build.is_some() {
            return Err("a machine requires exactly one of image or build".into());
        }
        if fields.image.as_ref().is_some_and(|image| {
            image.is_empty() || image.chars().any(char::is_whitespace) || image.contains("://")
        }) {
            return Err(
                "image must be a nonempty registry reference without a transport prefix".into(),
            );
        }
        Ok(Self {
            memory_mib: fields.memory_mib,
            image: fields.image,
            build: fields.build,
            entrypoint: fields.entrypoint,
            command: fields.command,
            environment: fields.environment,
            working_directory: fields.working_directory,
            user: fields.user,
            files: fields.files,
            volumes: fields.volumes,
            allow_crashes: fields.allow_crashes,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Build {
    pub context: PathBuf,
    #[serde(default = "dockerfile")]
    pub dockerfile: PathBuf,
}

fn dockerfile() -> PathBuf {
    "Dockerfile".into()
}

/// A file or directory captured at submission, never a live host mount.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InitialFile {
    pub source: PathBuf,
    pub target: String,
}

/// A fresh writable disk scoped to one machine.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Volume {
    pub target: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkDefaults {
    #[serde(default = "latency")]
    pub latency: String,
}

fn latency() -> String {
    "1ms".into()
}

impl Default for NetworkDefaults {
    fn default() -> Self {
        Self { latency: latency() }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulatorDefaults {
    /// Maximum simulated time between parallel execution barriers.
    #[serde(default = "quantum")]
    pub quantum: String,
}

fn quantum() -> String {
    "1ms".into()
}

impl Default for SimulatorDefaults {
    fn default() -> Self {
        Self { quantum: quantum() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oracle_has_the_same_launch_options_without_a_machine_reference() {
        let project: Project = serde_yaml_ng::from_str(
            "name: test\noracle:\n  build:\n    context: ./oracle\n  entrypoint: []\n  environment:\n    REMOVE: null\n    KEEP: 'yes'\n",
        ).unwrap();
        assert!(project.machines.is_empty());
        assert_eq!(project.oracle.entrypoint, Some(vec![]));
        assert_eq!(project.oracle.command, None);
        assert_eq!(project.oracle.environment["REMOVE"], None);
        assert_eq!(project.oracle.environment["KEEP"].as_deref(), Some("yes"));
        assert_eq!(
            project.oracle.build.unwrap().dockerfile,
            PathBuf::from("Dockerfile")
        );
        assert_eq!(project.simulator.quantum, "1ms");
        assert_eq!(project.network.latency, "1ms");
    }

    #[test]
    fn machine_source_is_exclusive_and_required() {
        for machine in ["{}", "{image: app, build: {context: .}}", "{image: ''}"] {
            assert!(serde_yaml_ng::from_str::<Machine>(machine).is_err());
        }
    }

    #[test]
    fn runtime_options_and_advanced_knobs_are_not_authoring_inputs() {
        for extra in [
            "limits: {wallTime: 5m}",
            "seed: 1",
            "startPaused: true",
            "simulator: {backend: tcg}",
            "network: {jitter: 1ms}",
            "simulator: {machineWorkers: 4}",
            "simulator: {blocksPerProgressCall: 10000}",
        ] {
            let yaml = format!("name: test\noracle: {{image: oracle}}\n{extra}\n");
            assert!(
                serde_yaml_ng::from_str::<Project>(&yaml).is_err(),
                "{extra}"
            );
        }
    }

    #[test]
    fn allow_crashes_is_a_per_machine_flag() {
        let project: Project = serde_yaml_ng::from_str(
            "name: keel\nmachines:\n  s3:\n    image: keel\n    allowCrashes: true\n  db:\n    image: db\noracle: {image: oracle}\n",
        )
        .unwrap();
        assert!(project.machines["s3"].allow_crashes);
        assert!(!project.machines["db"].allow_crashes);
        assert!(!project.oracle.allow_crashes);
        // Set flags serialize as authored; machines without one omit the key.
        let value = serde_json::to_value(&project).unwrap();
        assert_eq!(value["machines"]["s3"]["allowCrashes"], true);
        assert!(value["machines"]["db"].get("allowCrashes").is_none());
        assert_eq!(serde_json::from_value::<Project>(value).unwrap(), project);
        for bad in [
            "{image: app, allowCrashes: 2}",
            "{image: app, allowCrashes: [SIGABRT]}",
            "{image: app, expectedCrashes: [{signal: SIGABRT, command: x}]}",
            "{image: app, expectedOutOfMemoryKills: true}",
        ] {
            assert!(serde_yaml_ng::from_str::<Machine>(bad).is_err(), "{bad}");
        }
    }
}
