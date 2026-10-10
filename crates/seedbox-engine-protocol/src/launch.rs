//! The request that launches an x86 world worker, and the machine features it
//! selects. The CLI writes it as JSON; the internal worker reads it back, and
//! the standalone VMM builds the same value from its flags.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Host CPU time a run may go without an exit before it is checked for a spin.
pub const DEFAULT_WATCHDOG_MS: u64 = 20;
/// The most machine threads a windowed world may use.
pub const MAX_MACHINE_WORKERS: usize = 64;

/// Run-shaping machine features. They change results and are recorded in a
/// run's identity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineFeatures {
    /// Program the guest timer through the time page instead of the port.
    pub page_timer: bool,
    /// Defer virtio-net TX kicks to deterministic polls.
    pub deferred_tx: bool,
}

/// Performance choices and diagnostics; they never change guest results.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Performance {
    /// Host CPU milliseconds between watchdog observations of a run with no exit.
    pub watchdog_ms: u64,
}

impl Default for Performance {
    fn default() -> Self {
        Self {
            watchdog_ms: DEFAULT_WATCHDOG_MS,
        }
    }
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
    /// Test hook for the windowed scheduler, never a performance setting:
    /// delay machine i's runs by (machines - i) times this many microseconds,
    /// so later machines finish first and the result must not depend on it.
    pub windowed_skew_us: u64,
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
            windowed_skew_us: 0,
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
            page_timer: true,
            deferred_tx: true,
        };
        request.checkpoint_at_ns = vec![1, 2];
        request.performance.watchdog_ms = 7;
        let bytes = serde_json::to_vec(&request).unwrap();
        let back: WorkerRequest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back, request);
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["features"]["deferred_tx"], true);
        value["unknown"] = true.into();
        assert!(serde_json::from_value::<WorkerRequest>(value).is_err());
    }

    #[test]
    fn worker_bounds() {
        let mut request = WorkerRequest::new("w".into(), 0, "o".into());
        request.validate().unwrap();
        request.machine_workers = 0;
        assert!(request.validate().is_err());
        request.machine_workers = 2;
        request.machine_cpus = vec![1];
        assert!(request.validate().is_err());
        request.machine_cpus = vec![1, 2];
        request.validate().unwrap();
    }
}
