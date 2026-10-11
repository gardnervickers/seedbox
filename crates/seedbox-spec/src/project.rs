//! Human-authored `seedbox.yaml`, before local paths and image defaults resolve.
//!
//! This format deliberately excludes execution options and retained recordings.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

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
    pub cpu: CpuDefaults,
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

/// The virtual CPU every machine runs on. Application compute is charged
/// through the guest's progress instrumentation (guest/progress): every
/// `PROGRESS_INSTRUCTIONS` executed instructions a thread makes a progress
/// call, and the guest kernel charges it the time those instructions take at
/// `speed`, give or take `jitter`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CpuDefaults {
    /// Instructions per virtual second, authored as a frequency
    /// (`speed: 2GHz`, `speed: 800MHz`).
    #[serde(default = "speed_hz", rename = "speed", with = "frequency")]
    pub speed_hz: u64,
    /// Seeded variation in the time a progress call costs, authored as a
    /// percentage of the nominal charge (`jitter: 5%`). Each call draws
    /// uniformly within that much of the nominal charge, above or below,
    /// from a per-machine stream the simulator seeds; zero charges every
    /// call the same.
    #[serde(
        default,
        rename = "jitter",
        skip_serializing_if = "is_zero_u8",
        with = "percent"
    )]
    pub jitter_percent: u8,
}

/// Application instructions each guest thread executes between progress
/// calls. A performance constant of the instrumentation, not a setting:
/// fewer instructions make the virtual clock finer at a translation cost.
pub const PROGRESS_INSTRUCTIONS: u32 = 50_000;
/// Charge points per translated block: where inside a block a progress call
/// may land. One point charges at block entry.
pub const PROGRESS_POINTS: u32 = 1;
/// The slowest CPU: a progress call then costs 50 ms, the time a syscall
/// counted plan still charges faithfully.
pub const MIN_SPEED_HZ: u64 = 1_000_000;
/// The fastest CPU: a progress call then costs 50 ns.
pub const MAX_SPEED_HZ: u64 = 1_000_000_000_000;

impl CpuDefaults {
    /// Nominal virtual time one progress call costs at this speed.
    pub fn progress_ns(&self) -> u64 {
        (u64::from(PROGRESS_INSTRUCTIONS) * 1_000_000_000).div_ceil(self.speed_hz)
    }

    /// Bound on the seeded variation of a progress call's charge, each way.
    pub fn progress_jitter_ns(&self) -> u64 {
        self.progress_ns() * u64::from(self.jitter_percent) / 100
    }
}

fn speed_hz() -> u64 {
    2_000_000_000
}

impl Default for CpuDefaults {
    fn default() -> Self {
        Self {
            speed_hz: speed_hz(),
            jitter_percent: 0,
        }
    }
}

/// Serde helper for optional durations that are off at zero.
pub fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u8(value: &u8) -> bool {
    *value == 0
}

/// A CPU speed as a frequency string: a decimal number with a unit from
/// Hz to THz, within `MIN_SPEED_HZ..=MAX_SPEED_HZ`.
pub mod frequency {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    const UNITS: [(&str, u64); 5] = [
        ("THz", 1_000_000_000_000),
        ("GHz", 1_000_000_000),
        ("MHz", 1_000_000),
        ("kHz", 1_000),
        ("Hz", 1),
    ];

    pub fn serialize<S: Serializer>(hz: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(*hz))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).map_err(D::Error::custom)
    }

    /// The largest unit that divides the frequency exactly.
    pub fn format(hz: u64) -> String {
        let (unit, scale) = UNITS
            .iter()
            .find(|(_, scale)| hz.is_multiple_of(*scale))
            .copied()
            .unwrap_or(("Hz", 1));
        format!("{}{unit}", hz / scale)
    }

    pub fn parse(raw: &str) -> Result<u64, String> {
        let invalid = || {
            format!(
                "speed {raw:?}: expected a frequency such as 2GHz or 1.5GHz, from {} to {}",
                format(super::MIN_SPEED_HZ),
                format(super::MAX_SPEED_HZ)
            )
        };
        let digits = raw
            .bytes()
            .take_while(|b| b.is_ascii_digit() || *b == b'.')
            .count();
        let (number, unit) = raw.split_at(digits);
        let scale = UNITS
            .iter()
            .find(|(name, _)| *name == unit)
            .map(|(_, scale)| *scale)
            .ok_or_else(invalid)?;
        let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
        if whole.is_empty() || (number.contains('.') && fraction.is_empty()) {
            return Err(invalid());
        }
        // The fraction's digits are the scale's trailing zeros, exactly.
        let places = scale.ilog10() as usize;
        if fraction.len() > places || fraction.chars().any(|c| !c.is_ascii_digit()) {
            return Err(invalid());
        }
        let fraction = format!("{fraction:0<places$}");
        let fraction: u64 = if places == 0 {
            0
        } else {
            fraction.parse().map_err(|_| invalid())?
        };
        whole
            .parse::<u64>()
            .ok()
            .and_then(|n| n.checked_mul(scale))
            .and_then(|n| n.checked_add(fraction))
            .filter(|hz| (super::MIN_SPEED_HZ..=super::MAX_SPEED_HZ).contains(hz))
            .ok_or_else(invalid)
    }
}

