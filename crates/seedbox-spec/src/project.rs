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
    #[serde(default, skip_serializing_if = "Engine::is_default")]
    pub engine: Engine,
}

/// Simulation-wide engine choices that change results.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Engine {
    #[serde(default)]
    pub progress_client: ProgressClient,
}

impl Engine {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The build of the guest progress instrumentation that runs applications
/// (guest/progress). Both charge the same instructions at the same points;
/// they differ in how much translator work and memory each translated block
/// costs, which the translator's own charged syscalls make visible in
/// simulated time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProgressClient {
    /// The established client.
    #[default]
    Standard,
    /// Cheaper instrumentation: short-lived processes start about 2.5 times
    /// faster under the translator (arm64 only).
    Lean,
}

impl ProgressClient {
    pub fn is_standard(&self) -> bool {
        *self == Self::Standard
    }
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
    pub cpu: Cpu,
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
    /// Crashes this machine is designed to have, which pass the default
    /// `seedbox.no_crashing_processes` property instead of violating it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expected_crashes: Vec<ExpectedCrash>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MachineFields {
    #[serde(rename = "memoryMiB")]
    memory_mib: Option<NonZeroU32>,
    #[serde(default)]
    cpu: Cpu,
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
    expected_crashes: Vec<ExpectedCrash>,
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
        ExpectedCrash::check_all(&fields.expected_crashes)?;
        Ok(Self {
            memory_mib: fields.memory_mib,
            cpu: fields.cpu,
            image: fields.image,
            build: fields.build,
            entrypoint: fields.entrypoint,
            command: fields.command,
            environment: fields.environment,
            working_directory: fields.working_directory,
            user: fields.user,
            files: fields.files,
            volumes: fields.volumes,
            expected_crashes: fields.expected_crashes,
        })
    }
}

/// A crash a machine is designed to have, such as a watchdog that aborts a
/// stalled process for a supervisor to restart. It matches only the guest
/// kernel's fatal-signal report (`print-fatal-signals`) of `signal` from a
/// thread whose command name is `command` and, when given, whose executable's
/// file name is `executable`. A matching crash passes
/// `seedbox.no_crashing_processes` and is reported as a pass with the
/// declaration that matched; every other crash, and every out-of-memory
/// kill, is reported exactly as without the declaration.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "ExpectedCrashFields"
)]
pub struct ExpectedCrash {
    pub signal: CrashSignal,
    /// The crashing thread's command name as the kernel prints it (its
    /// `comm`, at most 15 bytes; a thread name set with `prctl` or
    /// `pthread_setname_np`).
    pub command: String,
    /// The file name, without a directory, of the crashing process's
    /// executable; omission matches any. Under the binary translator this is
    /// the translator's loader, `libdynamorio.so`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExpectedCrashFields {
    signal: CrashSignal,
    command: String,
    executable: Option<String>,
}

/// Expected crashes one machine may declare.
pub const MAX_EXPECTED_CRASHES: usize = 16;
/// A thread's command name holds at most this many bytes.
pub const MAX_COMMAND_BYTES: usize = 15;
/// An executable's file name holds at most this many bytes.
pub const MAX_EXECUTABLE_BYTES: usize = 255;

/// 1..=max printable ASCII bytes: what a kernel line carries unescaped.
fn printable(text: &str, max: usize) -> bool {
    !text.is_empty() && text.len() <= max && text.bytes().all(|b| (0x20..0x7f).contains(&b))
}

impl TryFrom<ExpectedCrashFields> for ExpectedCrash {
    type Error = String;

    fn try_from(fields: ExpectedCrashFields) -> Result<Self, Self::Error> {
        if !printable(&fields.command, MAX_COMMAND_BYTES) {
            return Err(format!(
                "expected crash command {:?} must be 1 to {MAX_COMMAND_BYTES} printable ASCII bytes, the thread name the kernel reports",
                fields.command
            ));
        }
        if let Some(executable) = &fields.executable {
            if !printable(executable, MAX_EXECUTABLE_BYTES) || executable.contains('/') {
                return Err(format!(
                    "expected crash executable {executable:?} must be a file name without a directory, 1 to {MAX_EXECUTABLE_BYTES} printable ASCII bytes"
                ));
            }
        }
        Ok(Self {
            signal: fields.signal,
            command: fields.command,
            executable: fields.executable,
        })
    }
}

