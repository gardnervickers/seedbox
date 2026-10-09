//! Contracts between orchestration, image preparation, and execution.
//!
//! Backends must enforce simulated/wall limits, honor cancellation, isolate their
//! output, and release any host reservation after stopping guests. A hardware
//! backend must acquire the shared host reservation *before* changing CPU/MSR
//! state, and restore it on normal return and catchable interruption. Neither a
//! preparer nor the orchestrator is permitted to perform these host mutations.

use crate::{
    config::{Check, Limits, Network, Schedule, Simulation},
    counter::CounterProfile,
    engine::ExecutableKind,
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
    pub backend: Backend,
    /// Actual runner/VMM binary bytes, not just a version string.
    pub engine: ContentDigest,
    pub witness_mode: WitnessMode,
    /// Settings that change results, hashed into the run identity.
    /// Schedule controls live in Simulation.
    pub semantic_parameters: SemanticParameters,
}

impl ExecutionProfile {
    /// How guest progress advances virtual time in this run.
    pub fn timebase(&self) -> Timebase {
        self.semantic_parameters.clock.timebase()
    }
}

/// The execution backend that records and replays a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Backend {
    /// Linux KVM engine (`seedbox-vmm`).
    #[serde(rename = "seedbox-kvm")]
    Kvm,
    /// Apple Hypervisor.framework ARM world (`seedbox-arm64`).
    #[serde(rename = "seedbox-hvf")]
    Hvf,
    /// Another `ExecutionBackend` implementation, such as a test double.
    #[serde(untagged)]
    Other(String),
}

/// Settings recorded in a run's identity because they change its results.
/// Every reader decodes them strictly; no setting is implied by omission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticParameters {
    pub guest_abi: GuestAbi,
    pub control_channel: ControlChannel,
    /// Guest RAM of every machine.
    pub memory_mb: u32,
    pub clock: Clock,
    /// Guest timer deadlines are read from the shared time page.
    pub page_timer: bool,
    /// Virtio-net TX kicks are deferred to deterministic polls.
    pub deferred_tx: bool,
    /// Seeded survival of the latest unflushed 512-byte sectors on crash.
    pub torn_writes: bool,
    /// Runtime checkpoint and the branch seed after it.
    pub checkpoint: Option<Checkpoint>,
    /// Launch contract of the retained executable. `None` is a standalone
    /// engine that takes the plain argv contract.
    pub engine: Option<EngineDescriptor>,
    /// Inputs of the HVF backend only.
    pub hvf: Option<HvfParameters>,
}

impl SemanticParameters {
    /// Counter clock with the default AMD Zen profile and every option off.
    pub fn new(memory_mb: u32) -> Self {
        Self {
            guest_abi: GuestAbi::OciInitV1,
            control_channel: ControlChannel::VirtioConsoleV1,
            memory_mb,
            clock: Clock::Counter(CounterProfile::Zen),
            page_timer: false,
            deferred_tx: false,
            torn_writes: false,
            checkpoint: None,
            engine: None,
            hvf: None,
        }
    }

    /// The executable contract that launches the retained engine.
    pub fn launch(&self) -> ExecutableKind {
        self.engine
            .as_ref()
            .map_or(ExecutableKind::Standalone, |engine| engine.kind)
    }

    /// Names of the settings that differ, ignoring the executable descriptor,
    /// which a replacement engine may change.
    pub fn differences(&self, other: &Self) -> Vec<&'static str> {
        let Self {
            guest_abi,
            control_channel,
            memory_mb,
            clock,
            page_timer,
            deferred_tx,
            torn_writes,
            checkpoint,
            engine: _,
            hvf,
        } = self;
        [
            (guest_abi != &other.guest_abi, "guest_abi"),
            (control_channel != &other.control_channel, "control_channel"),
            (memory_mb != &other.memory_mb, "memory_mb"),
            (clock != &other.clock, "clock"),
            (page_timer != &other.page_timer, "page_timer"),
            (deferred_tx != &other.deferred_tx, "deferred_tx"),
            (torn_writes != &other.torn_writes, "torn_writes"),
            (checkpoint != &other.checkpoint, "checkpoint"),
            (hvf != &other.hvf, "hvf"),
        ]
        .into_iter()
        .filter_map(|(differs, name)| differs.then_some(name))
        .collect()
    }
}

/// How the guest launches its application.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GuestAbi {
    /// One OCI application per machine, started by the Seedbox init.
    #[serde(rename = "oci-init-v1")]
    OciInitV1,
}

/// How the control service talks to the world.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ControlChannel {
    /// A virtio console port to the management endpoint.
    #[serde(rename = "virtio-console-v1")]
    VirtioConsoleV1,
}

impl ControlChannel {
    /// The name the engine's world configuration uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::VirtioConsoleV1 => "virtio-console-v1",
        }
    }
}

/// The timebase together with the branch counter a counter run reads.
/// PV reads no counter, so its identity carries none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Clock {
    Counter(CounterProfile),
    Pv,
}

impl Clock {
    pub fn new(timebase: Timebase, counter: CounterProfile) -> Self {
        match timebase {
            Timebase::Counter => Self::Counter(counter),
            Timebase::Pv => Self::Pv,
        }
    }

    pub fn timebase(self) -> Timebase {
        match self {
            Self::Counter(_) => Timebase::Counter,
            Self::Pv => Timebase::Pv,
        }
    }

    /// The branch counter read, or `None` for PV.
    pub fn counter(self) -> Option<CounterProfile> {
        match self {
            Self::Counter(profile) => Some(profile),
            Self::Pv => None,
        }
    }
}

/// A runtime checkpoint: the world forks at `at_ns` with a new branch seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub at_ns: u64,
    pub branch_seed: u64,
}

/// The retained executable's launch contract plus descriptive build metadata.
/// Only `kind` selects behaviour; the other fields are informational.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineDescriptor {
    pub kind: ExecutableKind,
    pub simulator_version: String,
    pub host_os: String,
    pub host_arch: String,
    pub guest_arch: String,
    pub engine_backend: String,
    pub engine_timebase: String,
}

/// HVF world inputs: boot, entropy and device policies, the host it was
/// admitted on, the guest kernel and the launch skew cap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HvfParameters {
    pub boot_policy: String,
    pub entropy_policy: String,
    pub device_policy: String,
    pub host_release: String,
    /// Digest of the sampled CPU feature registers.
    pub cpu_features: String,
    pub guest_kernel: ContentDigest,
    /// Largest seeded offset of each machine's initial launch, before it is
    /// clipped to the network lookahead.
    pub launch_max_skew_ns: u64,
}

pub use seedbox_engine_core::launch::Timebase;

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
    /// Execution timebase; absent in older records or other backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timebase: Option<Timebase>,
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
