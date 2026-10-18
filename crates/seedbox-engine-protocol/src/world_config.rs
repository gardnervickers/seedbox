//! The world document (`world.json`): what the CLI asks an engine to run.
//!
//! The CLI builds a [`WorldConfig`] from verified execution inputs and the
//! x86 engine deserializes the same type. Standalone fixtures
//! (`harness/worlds/*.json`) are world documents too. Unknown top-level keys
//! (such as a fixture's `_comment`) are ignored; nested objects are strict.
//! Relative paths resolve against the document's directory.

use crate::disk::DiskFaultModel;
pub const DEFAULT_LATENCY_NS: u64 = 1_000_000;
use seedbox_control::Plan;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Guest memory when a document names none.
pub const DEFAULT_MEM_MB: u64 = 512;
/// Simulated time limit when a document names none.
pub const DEFAULT_DURATION_NS: u64 = 60_000_000_000;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldConfig {
    pub kernel: PathBuf,
    #[serde(default = "default_mem_mb")]
    pub mem_mb: u64,
    #[serde(default = "default_duration_ns")]
    pub duration_ns: u64,
    /// End the run when this machine exits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_on_exit: Option<String>,
    #[serde(default)]
    pub timer_jitter_ns: u64,
    #[serde(default)]
    pub stop_tolerance_ns: u64,
    /// Exact timer frequency error in parts per million; absent is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exact_timer_ppm: Option<u64>,
    /// Absent: the machines have no network device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkConfig>,
    pub machines: Vec<MachineConfig>,
    /// Runtime control. Always written, as `null` when absent, because the
    /// ARM engine's document requires the key.
    #[serde(default)]
    pub control: Option<ControlConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_channel: Option<ControlChannel>,
    /// The initial launch barrier's skew bound. Read by the ARM engine only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_skew_ns: Option<u64>,
    /// Torn-write crash policy; absent drops every unflushed sector.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_loss: Option<DiskFaultModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointConfig>,
    /// Boot once under `boot.seed` and fork the run seeds from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<BootConfig>,
    /// Static fault plans, by id from 1.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_program: Vec<crate::input_program::TimedInput>,
    /// Timed serial lines and legacy partitions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scenario: Vec<ScenarioEvent>,
    /// A seeded unclean stop/restart schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kill_restart: Option<KillRestart>,
}

/// The spec's default CPU speed, for world documents written before it existed.
fn default_cpu_speed_hz() -> u64 {
    1_000_000_000
}

fn default_mem_mb() -> u64 {
    DEFAULT_MEM_MB
}
fn default_duration_ns() -> u64 {
    DEFAULT_DURATION_NS
}
fn default_latency_ns() -> u64 {
    DEFAULT_LATENCY_NS
}

/// The global network policy and per-pair baseline overrides.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConfig {
    #[serde(default = "default_latency_ns")]
    pub latency_ns: u64,
    #[serde(default)]
    pub jitter_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
    /// Absent unless used, so a world without overrides is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<LinkConfig>,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            latency_ns: DEFAULT_LATENCY_NS,
            jitter_ns: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            links: Vec::new(),
        }
    }
}

/// A directed baseline policy between two machines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkConfig {
    pub from: String,
    pub to: String,
    pub latency_ns: u64,
    #[serde(default)]
    pub jitter_ns: u64,
    #[serde(default)]
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_bound_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    /// RAM override in MiB; omission inherits WorldConfig.mem_mb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_mb: Option<u64>,
    /// This machine's CPU speed in Hz; a throttle is a lower speed than this.
    #[serde(default = "default_cpu_speed_hz")]
    pub cpu_speed_hz: u64,
    /// Nominal virtual time the guest kernel charges each PV progress call:
    /// the instructions between calls at the configured CPU speed. Zero
    /// charges a call like one syscall.
    #[serde(default)]
    pub progress_ns: u64,
    /// Bound on the seeded variation of that charge, each way, drawn from a
    /// per-machine stream; zero charges every call the same.
    #[serde(default)]
    pub progress_jitter_ns: u64,
    pub name: String,
    pub initrd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskConfig>,
    /// Appended to the generated kernel command line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmdline: Option<String>,
    /// Crashes the simulation definition declares as designed behavior here;
    /// they pass `seedbox.no_crashing_processes` instead of violating it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expected_crashes: Vec<crate::property::ExpectedCrash>,
}

impl MachineConfig {
    pub fn memory_mb(&self, world: &WorldConfig) -> u64 {
        self.mem_mb.unwrap_or(world.mem_mb)
    }
}

/// `{"size_mib": N}` for a zeroed disk, or `{"image": PATH}` for a raw image
/// (with `size_mib`, it must be that size).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<PathBuf>,
}

/// The runtime controller: its machine, its reply latency and the service
/// placement that resolves service targets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    pub machine: String,
    pub latency_ns: u64,
    pub placement: BTreeMap<String, String>,
}

/// The guest transport that carries runtime control.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlChannel {
    #[serde(rename = "virtio-console-v1")]
    VirtioConsoleV1,
}

