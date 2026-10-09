//! Host readiness: the KVM and perf capabilities the engine needs on this core.
//! It does not pin CPU models, microcode, CPUID, engines or kernels.
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
    let mut file = std::fs::File::open(path).context(
        Error::Setup,
        format_args!("runtime input {}", path.display()),
    )?;
    let mut hash = Sha256::new();
    let mut data = [0; 65536];
    let mut total = 0;
    loop {
        let n = file.read(&mut data).context(
            Error::Setup,
            format_args!("runtime input {}", path.display()),
        )?;
        if n == 0 {
            break;
        }
        total += n;
        if total as u64 > RUNTIME_INPUT_LIMIT {
            return Err(Error::Setup(format!(
                "runtime input {}: exceeds {}",
                path.display(),
                Size(RUNTIME_INPUT_LIMIT)
            )));
        }
        hash.update(&data[..n]);
    }
    Ok(ContentDigest::finish(hash))
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
    pub perf_type: u32,
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
        perf_type: 4,
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
        (
            o.perf_type == p.perf_type
                && (!require_perf || (o.perf_disabled_open && o.perf_error.is_none())),
            "disabled exact-event perf readiness",
        ),
    ];
    for (valid, field) in checks {
        if !valid {
            return Err(Error::Setup(format!("CPU {}: unsupported {field}", o.cpu)));
        }
    }
    Ok(())
}

// Retain static observations on refusal; no perf event is attempted until every
// static capability is valid. Final validation uses
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

