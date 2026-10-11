//! Retained execution inputs and outcomes; no machine execution dependencies.
use crate::{engine::ExecutableKind, image::Launch, store::ContentDigest};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
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
    /// Every option off.
    pub fn new(memory_mb: u32) -> Self {
        Self {
            guest_abi: GuestAbi::OciInitV1,
            control_channel: ControlChannel::VirtioConsoleV1,
            memory_mb,
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

/// A runtime checkpoint: the world forks at `at_ns` with a new branch seed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub at_ns: u64,
    pub branch_seed: u64,
}

/// The retained executable's launch contract plus descriptive build metadata.
/// `kind` and `owner_pipe` describe the launch protocol; build metadata is informational.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineDescriptor {
    pub kind: ExecutableKind,
    /// The executable accepts `--owner-pipe` after its private contract. EOF on
    /// stdin then terminates its process group if the owning server disappears.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_pipe: bool,
    /// Supports the private nonblocking observation socket launch argument.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live_observations: bool,
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
    /// Per-machine RAM overrides in MiB; older inputs use the profile default.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub machine_memory_mb: BTreeMap<String, u32>,
    pub profile: ExecutionProfile,
    /// Config files, source/build snapshots, and other replay dependencies.
    pub auxiliary: BTreeMap<String, ContentDigest>,
    /// Build/source identity (e.g. context hash, Dockerfile hash, builder version).
    pub provenance: BTreeMap<String, String>,
}

impl PreparedInputs {
    pub fn memory_mb(&self, machine: &str) -> u32 {
        self.machine_memory_mb
            .get(machine)
            .copied()
            .unwrap_or(self.profile.semantic_parameters.memory_mb)
    }

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
    pub fn owned(&self) -> Self {
        Self(Arc::new(CancellationState {
            cancelled: AtomicBool::new(false),
            parent: Some(self.clone()),
        }))
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
    /// Intentional host pause/pacing time, excluded from the execution budget.
    /// Runtime-only accounting; never part of serialized result comparisons.
    #[serde(skip)]
    pub host_wait_ns: u64,
    /// Final scheduler position (furthest machine clock for legacy reports).
    /// Absent when execution produced no report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub simulated_time_ns: Option<u64>,
    pub reason: StopReason,
    pub workload_exit: Option<WorkloadExit>,
    pub witness: Option<ContentDigest>,
    /// None means no full-state comparison is available. Never imply that an
    /// exit witness proves equality of guest RAM/registers.
    pub full_state: Option<BTreeMap<String, ContentDigest>>,
    pub faults: Vec<FaultEvent>,
    pub detail: Option<String>,
    /// Machines the scenario spawned at runtime, in spawn order. They run
    /// beside the prepared machines and have their own logs and state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spawned: Vec<String>,
}

impl ExecutionReport {
    /// Every machine of the run: prepared, then spawned.
    pub fn machines<'a>(
        &'a self,
        prepared: impl Iterator<Item = &'a String>,
    ) -> std::collections::BTreeSet<&'a str> {
        prepared.chain(&self.spawned).map(String::as_str).collect()
    }
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
