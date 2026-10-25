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
    /// Space the volume adds to its machine's disk, authored with a binary
    /// unit (`size: 4GiB`, `size: 512MiB`); omission uses
    /// [`DEFAULT_VOLUME_BYTES`].
    #[serde(
        default,
        rename = "size",
        skip_serializing_if = "Option::is_none",
        with = "volume_size"
    )]
    pub size_bytes: Option<u64>,
}

/// A volume's size when none is given.
pub const DEFAULT_VOLUME_BYTES: u64 = 1 << 30;
/// The smallest volume; ext4 needs room for its journal and metadata.
pub const MIN_VOLUME_BYTES: u64 = 16 << 20;
/// The largest disk of one machine, which holds all its volumes. Written
/// sectors live in the worker's memory, so this also bounds what one
/// machine's disk can cost the host.
pub const MAX_MACHINE_DISK_BYTES: u64 = 64 << 30;

/// The size of the disk that holds a machine's volumes. Volumes without a
/// size (`None`) share one [`DEFAULT_VOLUME_BYTES`] allotment; each declared
/// size adds to it.
/// A machine's volumes share one filesystem on this disk.
pub fn machine_disk_bytes(sizes: impl IntoIterator<Item = Option<u64>>) -> Result<u64, String> {
    let mut total = 0u64;
    let mut shared = false;
    for size in sizes {
        match size {
            Some(size) => {
                volume_size::check(size)?;
                total = total.saturating_add(size);
            }
            None => shared = true,
        }
    }
    if shared {
        total = total.saturating_add(DEFAULT_VOLUME_BYTES);
    }
    if total > MAX_MACHINE_DISK_BYTES {
        return Err(format!(
            "volume sizes total {}, above the {} limit for one machine",
            volume_size::format(total),
            volume_size::format(MAX_MACHINE_DISK_BYTES)
        ));
    }
    Ok(total)
}

/// A volume size as a whole number of MiB or GiB (`4GiB`, `1536MiB`),
/// within `MIN_VOLUME_BYTES..=MAX_MACHINE_DISK_BYTES`.
pub mod volume_size {
    use serde::{de::Error, Deserializer, Serializer};

    const UNITS: [(&str, u64); 2] = [("GiB", 1 << 30), ("MiB", 1 << 20)];

