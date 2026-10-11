//! Admission policy and its application orchestration boundary; no KVM activity.
use seedbox_application::{admission::*, execution::*, store::ContentDigest, Error, Result};
use std::{collections::BTreeMap, path::Path};

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
        backend: Backend::Kvm,
        engine: ContentDigest::of(b"any engine"),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: SemanticParameters::new(512),
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
    validate_profile(&p).unwrap();
    for memory_mb in [63, 65537] {
        let mut wrong = p.clone();
        wrong.semantic_parameters.memory_mb = memory_mb;
        assert!(validate_profile(&wrong).is_err());
    }
    let json = serde_json::to_value(&p).unwrap();
    for key in ["guest_abi", "control_channel", "memory_mb"] {
        let mut wrong = json.clone();
        wrong["semantic_parameters"][key] = "wrong".into();
        assert!(serde_json::from_value::<ExecutionProfile>(wrong.clone()).is_err());
        wrong["semantic_parameters"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        assert!(serde_json::from_value::<ExecutionProfile>(wrong).is_err());
    }
    // Parameters an older CLI recorded are not part of this format.
    let mut older = json;
    older["semantic_parameters"]["host_policy"] = "recorded by an older CLI".into();
    assert!(serde_json::from_value::<ExecutionProfile>(older).is_err());
}

#[test]
fn preflight_refusal_is_before_application_preparation_or_execution() {
    use seedbox_application::{
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
                backend: Backend::Other("fixture".into()),
                engine: ContentDigest::of(b"synthetic"),
                witness_mode: WitnessMode::Exits,
                semantic_parameters: SemanticParameters::new(512),
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
