//! Recorded executable dispatch. Hashes retain bytes; descriptors select their contract.

use crate::{execution::ExecutionProfile, Error, Result};
use std::collections::BTreeMap;

pub const INTERNAL_WORKER_V1: &str = "seedbox-internal-worker-v1";
pub const SIMULATOR_VERSION: &str = "0.1.0-dev.1";
const KIND: &str = "engine_executable_kind";
const FIELDS: [&str; 7] = [
    KIND,
    "simulator_version",
    "host_os",
    "host_arch",
    "guest_arch",
    "engine_backend",
    "timebase",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutableKind {
    Standalone,
    InternalWorker,
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
        ("timebase".into(), "counted-branches-v1".into()),
    ]))
}

/// Missing descriptors retain the historical standalone launch contract.
/// This validates recorded metadata without upgrading or filling missing fields.
pub fn recorded_launch(profile: &ExecutionProfile) -> Result<ExecutableKind> {
    let parameters = &profile.semantic_parameters;
    let Some(kind) = parameters.get(KIND) else {
        if FIELDS[1..].iter().any(|key| parameters.contains_key(*key)) {
            return Err(Error::Integrity(
                "partial engine executable descriptor".into(),
            ));
        }
        return Ok(ExecutableKind::Standalone);
    };
    if kind != INTERNAL_WORKER_V1 {
        return Err(Error::Integrity(format!(
            "unknown engine executable kind {kind}"
        )));
    }
    for key in FIELDS {
        if parameters.get(key).is_none_or(|value| value.is_empty()) {
            return Err(Error::Integrity(format!(
                "engine executable descriptor missing {key}"
            )));
        }
    }
    let version = &parameters["simulator_version"];
    if version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-+".contains(&byte))
    {
        return Err(Error::Integrity("invalid simulator_version".into()));
    }
    let platform = (
        parameters["host_os"].as_str(),
        parameters["host_arch"].as_str(),
        parameters["guest_arch"].as_str(),
        parameters["engine_backend"].as_str(),
        parameters["timebase"].as_str(),
    );
    if !matches!(
        platform,
        ("linux", "x86_64", "x86_64", "kvm", "counted-branches-v1")
    ) {
        return Err(Error::Integrity(
            "unsupported engine executable platform/backend/timebase".into(),
        ));
    }
    Ok(ExecutableKind::InternalWorker)
}

/// An executable is replayed on its recorded platform, never auto-substituted.
pub fn validate_host(profile: &ExecutionProfile) -> Result<()> {
    if recorded_launch(profile)? == ExecutableKind::InternalWorker {
        let parameters = &profile.semantic_parameters;
        if parameters["host_os"] != std::env::consts::OS
            || parameters["host_arch"] != std::env::consts::ARCH
        {
            return Err(Error::Integrity(format!(
                "retained engine requires {} {}; this host is {} {}",
                parameters["host_os"],
                parameters["host_arch"],
                std::env::consts::OS,
                std::env::consts::ARCH,
            )));
        }
        // The production world adapter still uses the counted x86 KVM engine.
        if profile.backend == "seedbox-kvm" && parameters["guest_arch"] != "x86_64" {
            return Err(Error::Integrity(
                "seedbox-kvm world execution requires the x86-64 counted engine".into(),
            ));
        }
    }
    Ok(())
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
    fn legacy_is_unchanged_and_partial_descriptors_fail() {
        let mut recorded = profile();
        assert_eq!(
            recorded_launch(&recorded).unwrap(),
            ExecutableKind::Standalone
        );
        assert!(recorded.semantic_parameters.is_empty());
        recorded
            .semantic_parameters
            .insert("host_os".into(), "linux".into());
        assert!(recorded_launch(&recorded).is_err());
    }

    #[test]
    fn complete_descriptor_selects_worker_without_requiring_current_version() {
        let mut recorded = profile();
        recorded.semantic_parameters = BTreeMap::from([
            (KIND.into(), INTERNAL_WORKER_V1.into()),
            ("simulator_version".into(), "0.0.1".into()),
            ("host_os".into(), "linux".into()),
            ("host_arch".into(), "x86_64".into()),
            ("guest_arch".into(), "x86_64".into()),
            ("engine_backend".into(), "kvm".into()),
            ("timebase".into(), "counted-branches-v1".into()),
        ]);
        assert_eq!(
            recorded_launch(&recorded).unwrap(),
            ExecutableKind::InternalWorker
        );
        for field in FIELDS {
            let mut partial = recorded.clone();
            partial.semantic_parameters.remove(field);
            assert!(recorded_launch(&partial).is_err(), "{field}");
        }
        for (field, value) in [
            ("host_os", "windows"),
            ("host_arch", "aarch64"),
            ("guest_arch", "aarch64"),
            ("engine_backend", "hvf"),
            ("timebase", "wall-clock"),
            ("simulator_version", "version with spaces"),
        ] {
            let mut incompatible = recorded.clone();
            incompatible
                .semantic_parameters
                .insert(field.into(), value.into());
            assert!(recorded_launch(&incompatible).is_err(), "{field}");
        }
        recorded
            .semantic_parameters
            .insert(KIND.into(), "future".into());
        assert!(recorded_launch(&recorded).is_err());
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    #[test]
    fn retained_foreign_platform_refuses_before_execution() {
        let mut recorded = profile();
        recorded.semantic_parameters = BTreeMap::from([
            (KIND.into(), INTERNAL_WORKER_V1.into()),
            ("simulator_version".into(), "0.0.1".into()),
        ]);
        for (field, value) in [
            ("host_os", "linux"),
            ("host_arch", "x86_64"),
            ("guest_arch", "x86_64"),
            ("engine_backend", "kvm"),
            ("timebase", "counted-branches-v1"),
        ] {
            recorded
                .semantic_parameters
                .insert(field.into(), value.into());
        }
        assert_eq!(
            recorded_launch(&recorded).unwrap(),
            ExecutableKind::InternalWorker
        );
        assert!(validate_host(&recorded)
            .unwrap_err()
            .to_string()
            .contains("retained engine requires"));
    }

    #[test]
    fn arm_fixtures_cannot_claim_the_compose_world_contract() {
        let mut recorded = profile();
        for (os, backend) in [("macos", "hvf"), ("linux", "kvm")] {
            recorded.semantic_parameters = BTreeMap::from([
                (KIND.into(), INTERNAL_WORKER_V1.into()),
                ("simulator_version".into(), "0.0.1".into()),
                ("host_os".into(), os.into()),
                ("host_arch".into(), "aarch64".into()),
                ("guest_arch".into(), "aarch64".into()),
                ("engine_backend".into(), backend.into()),
                ("timebase".into(), "arm64-pv-v2".into()),
            ]);
            assert!(recorded_launch(&recorded)
                .unwrap_err()
                .to_string()
                .contains("unsupported engine executable platform/backend/timebase"));
        }
    }
}
