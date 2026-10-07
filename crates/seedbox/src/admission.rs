//! Versioned host readiness for the unchanged engine168ad CPUID/PMU policy.
//! Matching another machine admits compatibility; it is not replay qualification.
use crate::{execution::ExecutionProfile, store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io::Read, path::Path};

pub const POLICY_VERSION: &str = "amd-zen3-engine168ad-v1";
pub const ENGINE: &str = "168ad5db0e63919a3c04dbfed6c22f133aa14600747db4f64ebc3f3dc2c37c7d";
pub const GUEST_KERNEL: &str = "e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847";
const HOST_KERNEL: &str = "66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc";

/// Bound input hashing before any OCI preparation or host state changes.
pub(crate) fn file_digest(path: &Path) -> Result<ContentDigest> {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path)
        .map_err(|e| Error::Setup(format!("runtime input {}: {e}", path.display())))?;
    let mut hash = Sha256::new();
    let mut data = [0; 65536];
    let mut total = 0;
    loop {
        let n = file
            .read(&mut data)
            .map_err(|e| Error::Setup(format!("runtime input {}: {e}", path.display())))?;
        if n == 0 {
            break;
        }
        total += n;
        if total > 256 * 1024 * 1024 {
            return Err(Error::Setup(format!(
                "runtime input {}: exceeds 256 MiB",
                path.display()
            )));
        }
        hash.update(&data[..n]);
    }
    ContentDigest::try_from(format!("{:x}", hash.finalize()))
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

