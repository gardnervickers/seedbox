//! Contracts between orchestration, image preparation, and execution.
//!
//! Backends must enforce simulated/wall limits, honor cancellation, isolate their
//! output, and release any host reservation after stopping guests. A hardware
//! backend must acquire the shared host reservation *before* pinning a guest
//! to a core, and release it on normal return and catchable interruption. Neither
//! a preparer nor the orchestrator is permitted to perform these host mutations.

use crate::{
    config::{Check, Limits, Network, Schedule, Simulation},
    store::{ContentDigest, ContentStore},
    Result,
};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use seedbox_artifacts::execution::*;

/// Resolves/builds images and retains their entire input closure in `store`.
/// No guest execution, CPU pinning, or host-state mutation is allowed here.
pub trait PreparationBackend: Send + Sync {
    /// Fast immutable runtime input checks, before any OCI work.
    fn validate_runtime_inputs(&self) -> Result<()> {
        Ok(())
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs>;
    /// Exploration preparation owns diagnostics independently of scratch inputs.
    /// Backends must explicitly support cancellation; the default fails before work.
    fn prepare_cancellable(
        &self,
        _simulation: &Simulation,
        _store: &ContentStore,
        _context: &crate::preparation::PreparationContext,
    ) -> Result<PreparedInputs> {
        Err(crate::Error::Setup(
            "preparation backend does not support exploration cancellation".into(),
        ))
    }
}

/// Only materialized, verified inputs reach the executor. Original Compose paths,
/// registry references, and build contexts are deliberately absent.
pub struct ExecutionRequest<'a> {
    /// Initial host playback state; never part of guest experiment identity.
    pub start_paused: bool,
    /// Saved engine position to restore before continuing this simulation.
    pub resume_checkpoint: Option<&'a Path>,
    /// Optional durable semantic-input journal, read after the worker stops.
    pub intervention_history: Option<&'a Path>,
    /// Verified retained inputs to reapply, distinct from the live output journal.
    pub replay_interventions: Option<&'a seedbox_engine_protocol::intervention::History>,
    /// Explicit server-owned observation socket, outside retained experiment identity.
    pub observations: Option<&'a Path>,
    pub seed: u64,
    pub program: &'a [seedbox_engine_protocol::input_program::TimedInput],
    pub limits: &'a Limits,
    pub network: &'a Network,
    pub schedule: &'a Schedule,
    pub control: Option<&'a crate::config::Control>,
    pub check: &'a Check,
    pub prepared: &'a PreparedInputs,
    pub artifacts: &'a BTreeMap<ContentDigest, PathBuf>,
    pub output: &'a Path,
    pub cancellation: &'a CancellationToken,
    /// Recorded per attempt, excluded from semantic identity/profile comparison.
    pub performance_parameters: &'a BTreeMap<String, String>,
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
    /// Admission proof and inputs must be checked even for old retained records.
    fn validate_retained(&self, _inputs: &PreparedInputs) -> Result<()> {
        Ok(())
    }
    /// Reject an unsupported resume from bounded metadata before any retained
    /// execution inputs are materialized. Legacy adapters keep their own checks.
    fn preflight_resume(
        &self,
        _checkpoint: Option<&Path>,
        _control: Option<&crate::config::Control>,
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
}