/// Disabled readiness attributes. Counting fields match the VMM's Counter::new;
/// admission alone sets CLOEXEC atomically, protecting descriptor inheritance.
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DisabledCounterSpec {
    pub kind: u32,
    pub config: u64,
    pub period: u64,
    pub read_format: u64,
    pub flags: u64,
    pub open_flags: u64,
}
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
pub(crate) fn disabled_counter_spec(
    counter: crate::counter::CounterProfile,
) -> DisabledCounterSpec {
    DisabledCounterSpec {
        kind: 4,
        config: counter.branch_event(),
        period: 1 << 62,
        read_format: 3,
        flags: 1 | (1 << 2) | (1 << 19),
        open_flags: PERF_OPEN_FLAGS,
    }
}
#[cfg(any(all(target_os = "linux", target_arch = "x86_64"), test))]
fn probe_selected_event<T>(
    counter: crate::counter::CounterProfile,
    open: impl FnOnce(DisabledCounterSpec) -> Result<T>,
) -> Result<()> {
    let descriptor = open(disabled_counter_spec(counter))?;
    drop(descriptor); // No enable, map, read or descriptor survives affinity restore.
    Ok(())
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

pub(crate) fn observe_counter(
    cpu: u32,
    counter: crate::counter::CounterProfile,
) -> Result<CoreObservation> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        linux::observe(cpu, counter)
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (cpu, counter);
        Err(Error::Setup(
            "host admission requires Linux x86_64 KVM; use an admitted host".into(),
        ))
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::{cell::RefCell, rc::Rc};
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
            perf_type: p.perf_type,
            perf_disabled_open: false,
            perf_error: None,
            workaround_msr: 0,
            raw_cpuid: Vec::new(),
        }
    }
    #[test]
    fn static_capability_refusal_is_honest_and_does_not_attempt_perf() {
        use std::cell::Cell;
        let attempted = Cell::new(false);
        let mut observation = static_observation();
        observation.immediate_exit = 0;
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
        assert!(final_error.contains("KVM immediate-exit capability"));
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
        static_refusal.immediate_exit = 0;
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
            o.immediate_exit = 0;
            Ok(o)
        });
        assert_eq!(attempted, [8]);
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].immediate_exit, 0);
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("KVM immediate-exit capability"));
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
                    o.immediate_exit = 0;
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
        drift.immediate_exit = 0;
        let static_refusal = validate_observation(&drift);
        let cleanup_refusal = scoped_probe(
            (),
            || validate_observation(&drift),
            |_| Err(Error::Execution("restore readback differs".into())),
        );
        for (admission, cause) in [
            (static_refusal, "KVM immediate-exit capability"),
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
    fn selected_disabled_events_match_vmm_counting_fields_and_close_before_restore() {
        use crate::counter::CounterProfile;
        use std::{io::Read, os::unix::net::UnixStream, time::Duration};
        let policy = required_policy();
        for (counter, event) in [
            (CounterProfile::Zen, 0x5100d1),
            (CounterProfile::IntelSkylake, 0x5101c4),
            (CounterProfile::IntelIceLake, 0x5111c4),
        ] {
            let spec = disabled_counter_spec(counter);
            assert_eq!(
                (
                    spec.kind,
                    spec.config,
                    spec.period,
                    spec.read_format,
                    spec.flags,
                    spec.open_flags
                ),
                (4, event, 1 << 62, 3, 1 | (1 << 2) | (1 << 19), 8)
            );
            if counter == CounterProfile::Zen {
                assert_eq!(spec.kind, policy.perf_type);
            }
            let (mut reader, writer) = UnixStream::pair().unwrap();
            reader
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            scoped_probe(
                (),
                || {
                    probe_selected_event(counter, |actual| {
                        assert_eq!(actual, spec);
                        Ok(writer)
                    })
                },
                |_| {
                    let mut byte = [0];
                    assert_eq!(
                        reader.read(&mut byte).unwrap(),
                        0,
                        "probe descriptor must close before restoring affinity"
                    );
                    Ok(())
                },
            )
            .unwrap();
            for restore_fails in [false, true] {
                let mut restored = false;
                let error = scoped_probe(
                    (),
                    || {
                        probe_selected_event::<()>(counter, |actual| {
                            assert_eq!(actual.config, event);
                            Err(Error::Setup(
                                "perf_event_open: Permission denied (os error 13)".into(),
                            ))
                        })
                    },
                    |_| {
                        restored = true;
                        if restore_fails {
                            Err(Error::Execution("affinity readback mismatch".into()))
                        } else {
                            Ok(())
                        }
                    },
                )
                .unwrap_err()
                .to_string();
                assert!(restored);
                assert!(error.contains("os error 13"));
                assert_eq!(
                    error.contains("host preflight cleanup failed"),
                    restore_fails
                );
            }
        }
    }

    #[test]
    fn selected_perf_refusal_is_before_application_preparation_or_execution() {
        use crate::{
            config,
            counter::CounterProfile,
            execution::*,
            orchestration::{Orchestrator, RunOptions},
            store::ContentStore,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Refused;
        impl ExecutionBackend for Refused {
            fn profile(&self) -> ExecutionProfile {
                ExecutionProfile {
                    backend: Backend::Other("fixture".into()),
                    engine: ContentDigest::of(b"synthetic"),
                    witness_mode: WitnessMode::Exits,
                    semantic_parameters: SemanticParameters::new(512),
                }
            }
            fn preflight(&self, _: Option<&Path>) -> Result<()> {
                scoped_probe(
                    (),
                    || {
                        probe_selected_event::<()>(CounterProfile::IntelIceLake, |spec| {
                            assert_eq!(spec.config, 0x5111c4);
                            Err(Error::Setup("selected event refused: EACCES (13)".into()))
                        })
                    },
                    |_| Ok(()),
                )
            }
            fn execute(&self, _: ExecutionRequest<'_>) -> Result<ExecutionReport> {
                panic!("must not execute after event refusal")
            }
        }
        struct Prepare(AtomicUsize);
        impl PreparationBackend for Prepare {
            fn prepare(&self, _: &config::Simulation, _: &ContentStore) -> Result<PreparedInputs> {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("must not prepare after event refusal")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let prepare = Prepare(AtomicUsize::new(0));
        let simulation=config::parse("name: event-refusal\nservices:\n  app: {image: fixture:synthetic}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",&root.path().join("compose.yaml"),&BTreeMap::new()).unwrap();
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