/// A runtime checkpoint and the branch run from it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    pub at_ns: u64,
    pub branch_seed: u64,
    /// When present, the engine checks it against its own inputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_identity: Option<String>,
}

/// Boot under a fixed seed and release run seeds from the booted world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootConfig {
    pub seed: u64,
    #[serde(default)]
    pub release_skew_ns: u64,
    /// Fork at this simulated time instead of the guests' launch barrier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_at_ns: Option<u64>,
}

/// A timed scenario event: a serial line to a machine and/or a legacy
/// partition between two machines.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScenarioEvent {
    pub at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// The first two names are the two sides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<Vec<String>>,
    /// Cut (true) or heal (false) the partition.
    #[serde(default = "default_on")]
    pub on: bool,
}

fn default_on() -> bool {
    true
}

/// Stop and restart one member at a time: gaps and downtimes are drawn from
/// inclusive `[min, max]` ranges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillRestart {
    pub members: Vec<String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub gap_ns: [u64; 2],
    pub down_ns: [u64; 2],
}

/// The parts of a world that the run seed arms at its release.
#[derive(Clone, Copy, Debug, Default)]
pub struct SeedPlan<'a> {
    pub faults: &'a [Plan],
    pub input_program: &'a [crate::input_program::TimedInput],
    pub scenario: &'a [ScenarioEvent],
    pub kill_restart: Option<&'a KillRestart>,
    pub control: Option<&'a ControlConfig>,
}

impl WorldConfig {
    /// Each machine's expected crashes, in machine order. The world
    /// coordinator validates them.
    pub fn expected_crashes(&self) -> Vec<Vec<crate::property::ExpectedCrash>> {
        self.machines
            .iter()
            .map(|m| m.expected_crashes.clone())
            .collect()
    }

    pub fn seed_plan(&self) -> SeedPlan<'_> {
        SeedPlan {
            faults: &self.faults,
            input_program: &self.input_program,
            scenario: &self.scenario,
            kill_restart: self.kill_restart.as_ref(),
            control: self.control.as_ref(),
        }
    }
}

/// The seed-plan keys of a world document, for callers that hold it as JSON.
#[derive(Default, Deserialize)]
pub struct SeedPlanConfig {
    #[serde(default)]
    pub faults: Vec<Plan>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_program: Vec<crate::input_program::TimedInput>,
    #[serde(default)]
    pub scenario: Vec<ScenarioEvent>,
    #[serde(default)]
    pub kill_restart: Option<KillRestart>,
    #[serde(default)]
    pub control: Option<ControlConfig>,
}

impl SeedPlanConfig {
    /// Read the seed plan from a world document held as JSON.
    pub fn from_value(cfg: &serde_json::Value) -> Result<Self, String> {
        if cfg.is_null() {
            return Ok(Self::default());
        }
        Self::deserialize(cfg).map_err(|e| format!("world plan: {e}"))
    }
    pub fn plan(&self) -> SeedPlan<'_> {
        SeedPlan {
            faults: &self.faults,
            input_program: &self.input_program,
            scenario: &self.scenario,
            kill_restart: self.kill_restart.as_ref(),
            control: self.control.as_ref(),
        }
    }
}

