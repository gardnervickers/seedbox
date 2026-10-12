//! Engine launch contracts and build provenance. The engine is always the
//! running Seedbox executable; runs record which build executed them.

use crate::execution::{EngineDescriptor, ExecutionProfile};

/// `__engine` contract of the x86 world worker linked into this CLI.
pub const INTERNAL_WORKER_V1: &str = "seedbox-internal-worker-v1";
/// `__engine` contract of the ARM world worker linked into the Mac CLI.
pub const ARM_WORLD_V1: &str = "arm64-world-v1";
/// Timebase the ARM world worker reports.
pub const ARM_REPORT_TIMEBASE: &str = "arm64-pv-world-v1";
pub const SIMULATOR_VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const ARM_LAUNCH_MAX_SKEW_NS: u64 = 1_000_000;

/// Which worker entry point of the Seedbox executable runs a world.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutableKind {
    InternalWorker,
    ArmWorld,
}

impl ExecutableKind {
    /// The `__engine` argument that selects this worker.
    pub fn contract(self) -> &'static str {
        match self {
            Self::InternalWorker => INTERNAL_WORKER_V1,
            Self::ArmWorld => ARM_WORLD_V1,
        }
    }
}

/// Retain the numeric launch cap and clip it to network lookahead for causality.
/// Profiles without a cap preserve their original immediate launch behavior.
pub fn arm_launch_skew(profile: &ExecutionProfile, lookahead: u64) -> Option<u64> {
    profile
        .semantic_parameters
        .hvf
        .as_ref()
        .map(|hvf| hvf.launch_max_skew_ns.min(lookahead))
}

/// Provenance of the ARM world worker built from `revision`.
pub fn arm_descriptor(revision: Option<String>) -> EngineDescriptor {
    EngineDescriptor {
        simulator_version: SIMULATOR_VERSION.into(),
        revision,
        host_os: "macos".into(),
        host_arch: "aarch64".into(),
        guest_arch: "aarch64".into(),
        engine_backend: "hvf".into(),
        engine_timebase: ARM_REPORT_TIMEBASE.into(),
    }
}

/// Provenance of the x86 world worker built from `revision`.
pub fn kvm_descriptor(revision: Option<String>) -> EngineDescriptor {
    EngineDescriptor {
        simulator_version: SIMULATOR_VERSION.into(),
        revision,
        host_os: "linux".into(),
        host_arch: "x86_64".into(),
        guest_arch: "x86_64".into(),
        engine_backend: "kvm".into(),
        engine_timebase: "pv-v1".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        execution::{Backend, HvfParameters, SemanticParameters, WitnessMode},
        store::ContentDigest,
    };

    fn profile() -> ExecutionProfile {
        ExecutionProfile {
            backend: Backend::Kvm,
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(512),
        }
    }

    #[test]
    fn launch_cap_is_numeric_and_cannot_outrun_network_lookahead() {
        let mut recorded = profile();
        assert_eq!(arm_launch_skew(&recorded, 800_000), None);
        for (cap, expected) in [
            (0, 0),
            (400_000, 400_000),
            (1_000_000, 800_000),
            (2_000_000, 800_000),
            (u64::MAX, 800_000),
        ] {
            recorded.semantic_parameters.hvf = Some(HvfParameters {
                boot_policy: String::new(),
                entropy_policy: String::new(),
                device_policy: String::new(),
                host_release: String::new(),
                cpu_features: String::new(),
                guest_kernel: ContentDigest::of(b"kernel"),
                launch_max_skew_ns: cap,
            });
            assert_eq!(arm_launch_skew(&recorded, 800_000), Some(expected));
            assert_eq!(arm_launch_skew(&recorded, 0), Some(0));
        }
        let mut json = serde_json::to_value(&recorded).unwrap();
        for value in [
            serde_json::json!(-1),
            serde_json::json!("not-a-number"),
            serde_json::json!(1.8446744073709552e19),
        ] {
            json["semantic_parameters"]["hvf"]["launch_max_skew_ns"] = value;
            assert!(serde_json::from_value::<ExecutionProfile>(json.clone()).is_err());
        }
    }
}
