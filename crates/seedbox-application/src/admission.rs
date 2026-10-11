//! Host readiness: the KVM capabilities the engine needs. It does
//! not pin CPU vendors, models, microcode, CPUID, engines or kernels.
use crate::{
    execution::ExecutionProfile, limits::Size, store::ContentDigest, Context, Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{io::Read, path::Path};

/// Check KVM device permissions before a CLI downloads immutable guest inputs.
#[cfg(target_os = "linux")]
pub fn check_kvm_access() -> Result<()> {
    crate::host::check_kvm_access()
}

/// Bound input hashing before any OCI preparation or host state changes.
const RUNTIME_INPUT_LIMIT: u64 = 256 * 1024 * 1024;

pub fn file_digest(path: &Path) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let input = || format!("runtime input {}", path.display());
    let too_large = || {
        Error::Setup(format!(
            "{}: exceeds {}",
            input(),
            Size(RUNTIME_INPUT_LIMIT)
        ))
    };
    let mut file = std::fs::File::open(path).context(Error::Setup, input())?;
    if file.metadata().context(Error::Setup, input())?.len() > RUNTIME_INPUT_LIMIT {
        return Err(too_large());
    }
    crate::store::file_digest_with(&mut file, |file| {
        let mut hash = Sha256::new();
        let mut data = [0; 65536];
        let mut total = 0;
        loop {
            let n = file.read(&mut data).context(Error::Setup, input())?;
            if n == 0 {
                break;
            }
            total += n;
            if total as u64 > RUNTIME_INPUT_LIMIT {
                return Err(too_large());
            }
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

/// The ABI and channel are fixed by the profile's type; memory needs a range.
pub fn validate_profile(profile: &ExecutionProfile) -> Result<()> {
    if !(64..=65536).contains(&profile.semantic_parameters.memory_mb) {
        return Err(Error::Integrity(
            "admitted profile requires memory_mb in 64..65536".into(),
        ));
    }
    Ok(())
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
pub(crate) fn collect_admitted(
    observations: &mut Vec<CoreObservation>,
    observe: impl FnOnce() -> Result<CoreObservation>,
) -> Result<()> {
    let observation = observe()?;
    let admission = validate_observation(&observation);
    observations.push(observation);
    admission
}

// Publication never replaces an established refusal, especially cleanup
// uncertainty. Keep both bounded internal diagnostics; no application bytes are
// parsed or included here.
pub(crate) fn finish_admission(
    admission: Result<()>,
    publish: impl FnOnce(&Result<()>) -> Result<()>,
) -> Result<()> {
    let publication = publish(&admission);
    match (admission, publication) {
        (Ok(()), publication) => publication,
        (Err(refusal), Ok(())) => Err(refusal),
        (Err(refusal), Err(publication)) => {
            fn bounded(error: &Error) -> String {
                let text = error.to_string();
                let mut chars = text.chars();
                let mut prefix: String = chars.by_ref().take(4096).collect();
                if chars.next().is_some() {
                    prefix.push_str(" [truncated]");
                }
                prefix
            }
            Err(Error::Execution(format!(
                "host admission refused: {}; additionally, evidence publication failed: {}",
                bounded(&refusal),
                bounded(&publication)
            )))
        }
    }
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

/// Observe KVM capabilities without modifying inherited CPU affinity.
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
    fn publication_failure_preserves_capability_refusal() {
        let mut observation = static_observation();
        observation.sync_regs = 0;
        let error = finish_admission(validate_observation(&observation), |established| {
            assert!(established.is_err());
            Err(Error::Setup("evidence ENOSPC".into()))
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("KVM sync-register capability"));
        assert!(error.contains("evidence ENOSPC"));
        assert!(finish_admission(Ok(()), |_| Err(Error::Setup("evidence ENOSPC".into()))).is_err());
        let bounded = finish_admission(Err(Error::Execution("r".repeat(10000))), |_| {
            Err(Error::Execution("p".repeat(10000)))
        })
        .unwrap_err()
        .to_string();
        assert!(bounded.len() < 8400);
        assert!(bounded.contains("[truncated]"));
    }
}
