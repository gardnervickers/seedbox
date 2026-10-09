//! Contracts between orchestration, image preparation, and execution.
//!
//! Backends must enforce simulated/wall limits, honor cancellation, isolate their
//! output, and release any host reservation after stopping guests. A hardware
//! backend must acquire the shared host reservation *before* pinning a guest
//! to a core, and release it on normal return and catchable interruption. Neither
//! a preparer nor the orchestrator is permitted to perform these host mutations.

use crate::{
    config::{Check, Limits, Network, Schedule, Simulation},
    image::Launch,
    store::{ContentDigest, ContentStore},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    pub backend: String,
    /// Actual runner/VMM binary bytes, not just a version string.
    pub engine: ContentDigest,
    pub witness_mode: WitnessMode,
    /// Semantic settings (e.g. kernel ABI). Schedule controls live in Simulation.
    pub semantic_parameters: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessMode {
    Exits,
    FullState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedImage {
    /// Platform-specific manifest, not a multi-platform index or mutable tag.
    pub manifest: ContentDigest,
    pub config: ContentDigest,
    pub layers: Vec<ContentDigest>,
    pub launch: Launch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedInputs {
    /// Guest CLOCK_REALTIME at the launch barrier, sampled once during preparation.
    /// Historical prepared inputs omit this and retain their original boot bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_epoch: Option<u32>,
    pub kernel: ContentDigest,
    pub images: BTreeMap<String, PreparedImage>,
    /// Every service maps to one machine; services may share a machine.
    pub placement: BTreeMap<String, String>,
    /// Machine name to prepared boot filesystem/archive and launch helpers.
    pub guests: BTreeMap<String, ContentDigest>,
    pub profile: ExecutionProfile,
    /// Config files, source/build snapshots, and other replay dependencies.
    pub auxiliary: BTreeMap<String, ContentDigest>,
    /// Build/source identity (e.g. context hash, Dockerfile hash, builder version).
    pub provenance: BTreeMap<String, String>,
}

impl PreparedInputs {
    pub fn digests(&self) -> impl Iterator<Item = &ContentDigest> {
        [&self.kernel, &self.profile.engine]
            .into_iter()
            .chain(self.auxiliary.values())
            .chain(self.guests.values())
            .chain(self.images.values().flat_map(|image| {
                [&image.manifest, &image.config]
                    .into_iter()
                    .chain(image.layers.iter())
            }))
    }
}

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

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    parent: Option<CancellationToken>,
}
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<CancellationState>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
            || self.0.parent.as_ref().is_some_and(Self::is_cancelled)
    }
    /// Internal failures cancel owned work without changing the caller token;
    /// external cancellation remains directly observable without a polling race.
    pub(crate) fn owned(&self) -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            parent: Some(self.clone()),
        }))
    }
}

/// Only materialized, verified inputs reach the executor. Original Compose paths,
/// registry references, and build contexts are deliberately absent.
pub struct ExecutionRequest<'a> {
    pub seed: u64,
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
    /// Execute requests that differ only in seed and output, in order, with
    /// one result per request. A backend that records a shared boot may boot
    /// once and fork each seed from it; every result must equal executing that
    /// request alone. The default executes them one at a time.
    fn execute_forked(&self, requests: Vec<ExecutionRequest<'_>>) -> Vec<Result<ExecutionReport>> {
        requests
            .into_iter()
            .map(|request| self.execute(request))
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum StopReason {
    Completed,
    SimulatedTimeLimit,
    WallTimeLimit,
    Cancelled,
    Quiescent,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkloadExit {
    pub service: String,
    pub code: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultEvent {
    pub requested_at_ns: u64,
    /// Guest virtual time of the acknowledged effect, not request/receipt time.
    pub applied_at_ns: u64,
    /// Empty for network effects, which have no single service target.
    pub service: String,
    pub action: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionReport {
    pub reason: StopReason,
    pub workload_exit: Option<WorkloadExit>,
    pub witness: Option<ContentDigest>,
    /// None means no full-state comparison is available. Never imply that an
    /// exit witness proves equality of guest RAM/registers.
    pub full_state: Option<BTreeMap<String, ContentDigest>>,
    pub faults: Vec<FaultEvent>,
    pub detail: Option<String>,
}

#[cfg(test)]
mod cancellation_tests {
    use super::CancellationToken;
    #[test]
    fn owned_cancellation_is_isolated_and_parent_cancellation_is_immediate() {
        let caller = CancellationToken::default();
        let first = caller.owned();
        let sibling = caller.owned();
        first.cancel();
        assert!(first.is_cancelled());
        assert!(!caller.is_cancelled() && !sibling.is_cancelled());
        let nested = sibling.owned();
        assert!(!nested.is_cancelled());
        caller.cancel();
        assert!(sibling.is_cancelled() && nested.is_cancelled());
    }
}
