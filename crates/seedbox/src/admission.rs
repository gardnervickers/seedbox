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
    let counter = crate::vmm::recorded_counter(profile)?;
    if counter != crate::counter::CounterProfile::Zen
        && profile
            .semantic_parameters
            .keys()
            .any(|k| k.starts_with("host_policy"))
    {
        return Err(Error::Integrity(
            "unqualified counter cannot carry qualified AMD host policy bindings".into(),
        ));
    }
    let required_bindings = if counter == crate::counter::CounterProfile::Zen {
        bindings()?
    } else {
        BTreeMap::new()
    };
    for (name, required) in required_bindings {
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
            effective_cpuid(&o.raw_cpuid) == p.effective_cpuid,
            "complete effective CPUID policy",
        ),
        (
            o.perf_type == p.perf_type
                && (!require_perf || (o.perf_disabled_open && o.perf_error.is_none())),
            "disabled exact-event perf readiness",
        ),
    ];
    for (valid, field) in checks {
        if !valid {
            return Err(Error::Setup(format!("CPU {}: unsupported or drifted {field}; restore the admitted profile before execution", o.cpu)));
        }
    }
    Ok(())
}

// Retain static observations on refusal; no perf event is attempted until every
// static field (including complete ordered CPUID) is valid. Final validation uses
// that same order, so an unattempted event cannot conceal the original failure.
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn probe_disabled_perf<T>(
    mut observation: CoreObservation,
    open: impl FnOnce() -> Result<T>,
) -> CoreObservation {
    if validate_fields(&observation, false).is_err() {
        return observation;
    }
    match open() {
        Ok(descriptor) => {
            drop(descriptor);
            observation.perf_disabled_open = true;
        }
        Err(error) => observation.perf_error = Some(error.to_string()),
    }
    observation
}