impl ExpectedCrash {
    /// Check one machine's declarations as a whole: bounded, none repeated.
    pub fn check_all(expected: &[Self]) -> Result<(), String> {
        if expected.len() > MAX_EXPECTED_CRASHES {
            return Err(format!(
                "expectedCrashes: at most {MAX_EXPECTED_CRASHES} per machine"
            ));
        }
        for (i, crash) in expected.iter().enumerate() {
            if expected[..i].contains(crash) {
                return Err(format!(
                    "expectedCrashes: {} in {:?} is declared twice",
                    crash.signal.name(),
                    crash.command
                ));
            }
        }
        Ok(())
    }
}

/// A signal the kernel reports as a crash: one whose default action dumps
/// core. SIGKILL and SIGTERM are never reported, so they cannot be expected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CrashSignal {
    /// Quit from the keyboard (3).
    Sigquit,
    /// Illegal instruction (4).
    Sigill,
    /// Trace or breakpoint trap (5).
    Sigtrap,
    /// Abort, as from `abort()` (6).
    Sigabrt,
    /// Bus error (7).
    Sigbus,
    /// Arithmetic exception (8).
    Sigfpe,
    /// Invalid memory reference (11).
    Sigsegv,
    /// CPU time limit exceeded (24).
    Sigxcpu,
    /// File size limit exceeded (25).
    Sigxfsz,
    /// Bad system call (31).
    Sigsys,
}

impl CrashSignal {
    /// The signal's number, the same on x86-64 and arm64 Linux.
    pub fn number(self) -> i32 {
        match self {
            Self::Sigquit => 3,
            Self::Sigill => 4,
            Self::Sigtrap => 5,
            Self::Sigabrt => 6,
            Self::Sigbus => 7,
            Self::Sigfpe => 8,
            Self::Sigsegv => 11,
            Self::Sigxcpu => 24,
            Self::Sigxfsz => 25,
            Self::Sigsys => 31,
        }
    }

    /// The name as authored, such as `SIGABRT`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Sigquit => "SIGQUIT",
            Self::Sigill => "SIGILL",
            Self::Sigtrap => "SIGTRAP",
            Self::Sigabrt => "SIGABRT",
            Self::Sigbus => "SIGBUS",
            Self::Sigfpe => "SIGFPE",
            Self::Sigsegv => "SIGSEGV",
            Self::Sigxcpu => "SIGXCPU",
            Self::Sigxfsz => "SIGXFSZ",
            Self::Sigsys => "SIGSYS",
        }
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
    /// Space the volume adds to its machine's retained disk, authored with a
    /// binary unit (`size: 4GiB`, `size: 512MiB`); omission uses
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
/// The largest retained disk of one machine, which holds all its volumes.
/// Written sectors live in the simulator's memory, so this also bounds what
/// one machine's disk can cost the host.
pub const MAX_MACHINE_DISK_BYTES: u64 = 64 << 30;

