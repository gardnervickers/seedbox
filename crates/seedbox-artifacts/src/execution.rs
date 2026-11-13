//! Retained execution inputs and outcomes; no machine execution dependencies.
use crate::{engine::ExecutableKind, image::Launch, store::ContentDigest};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionProfile {
    /// Recorded CPU backend (`driver-pv` or `driver-tcg`); test doubles may name themselves.
    pub backend: String,
    pub witness_mode: WitnessMode,
    /// Settings that change results, hashed into the run identity.
    pub semantic_parameters: SemanticParameters,
}

impl ExecutionProfile {
    pub fn driver_backend(&self) -> Result<Option<seedbox_spec::backend::CpuBackend>, String> {
        seedbox_spec::backend::CpuBackend::from_recorded_name(&self.backend)
    }
}

/// Settings recorded in a run's identity because they change its results.
/// Every reader decodes them strictly; no setting is implied by omission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticParameters {
    /// Seeded survival of the latest unflushed 512-byte sectors on crash.
    pub torn_writes: bool,
    /// Driver executable descriptor. Non-executing test backends may omit it.
    pub engine: Option<EngineDescriptor>,
}

impl Default for SemanticParameters {
    fn default() -> Self {
        Self::new()
    }
}

impl SemanticParameters {
    /// Every option off.
    pub fn new() -> Self {
        Self {
            torn_writes: false,
            engine: None,
        }
    }

    /// The executable contract that launches the retained engine.
    pub fn launch(&self) -> Option<ExecutableKind> {
        self.engine.as_ref().map(|engine| engine.kind)
    }

    /// Names of the settings that differ, ignoring the executable descriptor,
    /// which a replacement engine may change.
    pub fn differences(&self, other: &Self) -> Vec<&'static str> {
        let Self {
            torn_writes,
            engine: _,
        } = self;
        [(torn_writes != &other.torn_writes, "torn_writes")]
            .into_iter()
            .filter_map(|(differs, name)| differs.then_some(name))
            .collect()
    }
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
    pub engine_timebase: String,
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
    pub wall_clock_epoch: Option<u32>,
    pub images: BTreeMap<String, PreparedImage>,
    /// Every service maps to one machine; services may share a machine.
    pub placement: BTreeMap<String, String>,
    /// Machine name to prepared boot filesystem/archive and launch helpers.
    pub guests: BTreeMap<String, ContentDigest>,
    /// Complete machine RAM inventory in MiB, with no profile-wide fallback.
    pub machine_memory_mb: BTreeMap<String, u32>,
    pub profile: ExecutionProfile,
    /// Config files, source/build snapshots, and other replay dependencies.
    pub auxiliary: BTreeMap<String, ContentDigest>,
    /// Build/source identity (e.g. context hash, Dockerfile hash, builder version).
    pub provenance: BTreeMap<String, String>,
}

impl PreparedInputs {
    pub fn memory_mb(&self, machine: &str) -> crate::Result<u32> {
        self.machine_memory_mb.get(machine).copied().ok_or_else(|| {
            crate::Error::Integrity(format!("prepared memory missing for machine {machine}"))
        })
    }

    /// Driver process executable is retained with its manifest closure, never in the profile.
    pub fn executable(&self) -> crate::Result<&ContentDigest> {
        self.auxiliary
            .get("driver-world/executable")
            .ok_or_else(|| {
                crate::Error::Integrity("Driver executable missing from retained closure".into())
            })
    }

    pub fn digests(&self) -> impl Iterator<Item = &ContentDigest> {
        self.auxiliary
            .values()
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

/// The action of an applied spawn; its `service` names the machine it added.
pub const SPAWN_ACTION: &str = "spawn";

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
    /// Final canonical simulation time.
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
    /// The Driver's machines when the run stopped, the oracle first. Unlike
    /// spawn records, which cover only this attempt, it names machines
    /// spawned before the checkpoint a run resumed from. Empty when the
    /// backend names no machines beyond the prepared guests.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub machines: Vec<String>,
    pub detail: Option<String>,
    /// Guest property results reduced at commit; absent when no guest
    /// reported property evidence. Part of result comparisons.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub properties: Option<seedbox_engine_protocol::property::Assessment>,
}

impl ExecutionReport {
    /// Every machine the run's evidence may name: the initial `guests`, which
    /// prepared inputs name, and the machines present when the run stopped,
    /// which include spawned ones.
    pub fn machines<'a>(
        &'a self,
        guests: &'a BTreeMap<String, ContentDigest>,
    ) -> BTreeSet<&'a str> {
        guests
            .keys()
            .chain(&self.machines)
            .map(String::as_str)
            .collect()
    }

    /// Whether full-state digests name exactly [`Self::machines`]; false
    /// when there are none.
    pub fn full_state_covers(&self, guests: &BTreeMap<String, ContentDigest>) -> bool {
        self.full_state
            .as_ref()
            .is_some_and(|state| state.keys().map(String::as_str).eq(self.machines(guests)))
    }
}

#[cfg(test)]
mod report_tests {
    use super::*;

    fn fault(service: &str, action: &str) -> FaultEvent {
        FaultEvent {
            requested_at_ns: 1,
            applied_at_ns: 1,
            service: service.into(),
            action: action.into(),
            detail: String::new(),
        }
    }

    fn digests(names: &[&str]) -> BTreeMap<String, ContentDigest> {
        names
            .iter()
            .map(|name| (name.to_string(), ContentDigest::of(name.as_bytes())))
            .collect()
    }

    #[test]
    fn full_state_covers_initial_and_present_machines() {
        let guests = digests(&["db", "oracle"]);
        let mut report = ExecutionReport {
            host_wait_ns: 0,
            simulated_time_ns: None,
            reason: StopReason::Completed,
            workload_exit: None,
            witness: None,
            full_state: None,
            // Membership comes from the machines present at stop, not from
            // this attempt's spawn records: a resumed run has none.
            faults: vec![fault("db", "crash"), fault("other", SPAWN_ACTION)],
            machines: vec!["oracle".into(), "db".into(), "replica".into()],
            detail: None,
            properties: None,
        };
        assert!(report
            .machines(&guests)
            .into_iter()
            .eq(["db", "oracle", "replica"]));
        assert!(!report.full_state_covers(&guests));
        report.full_state = Some(digests(&["db", "oracle", "replica"]));
        assert!(report.full_state_covers(&guests));
        for names in [
            &["db", "oracle"][..],
            &["db", "oracle", "replica", "other"],
            &["oracle", "replica"],
        ] {
            report.full_state = Some(digests(names));
            assert!(!report.full_state_covers(&guests), "{names:?}");
        }
        // Without named machines, full state covers exactly the guests.
        report.machines.clear();
        report.full_state = Some(digests(&["db", "oracle"]));
        assert!(report.full_state_covers(&guests));
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