// observe returns only after its descriptor/affinity cleanup. Retain that
// observation, then fail immediately on established refusal before another core
// can replace it with an unrelated collection error.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn collect_admitted(
    cpus: impl IntoIterator<Item = u32>,
    observations: &mut Vec<CoreObservation>,
    mut observe: impl FnMut(u32) -> Result<CoreObservation>,
) -> Result<()> {
    for cpu in cpus {
        let observation = observe(cpu)?;
        let admission = validate_observation(&observation);
        observations.push(observation);
        admission?;
    }
    Ok(())
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

// PERF_FLAG_FD_CLOEXEC: set atomically by perf_event_open, never by later fcntl.
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
const PERF_OPEN_FLAGS: u64 = 1 << 3;

#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn checked_affinity_restore(
    expected: &[u32],
    set: impl FnOnce() -> Result<()>,
    read: impl FnOnce() -> Result<Vec<u32>>,
) -> Result<()> {
    set()?;
    if read()? != expected {
        return Err(Error::Execution(
            "restored preflight affinity differs from original; cleanup is unverified".into(),
        ));
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

/// Applicable readiness for explicitly unqualified execution, never AMD qualification.
#[cfg(target_os = "linux")]
pub(crate) fn check_unqualified(
    cpus: impl IntoIterator<Item = u32>,
    counter: crate::counter::CounterProfile,
) -> Result<()> {
    #[cfg(target_arch = "x86_64")]
    {
        for cpu in cpus {
            linux::check_unqualified(cpu, counter)?;
        }
        Ok(())
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (cpus, counter);
        Err(Error::Setup(
            "experimental execution requires x86-64 Linux KVM".into(),
        ))
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    fn static_observation() -> CoreObservation {
        let p = required_policy().unwrap();
        CoreObservation {
            cpu: 8,
            architecture: p.architecture,
            vendor: p.vendor,
            family: p.family,
            model: p.model,
            stepping: p.stepping,
            microcode: p.microcode,
            booted_kernel: p.host_kernel.clone(),
            current_kernel: p.host_kernel,
            kvm_api: p.kvm_api,
            immediate_exit: p.immediate_exit,
            guest_debug: p.guest_debug,
            sync_regs: p.sync_regs_required,
            perf_type: p.perf_type,
            perf_disabled_open: false,
            perf_error: None,
            workaround_msr: 0,
            raw_cpuid: p.effective_cpuid,
        }
    }
    #[test]
    fn static_cpuid_refusal_is_honest_and_does_not_attempt_perf() {
        use std::cell::Cell;
        let attempted = Cell::new(false);
        let mut observation = static_observation();
        observation.raw_cpuid[0].flags ^= 1;
        let original = validate_fields(&observation, false)
            .unwrap_err()
            .to_string();
        let observed = probe_disabled_perf(observation, || {
            attempted.set(true);
            Ok(())
        });
        assert!(!attempted.get());
        assert!(!observed.perf_disabled_open);
        assert!(observed.perf_error.is_none());
        let final_error = validate_observation(&observed).unwrap_err().to_string();
        assert_eq!(final_error, original);
        assert!(final_error.contains("complete effective CPUID policy"));
    }
    #[test]
    fn actual_perf_open_refusal_preserves_its_cause_and_cleanup_precedence() {
        use std::cell::Cell;
        let attempted = Cell::new(false);
        let observed = probe_disabled_perf(static_observation(), || {
            attempted.set(true);
            Err::<(), _>(Error::Setup("perf_event_open: EACCES (13)".into()))
        });
        assert!(attempted.get());
        assert!(!observed.perf_disabled_open);
        assert_eq!(
            observed.perf_error.as_deref(),
            Some("perf_event_open: EACCES (13)")
        );
        assert!(validate_observation(&observed)
            .unwrap_err()
            .to_string()
            .contains("disabled exact-event perf readiness"));

        let mut static_refusal = static_observation();
        static_refusal.raw_cpuid[0].flags ^= 1;
        let result = scoped_probe(
            (),
            || {
                Ok(probe_disabled_perf::<()>(static_refusal, || {
                    panic!("unexpected perf attempt")
                }))
            },
            |_| Err(Error::Execution("restore readback differs".into())),
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("host preflight cleanup failed"));
        assert!(error.contains("restore readback differs"));
    }
    #[test]
    fn first_sibling_static_refusal_cannot_be_masked_by_later_collection_error() {
        let mut observations = Vec::new();
        let mut attempted = Vec::new();
        let result = collect_admitted([8, 24], &mut observations, |cpu| {
            attempted.push(cpu);
            if cpu == 24 {
                return Err(Error::Setup("later sibling collection failed".into()));
            }
            let mut o = static_observation();
            o.raw_cpuid[0].flags ^= 1;
            Ok(o)
        });
        assert_eq!(attempted, [8]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].raw_cpuid[0].flags, 1);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("complete effective CPUID policy"));
    }
    #[test]
    fn first_sibling_cleanup_failure_wins_and_later_collection_is_not_attempted() {
        let mut observations = Vec::new();
        let mut attempted = Vec::new();
        let mut restored = false;
        let result = collect_admitted([8, 24], &mut observations, |cpu| {
            attempted.push(cpu);
            if cpu == 24 {
                return Err(Error::Setup("later sibling collection failed".into()));
            }
            scoped_probe(
                (),
                || {
                    let mut o = static_observation();
                    o.raw_cpuid[0].flags ^= 1;
                    Ok(o)
                },
                |_| {
                    restored = true;
                    Err(Error::Execution("restore readback differs".into()))
                },
            )
        });
        assert!(restored);
        assert_eq!(attempted, [8]);
        assert!(observations.is_empty());
        let error = result.unwrap_err().to_string();
        assert!(error.contains("host preflight cleanup failed"));
        assert!(error.contains("restore readback differs"));
    }
    #[test]
    fn evidence_failure_cannot_replace_static_or_cleanup_refusal_or_admit_success() {
        let mut drift = static_observation();
        drift.raw_cpuid[0].flags ^= 1;
        let static_refusal = validate_observation(&drift);
        let cleanup_refusal = scoped_probe(
            (),
            || validate_observation(&drift),
            |_| Err(Error::Execution("restore readback differs".into())),
        );
        for (admission, cause) in [
            (static_refusal, "complete effective CPUID policy"),
            (cleanup_refusal, "host preflight cleanup failed"),
        ] {
            let mut published = false;
            let error = finish_admission(admission, |established| {
                assert!(established.is_err());
                published = true;
                Err(Error::Setup("injected evidence ENOSPC".into()))
            })
            .unwrap_err()
            .to_string();
            assert!(published);
            assert!(error.contains(cause));
            assert!(error.contains("additionally, evidence publication failed"));
            assert!(error.contains("injected evidence ENOSPC"));
        }
        let error = finish_admission(Ok(()), |established| {
            assert!(established.is_ok());
            Err(Error::Setup("injected evidence ENOSPC".into()))
        })
        .unwrap_err()
        .to_string();
        assert_eq!(error, "injected evidence ENOSPC");

        let bounded = finish_admission(Err(Error::Execution("r".repeat(10000))), |_| {
            Err(Error::Execution("p".repeat(10000)))
        })
        .unwrap_err()
        .to_string();
        assert!(bounded.len() < 8400);
        assert!(bounded.contains("[truncated]"));
    }
    #[test]
    fn perf_open_requires_atomic_close_on_exec() {
        // Linux UAPI PERF_FLAG_FD_CLOEXEC, independent of perf event attributes.
        assert_eq!(PERF_OPEN_FLAGS, 8);
    }
    #[test]
    fn successful_restore_setter_cannot_hide_restricted_or_unreadable_affinity() {
        use std::cell::Cell;
        for observed in [
            Ok(vec![2, 18]),
            Ok(vec![2]),
            Err(Error::Execution("readback failed".into())),
        ] {
            let set = Cell::new(false);
            let should_pass = observed.as_ref().is_ok_and(|cpus| cpus == &[2, 18]);
            let result = scoped_probe(
                (),
                || Ok(()),
                |_| {
                    checked_affinity_restore(
                        &[2, 18],
                        || {
                            set.set(true);
                            Ok(())
                        },
                        || observed,
                    )
                },
            );
            assert!(set.get());
            assert_eq!(result.is_ok(), should_pass);
            if !should_pass {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("preflight cleanup failed"));
            }
        }
    }
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