/// The size of the retained disk that holds a machine's volumes: the sum of
/// their sizes, `None` taking the default. A machine's volumes share one
/// filesystem on this disk.
pub fn machine_disk_bytes(sizes: impl IntoIterator<Item = Option<u64>>) -> Result<u64, String> {
    let mut total = 0u64;
    for size in sizes {
        let size = size.unwrap_or(DEFAULT_VOLUME_BYTES);
        volume_size::check(size)?;
        total = total.saturating_add(size);
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

/// One machine's virtual CPU. Application compute is charged
/// through the guest's progress instrumentation (guest/progress): every
/// `PROGRESS_INSTRUCTIONS` executed instructions a thread makes a progress
/// call, and the guest kernel charges it the time those instructions take at
/// `speed`, give or take `jitter`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Cpu {
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

impl Cpu {
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
    1_000_000_000
}

impl Default for Cpu {
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
            // A bare number has no unit; name the field and the expected form.
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
        assert_eq!(project.oracle.cpu, Cpu::default());
        assert_eq!(project.oracle.cpu.speed_hz, 1_000_000_000);
        assert_eq!(project.oracle.cpu.progress_ns(), 50_000);
        assert_eq!(project.network.latency, "1ms");
    }

    #[test]
    fn machine_source_is_exclusive_and_required() {
        for machine in ["{}", "{image: app, build: {context: .}}", "{image: ''}"] {
            assert!(serde_yaml_ng::from_str::<Machine>(machine).is_err());
        }
    }

    #[test]
    fn expected_crashes_are_explicit_per_machine_declarations() {
        let project: Project = serde_yaml_ng::from_str(
            "name: keel\nmachines:\n  s3:\n    image: keel\n    expectedCrashes:\n    - signal: SIGABRT\n      command: keel-watchdog\n    - {signal: SIGSEGV, command: keel-s3dd, executable: libdynamorio.so}\noracle: {image: oracle}\n",
        )
        .unwrap();
        let expected = &project.machines["s3"].expected_crashes;
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[0].signal, CrashSignal::Sigabrt);
        assert_eq!(expected[0].signal.number(), 6);
        assert_eq!(expected[0].command, "keel-watchdog");
        assert_eq!(expected[0].executable, None);
        assert_eq!(expected[1].signal.number(), 11);
        assert_eq!(expected[1].executable.as_deref(), Some("libdynamorio.so"));
        assert!(project.oracle.expected_crashes.is_empty());
        // Declarations serialize as authored; machines without any omit the key.
        let value = serde_json::to_value(&project).unwrap();
        assert_eq!(
            value["machines"]["s3"]["expectedCrashes"][0],
            serde_json::json!({"signal": "SIGABRT", "command": "keel-watchdog"})
        );
        assert!(value["oracle"].get("expectedCrashes").is_none());
        assert_eq!(serde_json::from_value::<Project>(value).unwrap(), project);
    }

    #[test]
    fn expected_crashes_reject_vague_or_impossible_declarations() {
        let parse = |crashes: &str| {
            serde_yaml_ng::from_str::<Machine>(&format!(
                "{{image: keel, expectedCrashes: {crashes}}}"
            ))
            .map_err(|e| e.to_string())
        };
        assert!(parse("[]").unwrap().expected_crashes.is_empty());
        for (bad, error) in [
            // Signals the kernel never reports as crashes, numbers and wildcards.
            ("[{signal: SIGKILL, command: x}]", "unknown variant `SIGKILL`"),
            ("[{signal: SIGTERM, command: x}]", "unknown variant `SIGTERM`"),
            ("[{signal: 6, command: x}]", "expected one of `SIGQUIT`"),
            ("[{signal: sigabrt, command: x}]", "unknown variant `sigabrt`"),
            ("[{signal: '*', command: x}]", "unknown variant `*`"),
            // Both the signal and the command are required.
            ("[{signal: SIGABRT}]", "missing field `command`"),
            ("[{command: keel-watchdog}]", "missing field `signal`"),
            (
                "[{signal: SIGABRT, command: ''}]",
                "expected crash command \"\" must be 1 to 15 printable ASCII bytes",
            ),
            (
                "[{signal: SIGABRT, command: keel-watchdog-thread}]",
                "expected crash command \"keel-watchdog-thread\" must be 1 to 15",
            ),
            (
                "[{signal: SIGABRT, command: \"keel\\twatch\"}]",
                "must be 1 to 15 printable ASCII bytes",
            ),
            (
                "[{signal: SIGABRT, command: x, executable: /usr/bin/keel}]",
                "expected crash executable \"/usr/bin/keel\" must be a file name without a directory",
            ),
            (
                "[{signal: SIGABRT, command: x, executable: ''}]",
                "must be a file name without a directory",
            ),
            ("[{signal: SIGABRT, command: x, pid: 1}]", "unknown field `pid`"),
            (
                "[{signal: SIGABRT, command: x}, {signal: SIGABRT, command: x}]",
                "expectedCrashes: SIGABRT in \"x\" is declared twice",
            ),
            ("{signal: SIGABRT, command: x}", "expected a sequence"),
        ] {
            let message = parse(bad).unwrap_err();
            assert!(message.contains(error), "{bad}: {message}");
        }
        let many = (0..=MAX_EXPECTED_CRASHES)
            .map(|i| format!("{{signal: SIGABRT, command: t{i}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(parse(&format!("[{many}]"))
            .unwrap_err()
            .contains("expectedCrashes: at most 16 per machine"));
        // The same thread may be expected to crash with different signals.
        assert_eq!(
            parse("[{signal: SIGABRT, command: x}, {signal: SIGSEGV, command: x}]")
                .unwrap()
                .expected_crashes
                .len(),
            2
        );
    }

    #[test]
    fn runtime_options_and_advanced_knobs_are_not_authoring_inputs() {
        for extra in [
            "limits: {wallTime: 5m}",
            "seed: 1",
            "cpu: {speed: 2GHz}",
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
            assert!(serde_yaml_ng::from_str::<Cpu>(cpu).is_err(), "{cpu}");
        }
        assert!(serde_yaml_ng::from_str::<Project>(
            "name: test\noracle: {image: oracle}\nsimulator: {instructionsPerProgressCall: 7}\n"
        )
        .is_err());
    }

    #[test]
    fn progress_client_defaults_to_standard_and_is_omitted_when_standard() {
        let parse = |extra: &str| {
            serde_yaml_ng::from_str::<Project>(&format!(
                "name: test\noracle: {{image: oracle}}\n{extra}"
            ))
        };
        assert_eq!(
            parse("").unwrap().engine.progress_client,
            ProgressClient::Standard
        );
        assert_eq!(
            parse("engine: {}\n").unwrap().engine.progress_client,
            ProgressClient::Standard
        );
        let lean = parse("engine: {progressClient: lean}\n").unwrap();
        assert_eq!(lean.engine.progress_client, ProgressClient::Lean);
        assert!(serde_yaml_ng::to_string(&lean)
            .unwrap()
            .contains("progressClient: lean"));
        let standard = parse("engine: {progressClient: standard}\n").unwrap();
        assert!(!serde_yaml_ng::to_string(&standard)
            .unwrap()
            .contains("engine"));
        for bad in [
            "engine: {progressClient: Lean}\n",
            "engine: {progressClient: ''}\n",
            "engine: {progressClient: lean, budget: 1}\n",
            "engine: lean\n",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn cpu_speed_is_a_bounded_frequency() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<Cpu>(yaml);
        assert_eq!(parse("{}").unwrap().speed_hz, 1_000_000_000);
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
            let cpu = Cpu {
                speed_hz: hz,
                jitter_percent: 0,
            };
            let value = serde_json::to_value(&cpu).unwrap();
            assert_eq!(value, serde_json::json!({"speed": text}));
            assert_eq!(serde_json::from_value::<Cpu>(value).unwrap(), cpu);
        }
    }

    #[test]
    fn cpu_jitter_is_an_optional_percentage() {
        let parse = |yaml: &str| serde_yaml_ng::from_str::<Cpu>(yaml);
        assert_eq!(parse("{}").unwrap().jitter_percent, 0);
        assert_eq!(parse("jitter: 5%").unwrap().jitter_percent, 5);
        assert_eq!(parse("jitter: '0'").unwrap().jitter_percent, 0);
        assert_eq!(parse("jitter: 100%").unwrap().jitter_percent, 100);
        for bad in ["101%", "5", "-1%", "1.5%", "5 %", "5us", "%", "''"] {
            assert!(parse(&format!("jitter: '{bad}'")).is_err(), "{bad}");
        }
        let jitter = Cpu {
            speed_hz: 2_000_000_000,
            jitter_percent: 5,
        };
        let value = serde_json::to_value(&jitter).unwrap();
        assert_eq!(value, serde_json::json!({"speed": "2GHz", "jitter": "5%"}));
        assert_eq!(serde_json::from_value::<Cpu>(value).unwrap(), jitter);
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
            ("0004GiB", 4 << 30),
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
                error.contains("expected a whole number of MiB or GiB such as 4GiB or 512MiB, from 16MiB to 64GiB"),
                "{bad}: {error}"
            );
        }
        assert!(parse("{target: /data, size: true}")
            .unwrap_err()
            .to_string()
            .contains("expected a size such as 4GiB"));
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
        assert_eq!(
            machine_disk_bytes([Some(3 << 30), None]),
            Ok(4 << 30),
            "an omitted size takes the 1GiB default"
        );
        assert_eq!(machine_disk_bytes([Some(32 << 30); 2]), Ok(64 << 30));
        assert_eq!(
            machine_disk_bytes([Some(32 << 30), Some(32 << 30), Some(16 << 20)]),
            Err("volume sizes total 65552MiB, above the 64GiB limit for one machine".into())
        );
        assert_eq!(
            machine_disk_bytes([Some(u64::MAX)]),
            Err("volume size 18446744073709551615B must be a whole number of MiB from 16MiB to 64GiB".into())
        );
        assert!(machine_disk_bytes([Some((1 << 30) + 512)]).is_err());
        assert!(machine_disk_bytes([Some(0)]).is_err());
    }

    #[test]
    fn progress_charge_follows_speed_and_jitter() {
        let cpu = |speed_hz, jitter_percent| Cpu {
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