/// A whole percentage string from 0% to 100%, or a bare 0.
pub mod percent {
    use serde::{de::Error, Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(pct: &u8, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{pct}%"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u8, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse(&raw).map_err(D::Error::custom)
    }

    pub fn parse(raw: &str) -> Result<u8, String> {
        let invalid =
            || format!("jitter {raw:?}: expected a percentage such as 5%, from 0% to 100%");
        let number = match raw.strip_suffix('%') {
            Some(number) => number,
            None if raw == "0" => raw,
            None => return Err(invalid()),
        };
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid());
        }
        number
            .parse::<u8>()
            .ok()
            .filter(|pct| *pct <= 100)
            .ok_or_else(invalid)
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
        assert_eq!(project.cpu, CpuDefaults::default());
        assert_eq!(project.cpu.speed_hz, 2_000_000_000);
        assert_eq!(project.cpu.progress_ns(), 25_000);
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
        for cpu in [
            "instructionsPerProgressCall: 50000",
            "progressPointsPerBlock: 1",
            "progressJitter: 5us",
        ] {
            assert!(
                serde_yaml_ng::from_str::<CpuDefaults>(cpu).is_err(),
                "{cpu}"
            );
        }
        assert!(serde_yaml_ng::from_str::<Project>(
            "name: test\noracle: {image: oracle}\nsimulator: {instructionsPerProgressCall: 7}\n"
        )
        .is_err());
    }

    #[test]
    fn cpu_speed_is_a_bounded_frequency() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<CpuDefaults>(yaml);
        assert_eq!(parse("{}").unwrap().speed_hz, 2_000_000_000);
        for (text, hz) in [
            ("2GHz", 2_000_000_000),
            ("1.5GHz", 1_500_000_000),
            ("800MHz", 800_000_000),
            ("0.5GHz", 500_000_000),
            ("1MHz", 1_000_000),
            ("1000000Hz", 1_000_000),
            ("1THz", 1_000_000_000_000),
            ("2.000000001GHz", 2_000_000_001),
        ] {
            assert_eq!(
                parse(&format!("speed: {text}")).unwrap().speed_hz,
                hz,
                "{text}"
            );
        }
        for bad in [
            "2",
            "2 GHz",
            "2ghz",
            "999kHz",
            "1.1THz",
            "0.1MHz",
            "1.5Hz",
            ".5GHz",
            "1.GHz",
            "2.0000000001GHz",
            "-2GHz",
            "",
            "fast",
        ] {
            assert!(parse(&format!("speed: '{bad}'")).is_err(), "{bad}");
        }
        // Speeds serialize in the largest exact unit and round-trip.
        for (hz, text) in [
            (2_000_000_000, "2GHz"),
            (1_500_000_000, "1500MHz"),
            (1_000_000_000_000, "1THz"),
            (1_234_567, "1234567Hz"),
        ] {
            let cpu = CpuDefaults {
                speed_hz: hz,
                jitter_percent: 0,
            };
            let value = serde_json::to_value(&cpu).unwrap();
            assert_eq!(value, serde_json::json!({"speed": text}));
            assert_eq!(serde_json::from_value::<CpuDefaults>(value).unwrap(), cpu);
        }
    }

    #[test]
    fn cpu_jitter_is_an_optional_percentage() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<CpuDefaults>(yaml);
        assert_eq!(parse("{}").unwrap().jitter_percent, 0);
        assert_eq!(parse("jitter: 5%").unwrap().jitter_percent, 5);
        assert_eq!(parse("jitter: '0'").unwrap().jitter_percent, 0);
        assert_eq!(parse("jitter: 100%").unwrap().jitter_percent, 100);
        for bad in ["101%", "5", "-1%", "1.5%", "5 %", "5us", "%", "''"] {
            assert!(parse(&format!("jitter: '{bad}'")).is_err(), "{bad}");
        }
        let jitter = CpuDefaults {
            speed_hz: 2_000_000_000,
            jitter_percent: 5,
        };
        let value = serde_json::to_value(&jitter).unwrap();
        assert_eq!(value, serde_json::json!({"speed": "2GHz", "jitter": "5%"}));
        assert_eq!(
            serde_json::from_value::<CpuDefaults>(value).unwrap(),
            jitter
        );
    }

    #[test]
    fn progress_charge_follows_speed_and_jitter() {
        let cpu = |speed_hz, jitter_percent| CpuDefaults {
            speed_hz,
            jitter_percent,
        };
        assert_eq!(cpu(2_000_000_000, 0).progress_ns(), 25_000);
        assert_eq!(cpu(2_000_000_000, 0).progress_jitter_ns(), 0);
        assert_eq!(cpu(2_000_000_000, 5).progress_jitter_ns(), 1_250);
        assert_eq!(cpu(2_000_000_000, 100).progress_jitter_ns(), 25_000);
        assert_eq!(cpu(1_000_000, 0).progress_ns(), 50_000_000);
        assert_eq!(cpu(1_000_000_000_000, 0).progress_ns(), 50);
        // A speed that does not divide the budget rounds the charge up.
        assert_eq!(cpu(3_000_000_000, 0).progress_ns(), 16_667);
        assert_eq!(cpu(3_000_000_000, 10).progress_jitter_ns(), 1_666);
    }
}
