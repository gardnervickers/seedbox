//! Synthetic readiness tests; no KVM/perf/MSR activity.
use seedbox::{admission::*, execution::*, store::ContentDigest};
use std::collections::BTreeMap;

fn observed() -> CoreObservation {
    let p = required_policy();
    CoreObservation {
        cpu: 8,
        architecture: p.architecture,
        vendor: "AuthenticAMD".into(),
        family: 25,
        model: 33,
        stepping: 0,
        microcode: 0xa20102e,
        kvm_api: p.kvm_api,
        immediate_exit: p.immediate_exit,
        guest_debug: p.guest_debug,
        sync_regs: 7,
        perf_type: p.perf_type,
        perf_disabled_open: true,
        perf_error: None,
        workaround_msr: 0,
        raw_cpuid: vec![CpuidEntry {
            function: 1,
            ..Default::default()
        }],
    }
}
fn profile() -> ExecutionProfile {
    ExecutionProfile {
        backend: "seedbox-kvm".into(),
        engine: ContentDigest::of(b"any engine"),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: BTreeMap::from([
            ("guest_abi".into(), "oci-init-v1".into()),
            ("control_channel".into(), "virtio-console-v1".into()),
            ("memory_mb".into(), "512".into()),
        ]),
    }
}

#[test]
fn cpu_vendor_model_microcode_and_cpuid_are_not_pinned() {
    let mut o = observed();
    validate_observation(&o).unwrap();
    o.vendor = "GenuineIntel".into();
    o.family = 6;
    o.model = 0x55;
    o.workaround_msr = 0;
    o.family += 1;
    o.model += 1;
    o.stepping += 1;
    o.microcode += 1;
    o.raw_cpuid[0].ecx ^= 1;
    validate_observation(&o).unwrap();
}

#[test]
fn every_missing_engine_capability_is_rejected() {
    let changes: Vec<fn(&mut CoreObservation)> = vec![
        |o| o.architecture = "aarch64".into(),
        |o| o.kvm_api = 11,
        |o| o.immediate_exit = 0,
        |o| o.guest_debug = 0,
        |o| o.sync_regs = 1,
        |o| o.perf_type = 9,
        |o| o.perf_disabled_open = false,
        |o| o.perf_error = Some("failed open".into()),
    ];
    for change in changes {
        let mut o = observed();
        change(&mut o);
        assert!(validate_observation(&o).is_err());
    }
}

#[test]
fn any_engine_is_admitted_and_older_records_keep_their_extra_parameters() {
    let p = profile();
    validate_profile(&p).unwrap();
    let mut older = p.clone();
    for key in [
        "host_policy",
        "host_policy_fingerprint",
        "host_policy_fields",
    ] {
        older
            .semantic_parameters
            .insert(key.into(), "recorded by an older CLI".into());
    }
    validate_profile(&older).unwrap();
    for key in ["guest_abi", "control_channel", "memory_mb"] {
        let mut wrong = p.clone();
        wrong.semantic_parameters.insert(key.into(), "wrong".into());
        assert!(validate_profile(&wrong).is_err());
        wrong.semantic_parameters.remove(key);
        assert!(validate_profile(&wrong).is_err());
    }
}