impl SeedPlan<'_> {
    /// Every timed part of the run seed's plan (faults, scenario events and
    /// the kill schedule) must start at or after `from`, when every machine
    /// of a forked world has been released. Earlier times have already passed.
    pub fn check_after(&self, from: u64) -> Result<(), String> {
        let faults = self.faults.iter().enumerate();
        let scenario = self.scenario.iter().enumerate();
        let times = faults
            .map(|(i, f)| (format!("faults[{i}].at_ns"), f.at_ns))
            .chain(
                self.input_program
                    .iter()
                    .map(|input| (format!("input_program.{}.at_ns", input.key), input.at_ns)),
            )
            .chain(scenario.map(|(i, e)| (format!("scenario[{i}].at_ns"), e.at_ns)))
            .chain(
                self.kill_restart
                    .map(|k| ("kill_restart.start_ns".to_owned(), k.start_ns)),
            );
        for (what, t) in times {
            if t < from {
                return Err(format!(
                    "{what} {t} is before the fork point {from}; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)"
                ));
            }
        }
        Ok(())
    }

    /// Whether the plan has static faults, scenario events or a kill schedule.
    pub fn has_static_events(&self) -> bool {
        !self.faults.is_empty() || !self.scenario.is_empty() || self.kill_restart.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_harness_world_fixture_deserializes() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/worlds");
        let mut count = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "json") {
                let bytes = std::fs::read(&path).unwrap();
                let cfg: WorldConfig = serde_json::from_slice(&bytes)
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                assert!(!cfg.machines.is_empty(), "{}", path.display());
                // A typed round trip is lossless.
                let again: WorldConfig =
                    serde_json::from_slice(&serde_json::to_vec(&cfg).unwrap()).unwrap();
                assert_eq!(again, cfg, "{}", path.display());
                count += 1;
            }
        }
        assert!(count >= 20, "found {count} fixtures in {}", dir.display());
    }

    #[test]
    fn defaults_fill_omitted_keys_and_nested_objects_are_strict() {
        let cfg: WorldConfig = serde_json::from_str(
            r#"{"kernel":"k","machines":[{"name":"a","initrd":"i"}],"_comment":"ignored"}"#,
        )
        .unwrap();
        assert_eq!(cfg.mem_mb, 512);
        assert_eq!(cfg.duration_ns, 60_000_000_000);
        assert_eq!((cfg.stop_tolerance_ns, cfg.timer_jitter_ns), (0, 0));
        assert!(cfg.network.is_none() && cfg.control.is_none() && cfg.boot.is_none());
        let network: NetworkConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(network, NetworkConfig::default());
        assert_eq!(network.latency_ns, DEFAULT_LATENCY_NS);
        for bad in [
            r#"{"kernel":"k","machines":[{"name":"a","initrd":"i","extra":1}]}"#,
            r#"{"kernel":"k","machines":[{"name":"a","initrd":"i","disk":{"size":1}}]}"#,
            r#"{"kernel":"k","machines":[],"control_channel":"serial"}"#,
            r#"{"kernel":"k","machines":[],"network":{"latency":1}}"#,
        ] {
            assert!(serde_json::from_str::<WorldConfig>(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn per_machine_memory_is_optional_in_legacy_worlds() {
        let world: WorldConfig = serde_json::from_value(serde_json::json!({
            "kernel":"kernel", "mem_mb":256,
            "machines":[{"name":"app","initrd":"app","mem_mb":2048},{"name":"oracle","initrd":"oracle"}]
        })).unwrap();
        assert_eq!(world.machines[0].memory_mb(&world), 2048);
        assert_eq!(world.machines[1].memory_mb(&world), 256);
        let raw = serde_json::to_value(&world).unwrap();
        assert_eq!(raw["machines"][0]["mem_mb"], 2048);
        assert!(raw["machines"][1].get("mem_mb").is_none());
        assert_eq!(serde_json::from_value::<WorldConfig>(raw).unwrap(), world);
    }

    #[test]
    fn expected_crashes_are_per_machine_and_omitted_when_empty() {
        let world: WorldConfig = serde_json::from_value(serde_json::json!({
            "kernel":"kernel",
            "machines":[
                {"name":"s3","initrd":"s3","expected_crashes":[{"signal":6,"command":"keel-watchdog"}]},
                {"name":"oracle","initrd":"oracle"}
            ]
        }))
        .unwrap();
        let expected = world.expected_crashes();
        assert_eq!(expected.len(), 2);
        assert_eq!(expected[0][0].command, "keel-watchdog");
        assert!(expected[1].is_empty());
        let raw = serde_json::to_value(&world).unwrap();
        assert_eq!(raw["machines"][0]["expected_crashes"][0]["signal"], 6);
        assert!(raw["machines"][1].get("expected_crashes").is_none());
        assert_eq!(serde_json::from_value::<WorldConfig>(raw).unwrap(), world);
    }

    #[test]
    fn control_is_written_as_null_and_unused_keys_are_omitted() {
        let cfg = WorldConfig {
            kernel: "k".into(),
            mem_mb: 512,
            duration_ns: 1,
            end_on_exit: None,
            timer_jitter_ns: 0,
            stop_tolerance_ns: 0,
            exact_timer_ppm: None,
            network: None,
            machines: vec![],
            control: None,
            control_channel: None,
            launch_skew_ns: None,
            disk_loss: None,
            checkpoint: None,
            boot: None,
            faults: vec![],
            input_program: vec![],
            scenario: vec![],
            kill_restart: None,
        };
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            serde_json::json!({"kernel":"k","mem_mb":512,"duration_ns":1,"timer_jitter_ns":0,
                "stop_tolerance_ns":0,"machines":[],"control":null})
        );
    }

    #[test]
    fn plan_times_before_the_fork_point_are_rejected() {
        let plan: SeedPlanConfig = serde_json::from_value(serde_json::json!({
            "faults":[{"id":1,"at_ns":1_000_000_000,"deadline_ns":1_000_000_000,"action":"crash","machine":"a"}],
            "scenario":[{"at_ns":1_500_000_000,"serial":"x\n","machine":"a"}],
            "kill_restart":{"members":["a"],"start_ns":2_000_000_000,"end_ns":3_000_000_000u64,
                "gap_ns":[1,2],"down_ns":[1,2]},
        }))
        .unwrap();
        let mut view = plan.plan();
        assert_eq!(view.check_after(1_400_000_000).unwrap_err(),
            "faults[0].at_ns 1000000000 is before the fork point 1400000000; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)");
        assert!(view.check_after(1_000_000_000).is_ok());
        view.faults = &[];
        assert!(view
            .check_after(1_600_000_000)
            .unwrap_err()
            .starts_with("scenario[0].at_ns 1500000000 "));
        view.scenario = &[];
        assert!(view
            .check_after(2_000_000_001)
            .unwrap_err()
            .starts_with("kill_restart.start_ns 2000000000 "));
        assert!(SeedPlan::default().check_after(u64::MAX).is_ok());
    }
}