/// Exact engine168ad machine.rs::guest_cpuid filter, without sorting or omissions.
pub fn effective_cpuid(raw: &[CpuidEntry]) -> Vec<CpuidEntry> {
    let mut entries = raw.to_vec();
    for e in &mut entries {
        match (e.function, e.index) {
            (1, _) => {
                e.ebx = (e.ebx & 0xffff) | (1 << 16);
                e.ecx &= !(1 << 30 | 1 << 31 | 1 << 24 | 1 << 21 | 1 << 3);
                e.edx &= !(1 << 9);
            }
            (6, _) => e.ecx &= !1,
            (7, 0) => {
                e.ebx &= !(1 << 18 | 1 << 4 | 1 << 11);
                e.ecx &= !(1 << 22 | 1 << 5);
            }
            (0xb, _) | (0x1f, _) | (0x8000001e, _) | (0x80000022, _) => {
                e.eax = 0;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            (4, _) | (0x8000001d, _) => e.eax &= !(0x3f << 26),
            (0x80000001, _) => e.ecx &= !(1 << 2 | 1 << 23),
            (0x80000007, _) => e.edx = 1 << 8,
            (0x80000008, _) => {
                e.ecx &= !0xff;
                e.ebx &= !(1 << 4);
            }
            (f, _) if (0x4000_0000..0x5000_0000).contains(&f) => {
                e.eax = 0;
                e.ebx = 0;
                e.ecx = 0;
                e.edx = 0;
            }
            _ => {}
        }
    }
    entries
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequiredPolicy {
    pub version: String,
    pub architecture: String,
    pub vendor: String,
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub microcode: u64,
    pub host_kernel: String,
    pub engine: String,
    pub guest_kernel: String,
    pub guest_abi: String,
    pub control_channel: String,
    pub kvm_api: i32,
    pub immediate_exit: i32,
    pub guest_debug: i32,
    pub sync_regs_required: i32,
    pub perf_type: u32,
    pub perf_config: u64,
    pub perf_flags: u64,
    pub perf_period: u64,
    pub perf_read_format: u64,
    /// Source-derived expected table; actual guest observation is still required.
    pub effective_cpuid: Vec<CpuidEntry>,
}

pub fn required_policy() -> Result<RequiredPolicy> {
    Ok(RequiredPolicy {
        version: POLICY_VERSION.into(),
        architecture: "x86_64".into(),
        vendor: "AuthenticAMD".into(),
        family: 25,
        model: 33,
        stepping: 0,
        microcode: 0xa20102e,
        host_kernel: HOST_KERNEL.into(),
        engine: ENGINE.into(),
        guest_kernel: GUEST_KERNEL.into(),
        guest_abi: "oci-init-v1".into(),
        control_channel: "virtio-console-v1".into(),
        kvm_api: 12,
        immediate_exit: 1,
        guest_debug: 1,
        sync_regs_required: 3,
        perf_type: 4,
        perf_config: 0x5100d1,
        perf_flags: 1 | (1 << 2) | (1 << 19),
        perf_period: 1 << 62,
        perf_read_format: 3,
        effective_cpuid: serde_json::from_str(include_str!("admission/engine168ad-cpuid.json"))?,
    })
}

pub fn bindings() -> Result<BTreeMap<String, String>> {
    let fields = serde_json::to_string(&required_policy()?)?;
    Ok(BTreeMap::from([
        ("host_policy".into(), POLICY_VERSION.into()),
        (
            "host_policy_fingerprint".into(),
            ContentDigest::of(fields.as_bytes()).hex().to_owned(),
        ),
        ("host_policy_fields".into(), fields),
    ]))
}

pub fn validate_profile(profile: &ExecutionProfile) -> Result<()> {
    for (name, required) in bindings()? {
        match profile.semantic_parameters.get(&name) {
            None => return Err(Error::Integrity(format!("host admission proof missing: {name}; use the retained previous CLI only for historical evidence"))),
            Some(actual) if actual != &required => return Err(Error::Integrity(format!("host admission proof incompatible: {name}"))),
            _ => {}
        }
    }
    let policy = required_policy()?;
    for (name, value) in [
        ("guest_abi", &policy.guest_abi),
        ("control_channel", &policy.control_channel),
    ] {
        if profile.semantic_parameters.get(name) != Some(value) {
            return Err(Error::Integrity(format!(
                "{name} is outside the admitted host policy"
            )));
        }
    }
    let memory = profile
        .semantic_parameters
        .get("memory_mb")
        .and_then(|value| value.parse::<u32>().ok());
    if memory.is_none_or(|value| !(64..=65536).contains(&value)) {
        return Err(Error::Integrity(
            "admitted profile requires memory_mb in 64..65536".into(),
        ));
    }
    if profile.engine.hex() != ENGINE {
        return Err(Error::Integrity("engine is outside the admitted host policy; no engine-change override bypass is available".into()));
    }
    Ok(())
}

pub fn validate_kernel(kernel: &ContentDigest) -> Result<()> {
    if kernel.hex() != GUEST_KERNEL {
        return Err(Error::Integrity(
            "guest kernel is outside the admitted host policy".into(),
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
    pub booted_kernel: String,
    pub current_kernel: String,
    pub kvm_api: i32,
    pub immediate_exit: i32,
    pub guest_debug: i32,
    pub sync_regs: i32,
    pub perf_type: u32,
    pub perf_disabled_open: bool,
    pub perf_error: Option<String>,
    pub workaround_msr: u64,
    pub raw_cpuid: Vec<CpuidEntry>,
}

pub fn validate_observation(o: &CoreObservation) -> Result<()> {
    validate_fields(o, true)
}

fn validate_fields(o: &CoreObservation, require_perf: bool) -> Result<()> {
    let p = required_policy()?;
    let checks = [
        (o.architecture == p.architecture, "architecture"),
        (o.vendor == p.vendor, "CPU vendor"),
        (o.family == p.family, "CPU family"),
        (o.model == p.model, "CPU model"),
        (o.stepping == p.stepping, "CPU stepping"),
        (o.microcode == p.microcode, "microcode"),
        (
            o.booted_kernel == p.host_kernel && o.current_kernel == p.host_kernel,
            "host kernel bytes",
        ),
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
        (
            o.perf_type == p.perf_type
                && (!require_perf || (o.perf_disabled_open && o.perf_error.is_none())),
            "disabled exact-event perf readiness",
        ),
        (
            effective_cpuid(&o.raw_cpuid) == p.effective_cpuid,
            "complete effective CPUID policy",
        ),
    ];
    for (valid, field) in checks {
        if !valid {
            return Err(Error::Setup(format!("CPU {}: unsupported or drifted {field}; restore the admitted profile before execution", o.cpu)));
        }
    }
    Ok(())
}

/// The same scope boundary is exercised by portable cleanup controls.
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn scoped_probe<T, G>(
    mut guard: G,
    probe: impl FnOnce() -> Result<T>,
    restore: impl FnOnce(&mut G) -> Result<()>,
) -> Result<T> {
    let result = probe(); // Probe-owned descriptors drop before restoring affinity.
    if let Err(cleanup) = restore(&mut guard) {
        return Err(Error::Execution(format!(
            "host preflight cleanup failed: {cleanup}; observation: {}",
            result
                .as_ref()
                .err()
                .map(ToString::to_string)
                .unwrap_or_else(|| "completed".into())
        )));
    }
    result
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;

pub(crate) fn observe(cpu: u32) -> Result<CoreObservation> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::observe(cpu)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = cpu;
        Err(Error::Setup(
            "host admission requires Linux x86_64 KVM; use an admitted host".into(),
        ))
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    #[test]
    fn descriptors_close_then_affinity_restores_on_success_and_probe_errors() {
        struct Descriptor(Rc<RefCell<Vec<&'static str>>>);
        impl Drop for Descriptor {
            fn drop(&mut self) {
                self.0.borrow_mut().push("close");
            }
        }
        for fail in [false, true] {
            let events = Rc::new(RefCell::new(vec!["pin"]));
            let result = scoped_probe(
                events.clone(),
                || {
                    let _descriptor = Descriptor(events.clone());
                    events.borrow_mut().push("disabled-open");
                    if fail {
                        Err(Error::Setup("capability drift".into()))
                    } else {
                        Ok(())
                    }
                },
                |events| {
                    events.borrow_mut().push("restore");
                    Ok(())
                },
            );
            assert_eq!(result.is_err(), fail);
            assert_eq!(
                *events.borrow(),
                ["pin", "disabled-open", "close", "restore"]
            );
        }
    }
    #[test]
    fn cleanup_error_cannot_turn_probe_success_or_failure_into_admission() {
        for fail in [false, true] {
            let result = scoped_probe(
                (),
                || {
                    if fail {
                        Err(Error::Setup("probe".into()))
                    } else {
                        Ok(())
                    }
                },
                |_| Err(Error::Execution("restore failed".into())),
            );
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("preflight cleanup failed"));
        }
    }
}