    pub fn serialize<S: Serializer>(bytes: &Option<u64>, serializer: S) -> Result<S::Ok, S::Error> {
        match bytes {
            Some(bytes) => serializer.serialize_str(&format(*bytes)),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<u64>, D::Error> {
        struct Size;
        impl serde::de::Visitor<'_> for Size {
            type Value = Option<u64>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a size such as 4GiB")
            }
            fn visit_str<E: Error>(self, raw: &str) -> Result<Self::Value, E> {
                parse(raw).map(Some).map_err(E::custom)
            }
            // A bare number has no unit; name the expected form.
            fn visit_u64<E: Error>(self, raw: u64) -> Result<Self::Value, E> {
                self.visit_str(&raw.to_string())
            }
            fn visit_i64<E: Error>(self, raw: i64) -> Result<Self::Value, E> {
                self.visit_str(&raw.to_string())
            }
            fn visit_f64<E: Error>(self, raw: f64) -> Result<Self::Value, E> {
                self.visit_str(&raw.to_string())
            }
            /// An explicit null is the same as omission.
            fn visit_unit<E: Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }
        }
        deserializer.deserialize_any(Size)
    }

    /// The largest unit that divides the size exactly; bytes when none does.
    pub fn format(bytes: u64) -> String {
        UNITS
            .iter()
            .find(|(_, scale)| bytes.is_multiple_of(*scale))
            .map(|(unit, scale)| format!("{}{unit}", bytes / scale))
            .unwrap_or_else(|| format!("{bytes}B"))
    }

    fn range() -> String {
        format!(
            "from {} to {}",
            format(super::MIN_VOLUME_BYTES),
            format(super::MAX_MACHINE_DISK_BYTES)
        )
    }

    /// A size within the volume range and a whole number of MiB.
    pub fn check(bytes: u64) -> Result<(), String> {
        if bytes.is_multiple_of(1 << 20)
            && (super::MIN_VOLUME_BYTES..=super::MAX_MACHINE_DISK_BYTES).contains(&bytes)
        {
            Ok(())
        } else {
            Err(format!(
                "volume size {} must be a whole number of MiB {}",
                format(bytes),
                range()
            ))
        }
    }

    pub fn parse(raw: &str) -> Result<u64, String> {
        let invalid = || {
            format!(
                "size {raw:?}: expected a whole number of MiB or GiB such as 4GiB or 512MiB, {}",
                range()
            )
        };
        let digits = raw.bytes().take_while(u8::is_ascii_digit).count();
        let (number, unit) = raw.split_at(digits);
        let scale = UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, scale)| *scale)
            .ok_or_else(invalid)?;
        number
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .filter(|bytes| check(*bytes).is_ok())
            .ok_or_else(invalid)
    }
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
    fn volume_size_is_an_optional_binary_size() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<Volume>(yaml);
        assert_eq!(parse("target: /data").unwrap().size_bytes, None);
        assert_eq!(
            parse("{target: /data, size: null}").unwrap().size_bytes,
            None
        );
        for (text, bytes) in [
            ("4GiB", 4u64 << 30),
            ("1536MiB", 1536 << 20),
            ("16MiB", 16 << 20),
            ("64GiB", 64 << 30),
        ] {
            assert_eq!(
                parse(&format!("{{target: /data, size: {text}}}"))
                    .unwrap()
                    .size_bytes,
                Some(bytes),
                "{text}"
            );
        }
        for bad in [
            "4GB", "4gib", "4 GiB", "1.5GiB", "4096", "4096KiB", "15MiB", "65GiB", "0MiB", "-1GiB",
            "GiB", "''", "1TiB",
        ] {
            let error = parse(&format!("{{target: /data, size: {bad}}}"))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(
                    "expected a whole number of MiB or GiB such as 4GiB or 512MiB, from 16MiB to 64GiB"
                ),
                "{bad}: {error}"
            );
        }
        // Sizes serialize in the largest exact unit and round-trip; omission stays omitted.
        for (bytes, text) in [(4u64 << 30, "4GiB"), (1536 << 20, "1536MiB")] {
            let volume = Volume {
                target: "/data".into(),
                size_bytes: Some(bytes),
            };
            let value = serde_json::to_value(&volume).unwrap();
            assert_eq!(value, serde_json::json!({"target": "/data", "size": text}));
            assert_eq!(serde_json::from_value::<Volume>(value).unwrap(), volume);
        }
        let omitted = Volume {
            target: "/data".into(),
            size_bytes: None,
        };
        assert_eq!(
            serde_json::to_value(&omitted).unwrap(),
            serde_json::json!({"target": "/data"})
        );
    }

    #[test]
    fn a_machine_disk_holds_the_sum_of_its_volume_sizes() {
        assert_eq!(machine_disk_bytes([]), Ok(0));
        assert_eq!(machine_disk_bytes([None]), Ok(DEFAULT_VOLUME_BYTES));
        // Unsized volumes share one default allotment, as before sizes existed.
        assert_eq!(machine_disk_bytes([None; 3]), Ok(DEFAULT_VOLUME_BYTES));
        assert_eq!(machine_disk_bytes([Some(3 << 30), None, None]), Ok(4 << 30));
        assert_eq!(machine_disk_bytes([Some(3 << 30), None]), Ok(4 << 30));
        assert_eq!(machine_disk_bytes([Some(32 << 30); 2]), Ok(64 << 30));
        assert_eq!(
            machine_disk_bytes([Some(32 << 30), Some(32 << 30), Some(16 << 20)]),
            Err("volume sizes total 65552MiB, above the 64GiB limit for one machine".into())
        );
        assert!(machine_disk_bytes([Some((1 << 30) + 512)]).is_err());
        assert!(machine_disk_bytes([Some(0)]).is_err());
    }
}
