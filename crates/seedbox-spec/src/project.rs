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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineFields {
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
            image: fields.image,
            build: fields.build,
            entrypoint: fields.entrypoint,
            command: fields.command,
            environment: fields.environment,
            working_directory: fields.working_directory,
            user: fields.user,
            files: fields.files,
            volumes: fields.volumes,
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
    /// Application instructions each guest thread executes per PV progress
    /// call. Each call advances that machine's virtual time by one syscall.
    #[serde(default = "progress_budget")]
    pub instructions_per_progress_call: NonZeroU32,
    /// Charge points per translated block. Each point charges the
    /// instructions up to the next one and may make a progress call there, so
    /// more points spread the calls more evenly at some speed cost.
    #[serde(default = "progress_points")]
    pub progress_points_per_block: NonZeroU32,
    /// Seeded extra virtual time each progress call adds, drawn from zero up
    /// to this bound and authored as a duration (`progressJitter: 5us`). It
    /// takes the default of zero, which charges every call the same, only
    /// when seedbox.yaml leaves it out.
    #[serde(
        default,
        rename = "progressJitter",
        skip_serializing_if = "is_zero",
        with = "progress_jitter"
    )]
    pub progress_jitter_ns: u64,
}

/// The largest progress jitter: each nominal microsecond costs the guest one
/// extra progress call, so the bound also bounds the added work per call.
pub const MAX_PROGRESS_JITTER_NS: u64 = 1_000_000;

pub(crate) fn is_zero(value: &u64) -> bool {
    *value == 0
}

mod progress_jitter {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ns: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{ns}ns"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).map_err(D::Error::custom)
    }

    /// A whole number of ns, us or ms, or a bare 0, up to the maximum.
    pub(super) fn parse(raw: &str) -> Result<u64, String> {
        let invalid = || {
            format!(
                "progressJitter {raw:?}: expected a duration such as 5us, from 0 to {}ms",
                super::MAX_PROGRESS_JITTER_NS / 1_000_000
            )
        };
        let digits = raw.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = raw.split_at(digits);
        let scale = match unit {
            "" if number == "0" => 1,
            "ns" => 1,
            "us" => 1_000,
            "ms" => 1_000_000,
            _ => return Err(invalid()),
        };
        number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .filter(|ns| *ns <= super::MAX_PROGRESS_JITTER_NS)
            .ok_or_else(invalid)
    }
}

fn progress_budget() -> NonZeroU32 {
    NonZeroU32::new(50_000).expect("the default progress budget is nonzero")
}

fn progress_points() -> NonZeroU32 {
    NonZeroU32::MIN
}

impl Default for SimulatorDefaults {
    fn default() -> Self {
        Self {
            instructions_per_progress_call: progress_budget(),
            progress_points_per_block: progress_points(),
            progress_jitter_ns: 0,
        }
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
        assert_eq!(
            project.simulator.instructions_per_progress_call.get(),
            50_000
        );
        assert_eq!(project.simulator.progress_points_per_block.get(), 1);
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
            "network: {jitter: 1ms}",
        ] {
            let yaml = format!("name: test\noracle: {{image: oracle}}\n{extra}\n");
            assert!(
                serde_yaml_ng::from_str::<Project>(&yaml).is_err(),
                "{extra}"
            );
        }
        for simulator in [
            "instructionsPerProgressCall: 0",
            "progressPointsPerBlock: 0",
            "blocksPerProgressCall: 10000",
        ] {
            assert!(
                serde_yaml_ng::from_str::<SimulatorDefaults>(simulator).is_err(),
                "{simulator}"
            );
        }
        let simulator: SimulatorDefaults =
            serde_yaml_ng::from_str("instructionsPerProgressCall: 7\nprogressPointsPerBlock: 3")
                .unwrap();
        assert_eq!(simulator.instructions_per_progress_call.get(), 7);
        assert_eq!(simulator.progress_points_per_block.get(), 3);
    }

    #[test]
    fn progress_jitter_is_an_optional_bounded_duration() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<SimulatorDefaults>(yaml);
        assert_eq!(parse("{}").unwrap().progress_jitter_ns, 0);
        assert_eq!(
            parse("progressJitter: 5us").unwrap().progress_jitter_ns,
            5_000
        );
        assert_eq!(parse("progressJitter: '0'").unwrap().progress_jitter_ns, 0);
        assert_eq!(
            parse("progressJitter: 1ms").unwrap().progress_jitter_ns,
            1_000_000
        );
        for bad in ["2ms", "1001us", "5", "-1us", "1.5us", "5 us", "1s", "''"] {
            assert!(parse(&format!("progressJitter: '{bad}'")).is_err(), "{bad}");
        }
        // The default is omitted, so definitions without jitter serialize unchanged.
        let default = serde_json::to_value(SimulatorDefaults::default()).unwrap();
        assert!(default.get("progressJitter").is_none());
        let jitter = SimulatorDefaults {
            progress_jitter_ns: 2_500,
            ..Default::default()
        };
        let value = serde_json::to_value(&jitter).unwrap();
        assert_eq!(value["progressJitter"], "2500ns");
        assert_eq!(
            serde_json::from_value::<SimulatorDefaults>(value).unwrap(),
            jitter
        );
    }
}
