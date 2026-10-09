//! The request that launches an x86 world worker, and the machine features it
//! selects. The CLI writes it as JSON; the internal worker reads it back, and
//! the standalone VMM builds the same value from its flags.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Initial undo margin in branches when none is chosen.
pub const DEFAULT_UNDO_MARGIN: u64 = 256;
/// The margin setting that selects adaptive stepping.
pub const ADAPTIVE_MARGIN: &str = "adaptive";
/// Host CPU time a PV run may go without an exit before it is checked for a spin.
pub const DEFAULT_PV_WATCHDOG_MS: u64 = 20;
/// The most machine threads a windowed world may use.
pub const MAX_MACHINE_WORKERS: usize = 64;

/// How guest progress advances virtual time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Timebase {
    /// Count retired guest branches with the host PMU.
    #[default]
    Counter,
    /// Advance time at guest exits, syscalls and clock reads.
    Pv,
}

impl Timebase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Counter => "counter",
            Self::Pv => "pv",
        }
    }

    /// Decode a recorded or command-line name.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "counter" => Some(Self::Counter),
            "pv" => Some(Self::Pv),
            _ => None,
        }
    }
}

/// Run-shaping machine features. They change results and are recorded in a
/// run's identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineFeatures {
    pub timebase: Timebase,
    /// Program the guest timer through the time page instead of the port.
    pub page_timer: bool,
    /// Defer virtio-net TX kicks to deterministic polls.
    pub deferred_tx: bool,
}

impl MachineFeatures {
    /// The machine runs under the PV clock (no host perf counter).
    pub fn pv(self) -> bool {
        self.timebase == Timebase::Pv
    }
}

/// Performance choices and diagnostics; they never change guest results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Performance {
    /// Initial final margin for undo-able precise stops; 0 disables them.
    pub undo_margin: u64,
    /// A fixed stepping margin in branches; None adapts it.
    pub margin: Option<u64>,
    pub pv_watchdog_ms: u64,
    /// Windowed scheduler testing only: delay machine i's runs by
    /// (machines - i) times this many microseconds, so later machines finish first.
    pub windowed_skew_us: u64,
}

impl Default for Performance {
    fn default() -> Self {
        Self {
            undo_margin: DEFAULT_UNDO_MARGIN,
            margin: None,
            pv_watchdog_ms: DEFAULT_PV_WATCHDOG_MS,
            windowed_skew_us: 0,
        }
    }
}

/// Decode a margin setting: [`ADAPTIVE_MARGIN`] or a branch count.
pub fn parse_margin(value: &str) -> Result<Option<u64>, String> {
    if value == ADAPTIVE_MARGIN {
        return Ok(None);
    }
    value
        .parse()
        .map(Some)
        .map_err(|_| format!("margin {value}: expected {ADAPTIVE_MARGIN} or a branch count"))
}

/// Everything an x86 world worker runs from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerRequest {
    /// The world JSON; relative paths inside it resolve against its directory.
    pub config: PathBuf,
    pub seed: u64,
    /// Further seeds released from the same booted world after `seed`.
    pub fork_seeds: Vec<u64>,
    /// Directory for consoles, logs, traces and page hashes.
    pub out: PathBuf,
    /// Write full-state page digests.
    pub digest: bool,
    pub host_control: Option<PathBuf>,
    /// Bytes retained per console stream.
    pub console_limit: Option<usize>,
    /// Chunk index entries retained per console stream.
    pub log_chunk_limit: Option<usize>,
    pub features: MachineFeatures,
    /// Runtime checkpoints, strictly increasing; empty for none.
    pub checkpoint_at_ns: Vec<u64>,
    /// Machine threads for the windowed scheduler (performance only;
    /// docs/windowed-scheduler.md).
    pub machine_workers: usize,
    /// One CPU per machine thread, in thread order (performance only).
    pub machine_cpus: Vec<usize>,
    /// Write raw console logs while the world runs.
    pub stream_console: bool,
    /// Write per-machine execution traces.
    pub trace: bool,
    pub performance: Performance,
}

impl WorkerRequest {
    /// A request with every optional setting at its default.
    pub fn new(config: PathBuf, seed: u64, out: PathBuf) -> Self {
        Self {
            config,
            seed,
            fork_seeds: Vec::new(),
            out,
            digest: false,
            host_control: None,
            console_limit: None,
            log_chunk_limit: None,
            features: MachineFeatures::default(),
            checkpoint_at_ns: Vec::new(),
            machine_workers: 1,
            machine_cpus: Vec::new(),
            stream_console: false,
            trace: false,
            performance: Performance::default(),
        }
    }

    /// Check settings that depend on each other or have bounded ranges.
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_MACHINE_WORKERS).contains(&self.machine_workers) {
            return Err(format!("--machine-workers: use 1..{MAX_MACHINE_WORKERS}"));
        }
        if !self.machine_cpus.is_empty() && self.machine_cpus.len() != self.machine_workers {
            return Err("--machine-cpus: give one CPU per machine worker".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_and_rejects_unknown_fields() {
        let mut request = WorkerRequest::new("world.json".into(), 7, "out".into());
        request.features = MachineFeatures {
            timebase: Timebase::Pv,
            page_timer: true,
            deferred_tx: true,
        };
        request.checkpoint_at_ns = vec![1, 2];
        request.performance.margin = Some(64);
        let bytes = serde_json::to_vec(&request).unwrap();
        let back: WorkerRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, request);
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["features"]["timebase"], "pv");
        value["unknown"] = true.into();
        assert!(serde_json::from_value::<WorkerRequest>(value).is_err());
    }

    #[test]
    fn margins_and_worker_bounds() {
        assert_eq!(parse_margin("adaptive"), Ok(None));
        assert_eq!(parse_margin("64"), Ok(Some(64)));
        assert!(parse_margin("invalid").is_err());
        let mut request = WorkerRequest::new("w".into(), 0, "o".into());
        request.validate().unwrap();
        request.machine_workers = 0;
        assert!(request.validate().is_err());
        request.machine_workers = 2;
        request.machine_cpus = vec![1];
        assert!(request.validate().is_err());
        request.machine_cpus = vec![1, 2];
        request.validate().unwrap();
        assert_eq!(Timebase::from_name("pv"), Some(Timebase::Pv));
        assert_eq!(Timebase::from_name("PV"), None);
    }
}
