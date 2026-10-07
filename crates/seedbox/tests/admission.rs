//! Synthetic policy tests; no KVM/perf/MSR activity or host qualification.
use seedbox::{admission::*, execution::*, store::ContentDigest};
use std::collections::BTreeMap;

fn observed() -> CoreObservation {
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
        sync_regs: 7,
        perf_type: p.perf_type,
        perf_disabled_open: true,
        perf_error: None,
        workaround_msr: 0,
        raw_cpuid: p.effective_cpuid,
    }
}
fn profile() -> ExecutionProfile {
    ExecutionProfile {
        backend: "seedbox-kvm".into(),
        engine: ContentDigest::try_from(ENGINE.to_owned()).unwrap(),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: {
            let mut values = bindings().unwrap();
            values.extend(BTreeMap::from([
                ("guest_abi".into(), "oci-init-v1".into()),
                ("control_channel".into(), "virtio-console-v1".into()),
                ("memory_mb".into(), "512".into()),
            ]));
            values
        },
    }
}

#[test]
fn complete_policy_matches_without_host_enrollment_or_incidental_core_identity() {
    let mut first = observed();
    let mut second = first.clone();
    second.cpu = 10;
    second.workaround_msr = 1 << 54;
    for (entry, id) in [(&mut first, 0x10), (&mut second, 0x14)] {
        let leaf = entry
            .raw_cpuid
            .iter_mut()
            .find(|e| e.function == 1)
            .unwrap();
        leaf.ebx |= id << 24;
        entry
            .raw_cpuid
            .iter_mut()
            .find(|e| e.function == 0xb)
            .unwrap()
            .edx = id;
    }
    assert_ne!(first.raw_cpuid, second.raw_cpuid);
    assert_eq!(
        effective_cpuid(&first.raw_cpuid),
        effective_cpuid(&second.raw_cpuid)
    );
    validate_observation(&first).unwrap();
    validate_observation(&second).unwrap();
    validate_profile(&profile()).unwrap();
    assert_eq!(std::mem::size_of::<CpuidEntry>(), 40);
}

#[test]
fn every_required_host_drift_or_missing_readiness_is_rejected() {
    let changes: Vec<fn(&mut CoreObservation)> = vec![
        |o| o.architecture = "aarch64".into(),
        |o| o.vendor = "GenuineIntel".into(),
        |o| o.family += 1,
        |o| o.model += 1,
        |o| o.stepping += 1,
        |o| o.microcode += 1,
        |o| o.booted_kernel = "different".into(),
        |o| o.current_kernel = "different".into(),
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
fn cpuid_order_flags_indices_padding_and_unmasked_bits_are_not_omitted() {
    let changes: Vec<fn(&mut Vec<CpuidEntry>)> = vec![
        |c| {
            c.pop();
        },
        |c| c.push(CpuidEntry::default()),
        |c| c.swap(0, 1),
        |c| c[0].flags ^= 1,
        |c| c[0].index += 1,
        |c| c[0].padding[1] = 1,
        |c| c[0].ebx ^= 1,
        |c| c[0].eax ^= 1,
        |c| c[0].ecx ^= 1,
        |c| c[0].edx ^= 1,
        |c| c.iter_mut().find(|e| e.function == 1).unwrap().ecx ^= 1,
    ];
    for change in changes {
        let mut o = observed();
        change(&mut o.raw_cpuid);
        assert!(validate_observation(&o).is_err());
    }
}

#[test]
fn old_or_drifted_proof_is_distinct_from_engine_mismatch_and_cannot_be_synthesized() {
    for key in bindings().unwrap().keys() {
        let mut p = profile();
        p.semantic_parameters.remove(key);
        assert!(validate_profile(&p)
            .unwrap_err()
            .to_string()
            .contains("host admission proof missing"));
        let mut p = profile();
        p.semantic_parameters.insert(key.clone(), "drift".into());
        assert!(validate_profile(&p)
            .unwrap_err()
            .to_string()
            .contains("host admission proof incompatible"));
    }
    let mut p = profile();
    p.engine = ContentDigest::of(b"another engine");
    assert!(validate_profile(&p)
        .unwrap_err()
        .to_string()
        .contains("engine is outside"));
    let p = ExecutionProfile {
        semantic_parameters: BTreeMap::new(),
        ..profile()
    };
    assert!(validate_profile(&p).is_err());
    assert!(validate_kernel(&ContentDigest::of(b"other kernel")).is_err());
    validate_kernel(&ContentDigest::try_from(GUEST_KERNEL.to_owned()).unwrap()).unwrap();
}

#[test]
fn unchanged_actual_and_synthetic_historical_records_are_readable_but_not_admitted() {
    use seedbox::orchestration::{self, Orchestrator, Outcome, ReplayOptions, ReplayStatus};
    use seedbox::{store::ContentStore, vmm::VmmExecutor};
    use std::fs;
    for manifest in [
        include_bytes!("fixtures/formats/demo41-fail-v2.json").as_slice(),
        include_bytes!("fixtures/formats/synthetic-c84-v2.json").as_slice(),
    ] {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("original");
        fs::create_dir(&original).unwrap();
        fs::write(original.join("manifest.json"), manifest).unwrap();
        let record = orchestration::inspect(&original).unwrap();
        assert_eq!(record.outcome, Outcome::Fail);
        let engine = root.path().join("test-only-executor-engine");
        fs::write(&engine, b"synthetic executor bytes; never execute").unwrap();
        let store = ContentStore::new(root.path().join("objects"));
        let executor = VmmExecutor::new(&engine, &store, None, 512, WitnessMode::Exits).unwrap();
        let runner = Orchestrator::new(store, root.path().join("runs"));
        for allow_engine_change in [false, true] {
            let rejected = runner
                .replay_with_options(
                    &original,
                    &executor,
                    ReplayOptions {
                        allow_engine_change,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(rejected.record.outcome, Outcome::Error);
            assert_eq!(
                rejected.record.replay.as_ref().unwrap().status,
                ReplayStatus::NotComparable
            );
            assert_eq!(rejected.record.error.as_deref(),Some("input integrity: host admission proof missing: host_policy; use the retained previous CLI only for historical evidence"));
            assert!(!rejected.directory.join("output").exists());
            assert!(!rejected.directory.join("inputs").exists());
            assert_eq!(fs::read(original.join("manifest.json")).unwrap(), manifest);
        }
    }
}
