//! Contracts between orchestration, image preparation, and execution.
//!
//! Backends must enforce simulated/wall limits, honor cancellation, isolate their
//! output, and release any host reservation after stopping guests. A hardware
//! backend must acquire the shared host reservation *before* changing CPU/MSR
//! state, and restore it on normal return and catchable interruption. Neither a
//! preparer nor the orchestrator is permitted to perform these host mutations.

use crate::{
    config::{Check, Limits, Network, Simulation},
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
    /// Remaining execution-affecting settings must also be explicit.
    pub parameters: BTreeMap<String, String>,
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
    /// Prepared guest filesystem/archive, including deterministic launch helper.
    pub guest: ContentDigest,
    pub launch: Launch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedInputs {
    pub kernel: ContentDigest,
    pub images: BTreeMap<String, PreparedImage>,
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
            .chain(self.images.values().flat_map(|image| {
                [&image.manifest, &image.config, &image.guest]
                    .into_iter()
                    .chain(image.layers.iter())
            }))
    }
}

/// Resolves/builds images and retains their entire input closure in `store`.
/// No guest execution, CPU pinning, or host-state mutation is allowed here.
pub trait PreparationBackend: Send + Sync {
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs>;
}

#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);
impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Only materialized, verified inputs reach the executor. Original Compose paths,
/// registry references, and build contexts are deliberately absent.
pub struct ExecutionRequest<'a> {
    pub seed: u64,
    pub limits: &'a Limits,
    pub network: &'a Network,
    pub check: &'a Check,
    pub prepared: &'a PreparedInputs,
    pub artifacts: &'a BTreeMap<ContentDigest, PathBuf>,
    pub output: &'a Path,
    pub cancellation: &'a CancellationToken,
}

pub trait ExecutionBackend: Send + Sync {
    /// Must exactly match the recorded profile before execution can begin.
    fn profile(&self) -> ExecutionProfile;
    fn execute(&self, request: ExecutionRequest<'_>) -> Result<ExecutionReport>;
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
    pub at_ns: u64,
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
