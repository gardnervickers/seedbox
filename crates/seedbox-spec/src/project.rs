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
    /// Crashes this machine is designed to have, which pass the default
    /// `seedbox.no_crashing_processes` property instead of violating it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub expected_crashes: Vec<ExpectedCrash>,
    /// This machine's out-of-memory kills are designed behaviour: they pass
    /// the default `seedbox.no_out_of_memory_kills` property instead of
    /// violating it.
    #[serde(
        rename = "expectedOutOfMemoryKills",
        skip_serializing_if = "std::ops::Not::not"
    )]
    pub expected_out_of_memory_kills: bool,
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
    expected_crashes: Vec<ExpectedCrash>,
    #[serde(default, rename = "expectedOutOfMemoryKills")]
    expected_out_of_memory_kills: bool,
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
            expected_out_of_memory_kills: fields.expected_out_of_memory_kills,
        })
    }
}

/// A crash a machine is designed to have, such as a watchdog that aborts a
/// stalled process for a supervisor to restart. It matches only the guest
/// kernel's fatal-signal report (`print-fatal-signals`) of `signal` from a
/// thread whose command name is `command` and, when given, whose executable's
/// file name is `executable`. A matching crash is a passing evaluation of
/// `seedbox.no_crashing_processes` that names the declaration; every other
/// crash, and every out-of-memory kill, is reported as without it.
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
    fn expected_out_of_memory_kills_are_a_per_machine_flag() {
        let machine: Machine =
            serde_yaml_ng::from_str("{image: hog, expectedOutOfMemoryKills: true}").unwrap();
        assert!(machine.expected_out_of_memory_kills);
        assert_eq!(
            serde_json::to_value(&machine).unwrap()["expectedOutOfMemoryKills"],
            true
        );
        let plain: Machine = serde_yaml_ng::from_str("{image: app}").unwrap();
        assert!(!plain.expected_out_of_memory_kills);
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("expectedOutOfMemoryKills")
            .is_none());
        for bad in [
            "{image: hog, expectedOutOfMemoryKills: 2}",
            "{image: hog, expectedOomKills: true}",
        ] {
            assert!(serde_yaml_ng::from_str::<Machine>(bad).is_err(), "{bad}");
        }
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
}
