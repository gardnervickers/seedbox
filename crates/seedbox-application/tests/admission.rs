//! Synthetic readiness tests; no KVM activity.
use seedbox_application::{admission::*, execution::*};

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
        raw_cpuid: vec![CpuidEntry {
            function: 1,
            ..Default::default()
        }],
    }
}
fn profile() -> ExecutionProfile {
    ExecutionProfile {
        backend: "fixture".into(),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: SemanticParameters::new(),
    }
}

#[test]
fn cpu_vendor_model_microcode_and_cpuid_are_not_pinned() {
    let mut o = observed();
    validate_observation(&o).unwrap();
    o.vendor = "GenuineIntel".into();
    o.family = 6;
    o.model = 0x55;
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
    ];
    for change in changes {
        let mut o = observed();
        change(&mut o);
        assert!(validate_observation(&o).is_err());
    }
}

#[test]
fn any_engine_is_admitted_and_settings_are_decoded_strictly() {
    let p = profile();
    let json = serde_json::to_value(&p).unwrap();
    for key in ["guest_abi", "control_channel", "memory_mb"] {
        let mut wrong = json.clone();
        wrong["semantic_parameters"][key] = "wrong".into();
        assert!(serde_json::from_value::<ExecutionProfile>(wrong.clone()).is_err());
    }
    // Parameters an older CLI recorded are not part of this format.
    let mut older = json;
    older["semantic_parameters"]["host_policy"] = "recorded by an older CLI".into();
    assert!(serde_json::from_value::<ExecutionProfile>(older).is_err());
}
