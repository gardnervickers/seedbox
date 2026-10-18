//! Host readiness: the KVM capabilities the engine needs. It does
//! not pin CPU vendors, models, microcode, CPUID, engines or kernels.
use crate::{limits::Size, store::ContentDigest, Context, Error, Result};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

/// Check KVM device permissions before a CLI downloads immutable guest inputs.
#[cfg(target_os = "linux")]
pub fn check_kvm_access() -> Result<()> {
    crate::host::check_kvm_access()
}

/// Bound guest/runtime asset hashing before OCI preparation or host state changes.
const RUNTIME_INPUT_LIMIT: u64 = 256 * 1024 * 1024;

pub fn file_digest(path: &Path) -> Result<ContentDigest> {
    digest(path, Some(RUNTIME_INPUT_LIMIT))
}

/// The retained engine is the complete CLI, including embedded assets and debug
/// information. Execution streams it into the content store without the guest
/// asset size cap; readiness must accept the same bytes, with bounded memory.
pub(crate) fn engine_digest(path: &Path) -> Result<ContentDigest> {
    digest(path, None)
}

fn digest(path: &Path, limit: Option<u64>) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let input = || format!("runtime input {}", path.display());
    let check_size = |size| {
        if let Some(limit) = limit.filter(|limit| size > *limit) {
            return Err(Error::Setup(format!(
                "{}: exceeds {}",
                input(),
                Size(limit)
            )));
        }
        Ok(())
    };
    let mut file = std::fs::File::open(path).context(Error::Setup, input())?;
    check_size(file.metadata().context(Error::Setup, input())?.len())?;
    crate::store::file_digest_with(&mut file, |file| {
        let mut hash = Sha256::new();
        let mut data = [0; 65536];
        let mut total = 0;
        loop {
            let n = file.read(&mut data).context(Error::Setup, input())?;
            if n == 0 {
                break;
            }
            total += n as u64;
            check_size(total)?;
            hash.update(&data[..n]);
        }
        Ok(ContentDigest::finish(hash))
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(C)]
pub struct CpuidEntry {
    pub function: u32,
    pub index: u32,
    pub flags: u32,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    #[serde(default)]
    pub padding: [u32; 3],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredPolicy {
    pub architecture: String,
    pub guest_abi: String,
    pub control_channel: String,
    pub kvm_api: i32,
    pub immediate_exit: i32,
    pub guest_debug: i32,
    pub sync_regs_required: i32,
}

pub fn required_policy() -> RequiredPolicy {
    RequiredPolicy {
        architecture: "x86_64".into(),
        guest_abi: "oci-init-v1".into(),
        control_channel: "virtio-console-v1".into(),
        kvm_api: 12,
        immediate_exit: 1,
        guest_debug: 1,
        sync_regs_required: 3,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreObservation {
    pub cpu: u32,
    pub architecture: String,
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub microcode: u64,
    pub kvm_api: i32,
    pub immediate_exit: i32,
    pub guest_debug: i32,
    pub sync_regs: i32,
    pub raw_cpuid: Vec<CpuidEntry>,
}

pub fn validate_observation(o: &CoreObservation) -> Result<()> {
    let p = required_policy();
    let checks = [
        (o.architecture == p.architecture, "architecture"),
        (o.kvm_api == p.kvm_api, "KVM API"),
        (
            o.immediate_exit == p.immediate_exit,
            "KVM immediate-exit capability",
        ),
        (o.guest_debug == p.guest_debug, "KVM guest-debug capability"),
        (
            o.sync_regs & p.sync_regs_required == p.sync_regs_required,
            "KVM sync-register capability",
        ),
    ];
    for (valid, field) in checks {
        if !valid {
            return Err(Error::Setup(format!("CPU {}: unsupported {field}", o.cpu)));
        }
    }
    Ok(())
}

// Retain the bounded host observation before reporting a capability refusal.
#[cfg(any(test, not(all(target_os = "macos", target_arch = "aarch64"))))]
pub(crate) fn collect_admitted(
    observations: &mut Vec<CoreObservation>,
    observe: impl FnOnce() -> Result<CoreObservation>,
) -> Result<()> {
    let observation = observe()?;
    let admission = validate_observation(&observation);
    observations.push(observation);
    admission
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

/// Observe KVM capabilities without modifying inherited CPU affinity.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
pub(crate) fn observe() -> Result<CoreObservation> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::observe()
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        Err(Error::Setup(
            "host admission requires Linux x86_64 KVM; use an admitted host".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn complete_engine_can_exceed_guest_asset_limit_without_truncation() {
        use sha2::{Digest, Sha256};
        use std::io::{Seek, SeekFrom, Write};

        // A normal release CLI includes embedded assets and debug information.
        // Keep this fixture sparse, but hash its entire tail beyond the asset cap.
        let mut engine = tempfile::NamedTempFile::new().unwrap();
        engine.as_file_mut().set_len(RUNTIME_INPUT_LIMIT).unwrap();
        engine.seek(SeekFrom::End(0)).unwrap();
        engine.write_all(b"x").unwrap();
        let mut expected = Sha256::new();
        let zeros = [0; 65536];
        for _ in 0..RUNTIME_INPUT_LIMIT / zeros.len() as u64 {
            expected.update(zeros);
        }
        expected.update(b"x");
        assert_eq!(
            engine_digest(engine.path()).unwrap(),
            ContentDigest::finish(expected)
        );
        // A cached engine hash must not bypass the unchanged guest-asset bound.
        assert!(file_digest(engine.path())
            .unwrap_err()
            .to_string()
            .contains("exceeds 256 MiB"));
    }

    fn static_observation() -> CoreObservation {
        let p = required_policy();
        CoreObservation {
            cpu: 8,
            architecture: p.architecture,
            vendor: "AuthenticAMD".into(),
            family: 25,
            model: 33,
            stepping: 0,
            microcode: 0,
            kvm_api: p.kvm_api,
            immediate_exit: p.immediate_exit,
            guest_debug: p.guest_debug,
            sync_regs: p.sync_regs_required,
            raw_cpuid: Vec::new(),
        }
    }

    #[test]
    fn host_observation_is_retained_before_capability_refusal() {
        let mut observations = Vec::new();
        let result = collect_admitted(&mut observations, || {
            let mut observation = static_observation();
            observation.immediate_exit = 0;
            Ok(observation)
        });
        assert_eq!(observations.len(), 1);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("KVM immediate-exit capability"));
        let mut observations = Vec::new();
        assert!(collect_admitted(&mut observations, || Err(Error::Setup(
            "cannot open KVM".into()
        )))
        .is_err());
        assert!(observations.is_empty());
    }

    #[test]
    fn preflight_refusal_is_before_application_preparation_or_execution() {
        use crate::{
            config,
            execution::*,
            orchestration::{Orchestrator, RunOptions},
            store::ContentStore,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Refused;
        impl ExecutionBackend for Refused {
            fn profile(&self) -> ExecutionProfile {
                ExecutionProfile {
                    backend: "fixture".into(),
                    witness_mode: WitnessMode::Exits,
                    semantic_parameters: SemanticParameters::new(),
                }
            }
            fn preflight(&self, _: Option<&Path>) -> Result<()> {
                Err(Error::Setup("cannot open /dev/kvm: EACCES (13)".into()))
            }
            fn execute(&self, _: ExecutionRequest<'_>) -> Result<ExecutionReport> {
                panic!("must not execute after host refusal")
            }
        }
        struct Prepare(AtomicUsize);
        impl PreparationBackend for Prepare {
            fn prepare(&self, _: &config::Simulation, _: &ContentStore) -> Result<PreparedInputs> {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("must not prepare after host refusal")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let prepare = Prepare(AtomicUsize::new(0));
        let simulation=config::parse("name: host-refusal\nservices:\n  app: {image: fixture:synthetic}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",&root.path().join("compose.yaml"),&BTreeMap::new()).unwrap();
        let orchestrator = Orchestrator::new(
            ContentStore::new(root.path().join("objects")),
            root.path().join("runs"),
        );
        let error = orchestrator
            .run(&simulation, RunOptions::default(), &prepare, &Refused)
            .unwrap_err()
            .to_string();
        assert!(error.contains("EACCES (13)"));
        assert_eq!(prepare.0.load(Ordering::SeqCst), 0);
    }
}
