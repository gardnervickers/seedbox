//! Recorded executable dispatch. Hashes retain bytes; descriptors select their contract.

use crate::{execution::ExecutionProfile, Error, Result};
use std::collections::BTreeMap;

pub const INTERNAL_WORKER_V1: &str = "seedbox-internal-worker-v1";
pub const SIMULATOR_VERSION: &str = env!("CARGO_PKG_VERSION");
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
pub(crate) const ARM_LAUNCH_MAX_SKEW_NS: u64 = 1_000_000;
const KIND: &str = "engine_executable_kind";
pub(crate) const DESCRIPTOR_FIELDS: [&str; 7] = [
    KIND,
    "simulator_version",
    "host_os",
    "host_arch",
    "guest_arch",
    "engine_backend",
    "engine_timebase",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutableKind {
    Standalone,
    InternalWorker,
    ArmWorld,
}

/// Retain the numeric launch cap and clip it to network lookahead for causality.
/// Profiles without a cap preserve their original immediate launch behavior.
pub(crate) fn arm_launch_skew(profile: &ExecutionProfile, lookahead: u64) -> Result<Option<u64>> {
    profile
        .semantic_parameters
        .get("arm_launch_max_skew_ns")
        .map(|value| {
            value
                .parse::<u64>()
                .map(|cap| cap.min(lookahead))
                .map_err(|_| {
                    Error::Integrity(
                        "retained ARM arm_launch_max_skew_ns must be an unsigned integer".into(),
                    )
                })
        })
        .transpose()
}

pub fn arm_parameters() -> BTreeMap<String, String> {
    BTreeMap::from([
        (KIND.into(), "arm64-world-v1".into()),
        ("simulator_version".into(), SIMULATOR_VERSION.into()),
        ("host_os".into(), "macos".into()),
        ("host_arch".into(), "aarch64".into()),
        ("guest_arch".into(), "aarch64".into()),
        ("engine_backend".into(), "hvf".into()),
        ("engine_timebase".into(), "arm64-pv-world-v1".into()),
        ("timebase".into(), "pv".into()),
    ])
}

/// Compile-time platform metadata, with no process environment lookup.
pub fn internal_parameters() -> Result<BTreeMap<String, String>> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err(Error::Setup(
            "built-in Compose world execution requires Linux x86-64; ARM world integration is not available yet".into(),
        ));
    }
    Ok(BTreeMap::from([
        (KIND.into(), INTERNAL_WORKER_V1.into()),
        ("simulator_version".into(), SIMULATOR_VERSION.into()),
        ("host_os".into(), std::env::consts::OS.into()),
        ("host_arch".into(), std::env::consts::ARCH.into()),
        ("guest_arch".into(), std::env::consts::ARCH.into()),
        ("engine_backend".into(), "kvm".into()),
        ("engine_timebase".into(), "pv-v1".into()),
    ]))
}

/// Decode the executable protocol; other recorded metadata is descriptive.
/// Profiles without a kind retain the standalone launch contract.
pub fn recorded_launch(profile: &ExecutionProfile) -> Result<ExecutableKind> {
    match profile.semantic_parameters.get(KIND).map(String::as_str) {
        None => Ok(ExecutableKind::Standalone),
        Some(INTERNAL_WORKER_V1) => Ok(ExecutableKind::InternalWorker),
        Some("arm64-world-v1") => Ok(ExecutableKind::ArmWorld),
        Some(kind) => Err(Error::Integrity(format!(
            "unknown engine executable kind {kind}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{execution::WitnessMode, store::ContentDigest};

    fn profile() -> ExecutionProfile {
        ExecutionProfile {
            backend: "seedbox-kvm".into(),
            engine: ContentDigest::of(b"engine"),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: BTreeMap::new(),
        }
    }

    #[test]
    fn launch_cap_is_numeric_and_cannot_outrun_network_lookahead() {
        let mut recorded = profile();
        assert_eq!(arm_launch_skew(&recorded, 800_000).unwrap(), None);
        recorded
            .semantic_parameters
            .insert("arm_launch_policy".into(), "old-or-future-marker".into());
        for (cap, expected) in [
            (0, 0),
            (400_000, 400_000),
            (1_000_000, 800_000),
            (2_000_000, 800_000),
            (u64::MAX, 800_000),
        ] {
            recorded
                .semantic_parameters
                .insert("arm_launch_max_skew_ns".into(), cap.to_string());
            assert_eq!(arm_launch_skew(&recorded, 800_000).unwrap(), Some(expected));
            assert_eq!(arm_launch_skew(&recorded, 0).unwrap(), Some(0));
        }
        for value in ["-1", "not-a-number", "18446744073709551616"] {
            recorded
                .semantic_parameters
                .insert("arm_launch_max_skew_ns".into(), value.into());
            assert!(arm_launch_skew(&recorded, 800_000).is_err());
        }
    }

    #[test]
    fn launch_decoding_ignores_descriptive_metadata() {
        let mut recorded = profile();
        for (key, value) in [
            ("host_os", "future-os"),
            ("host_arch", "future-arch"),
            ("guest_arch", "another-arch"),
            ("simulator_version", "development version"),
            ("engine_timebase", "future-clock"),
        ] {
            recorded
                .semantic_parameters
                .insert(key.into(), value.into());
        }
        assert_eq!(
            recorded_launch(&recorded).unwrap(),
            ExecutableKind::Standalone
        );
        for (kind, expected) in [
            (INTERNAL_WORKER_V1, ExecutableKind::InternalWorker),
            ("arm64-world-v1", ExecutableKind::ArmWorld),
        ] {
            recorded
                .semantic_parameters
                .insert(KIND.into(), kind.into());
            assert_eq!(recorded_launch(&recorded).unwrap(), expected);
        }
        recorded
            .semantic_parameters
            .insert(KIND.into(), "future-worker".into());
        assert!(recorded_launch(&recorded)
            .unwrap_err()
            .to_string()
            .contains("unknown engine executable kind"));
    }
}
