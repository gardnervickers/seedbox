//! Recorded executable dispatch. Hashes retain bytes; descriptors select their contract.

use crate::{
    execution::{EngineDescriptor, ExecutionProfile},
    Error, Result,
};
use serde::{Deserialize, Serialize};

/// `__engine` contract of the x86 world worker linked into this CLI.
pub const INTERNAL_WORKER_V1: &str = "seedbox-internal-worker-v1";
/// `__engine` contract of the ARM world worker linked into the Mac CLI.
pub const ARM_WORLD_V1: &str = "arm64-world-v1";
/// Whole-world Driver worker, distinct from both retained legacy schedulers.
pub const DRIVER_WORLD_V1: &str = seedbox_engine_protocol::driver_world::CONTRACT;
/// Timebase the ARM world worker reports.
pub const ARM_REPORT_TIMEBASE: &str = "arm64-pv-world-v1";
pub const SIMULATOR_VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub const ARM_LAUNCH_MAX_SKEW_NS: u64 = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutableKind {
    /// A separate engine executable with the plain argv contract. It is the
    /// absence of a descriptor and never itself recorded.
    #[serde(skip)]
    Standalone,
    #[serde(rename = "seedbox-internal-worker-v1")]
    InternalWorker,
    #[serde(rename = "arm64-world-v1")]
    ArmWorld,
    #[serde(rename = "driver-world-v1")]
    DriverWorld,
}

impl ExecutableKind {
    /// The `__engine` argument that selects this worker, if any.
    pub fn contract(self) -> Option<&'static str> {
        match self {
            Self::Standalone => None,
            Self::InternalWorker => Some(INTERNAL_WORKER_V1),
            Self::ArmWorld => Some(ARM_WORLD_V1),
            Self::DriverWorld => Some(DRIVER_WORLD_V1),
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

/// The ARM world worker's descriptor.
pub fn arm_descriptor() -> EngineDescriptor {
    EngineDescriptor {
        kind: ExecutableKind::ArmWorld,
        owner_pipe: true,
        live_observations: true,
        simulator_version: SIMULATOR_VERSION.into(),
        host_os: "macos".into(),
        host_arch: "aarch64".into(),
        guest_arch: "aarch64".into(),
        engine_backend: "hvf".into(),
        engine_timebase: ARM_REPORT_TIMEBASE.into(),
    }
}

/// The x86 world worker's descriptor for `simulator_version`.
pub fn kvm_descriptor(kind: ExecutableKind, simulator_version: String) -> EngineDescriptor {
    EngineDescriptor {
        kind,
        owner_pipe: false,
        live_observations: false,
        simulator_version,
        host_os: "linux".into(),
        host_arch: "x86_64".into(),
        guest_arch: "x86_64".into(),
        engine_backend: "kvm".into(),
        engine_timebase: "pv-v1".into(),
    }
}

/// Compile-time platform metadata, with no process environment lookup.
pub fn internal_descriptor() -> Result<EngineDescriptor> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(Error::Setup(
            "built-in Compose world execution requires Linux x86-64; ARM world integration is not available yet".into(),
        ));
    }
    let mut descriptor = kvm_descriptor(ExecutableKind::InternalWorker, SIMULATOR_VERSION.into());
    descriptor.owner_pipe = true;
    descriptor.live_observations = true;
    Ok(descriptor)
}

/// Decode the executable protocol; other recorded metadata is descriptive.
/// Profiles without a descriptor retain the standalone launch contract.
pub fn recorded_launch(profile: &ExecutionProfile) -> Result<ExecutableKind> {
    Ok(profile.semantic_parameters.launch())
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
            engine: ContentDigest::of(b"engine"),
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

    #[test]
    fn launch_decoding_ignores_descriptive_metadata() {
        let mut recorded = profile();
        assert_eq!(
            recorded_launch(&recorded).unwrap(),
            ExecutableKind::Standalone
        );
        for (kind, contract) in [
            (ExecutableKind::InternalWorker, INTERNAL_WORKER_V1),
            (ExecutableKind::ArmWorld, ARM_WORLD_V1),
        ] {
            recorded.semantic_parameters.engine = Some(EngineDescriptor {
                kind,
                owner_pipe: false,
                live_observations: false,
                simulator_version: "development version".into(),
                host_os: "future-os".into(),
                host_arch: "future-arch".into(),
                guest_arch: "another-arch".into(),
                engine_backend: "future-backend".into(),
                engine_timebase: "future-clock".into(),
            });
            assert_eq!(recorded_launch(&recorded).unwrap(), kind);
            assert_eq!(kind.contract(), Some(contract));
            let json = serde_json::to_value(&recorded).unwrap();
            assert_eq!(json["semantic_parameters"]["engine"]["kind"], contract);
        }
        let mut json = serde_json::to_value(&recorded).unwrap();
        json["semantic_parameters"]["engine"]["kind"] = "future-worker".into();
        assert!(serde_json::from_value::<ExecutionProfile>(json.clone()).is_err());
        json["semantic_parameters"]["engine"]["kind"] = "standalone".into();
        assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
    }
}
