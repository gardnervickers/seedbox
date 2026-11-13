//! Explicit execution request and backend interface for retained Driver execution.
pub use seedbox_artifacts::evidence::MACHINE_OUTPUT_DIR;
pub use seedbox_artifacts::execution::*;
use seedbox_artifacts::{
    config::{Check, Limits, Network, Schedule},
    store::ContentDigest,
    Result,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
pub const VMM_FAILURE_MESSAGE: &str = "VMM failed; see vmm.stderr";
/// Only materialized, verified inputs reach the executor. Original Compose paths,
/// registry references, and build contexts are deliberately absent.
pub struct ExecutionRequest<'a> {
    /// Initial host playback state; never part of guest experiment identity.
    pub start_paused: bool,
    /// Saved engine position to restore before continuing this simulation.
    pub resume_checkpoint: Option<&'a Path>,
    /// Optional durable semantic-input journal, read after the worker stops.
    pub intervention_history: Option<&'a Path>,
    /// Explicit server-owned observation socket, outside retained experiment identity.
    pub observations: Option<&'a Path>,
    pub seed: u64,
    /// Fixed simulated timeslice, retained in the experiment configuration.
    pub quantum_ns: std::num::NonZeroU64,
    /// Translated blocks between PV progress calls, retained in the configuration.
    pub progress_budget: seedbox_spec::progress::ProgressBudget,
    /// Machines whose crash deaths and out-of-memory kills are not violations.
    pub allow_crashes: &'a BTreeSet<String>,
    pub program: &'a [seedbox_engine_protocol::input_program::TimedInput],
    pub limits: &'a Limits,
    pub network: &'a Network,
    pub schedule: &'a Schedule,
    pub control: Option<&'a seedbox_artifacts::config::Control>,
    pub check: &'a Check,
    pub prepared: &'a PreparedInputs,
    pub artifacts: &'a BTreeMap<ContentDigest, PathBuf>,
    pub output: &'a Path,
    pub cancellation: &'a CancellationToken,
    /// Recorded per attempt, excluded from semantic identity/profile comparison.
    pub performance_parameters: &'a BTreeMap<String, String>,
    /// Prepares machines the oracle spawns that `prepared` does not retain.
    /// Without one, such a spawn ends the run with an error.
    pub spawner: Option<&'a dyn Spawner>,
}

/// Prepares the machine an oracle spawn asks for, while its run waits.
pub trait Spawner: Send + Sync {
    /// Prepare `request`, writing any files under `directory`, which lasts as
    /// long as the attempt's output.
    fn prepare(
        &self,
        request: &seedbox_engine_protocol::driver_world::SpawnRequest,
        directory: &Path,
    ) -> Result<(
        seedbox_engine_protocol::driver_world::Machine,
        seedbox_engine_protocol::driver_world::Environment,
    )>;
}

pub trait ExecutionBackend: Send + Sync {
    /// Admit the requested exploration capacity before preparation. Backends are
    /// serial unless they explicitly support owned concurrent executions.
    fn preflight_workers(&self, workers: u32) -> Result<()> {
        if workers != 1 {
            return Err(crate::Error::Setup(
                "--workers: execution backend supports only one worker".into(),
            ));
        }
        self.preflight(None)
    }
    /// Readiness check; all transient host resources must close before return.
    fn preflight(&self, _output: Option<&Path>) -> Result<()> {
        Ok(())
    }
    /// Check the admission proof and retained inputs before they are used.
    fn validate_retained(&self, _inputs: &PreparedInputs) -> Result<()> {
        Ok(())
    }
    /// Reject an unsupported resume from bounded metadata before any retained
    /// execution inputs are materialized.
    fn preflight_resume(
        &self,
        _checkpoint: Option<&Path>,
        _control: Option<&seedbox_artifacts::config::Control>,
    ) -> Result<()> {
        Ok(())
    }
    /// The retained inputs execution reads; only these are copied into each
    /// attempt and reach `ExecutionRequest::artifacts`. Default: all of them.
    fn runtime_inputs<'a>(&self, inputs: &'a PreparedInputs) -> Vec<&'a ContentDigest> {
        inputs.digests().collect()
    }
    /// Must exactly match the recorded profile before execution can begin.
    fn profile(&self) -> ExecutionProfile;
    /// Only knobs whose witness invariance has been qualified belong here.
    fn performance_parameters(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport>;
    /// Add the machines this attempt's spawns prepared to `inputs`, so a
    /// replay builds them without preparing them again. Returns whether
    /// anything was added.
    fn retain_spawned(
        &self,
        _inputs: &mut PreparedInputs,
        _output: &Path,
        _store: &seedbox_artifacts::store::ContentStore,
    ) -> Result<bool> {
        Ok(false)
    }
}
