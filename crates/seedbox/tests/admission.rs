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
    let pinned = profile();
    let kernel = ContentDigest::try_from(GUEST_KERNEL.to_owned()).unwrap();
    for allow_dev_engine in [false, true] {
        admit_artifacts(&pinned, &kernel, allow_dev_engine).unwrap();
        assert!(!is_dev_engine(&pinned));
    }
    assert_eq!(POLICY_VERSION, "amd-zen3-engine62ad-v4");
    assert_eq!(
        GUEST_KERNEL,
        "68bf3f024fc6ec0c01399020b5c37e4b42d661c2604f3c87fc879ccf9280e166"
    );
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
fn independent_native_trace_order_is_required_without_dropping_fields() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/admission-cpu8-20261007.json")).unwrap();
    let raw: Vec<CpuidEntry> = serde_json::from_value(fixture["entries"].clone()).unwrap();
    assert_eq!(raw.len(), 63);
    assert_eq!(
        (raw[22].function, raw[61].function, raw[62].function),
        (0x80000000, 0x40000000, 0x40000001)
    );
    let mut o = observed();
    o.raw_cpuid = raw.clone();
    validate_observation(&o).unwrap();
    // Padding was not decoded by strace; default zero remains strict, not measured.
    let changes: [fn(&mut Vec<CpuidEntry>); 6] = [
        |c: &mut Vec<CpuidEntry>| c.sort_by_key(|e| (e.function, e.index, e.flags)),
        |c: &mut Vec<CpuidEntry>| {
            c.pop();
        },
        |c: &mut Vec<CpuidEntry>| c.push(c[0]),
        |c: &mut Vec<CpuidEntry>| c[22].eax ^= 1,
        |c: &mut Vec<CpuidEntry>| c[22].flags ^= 1,
        |c: &mut Vec<CpuidEntry>| c[22].padding[0] = 1,
    ];
    for change in changes {
        let mut o = observed();
        o.raw_cpuid = raw.clone();
        change(&mut o.raw_cpuid);
        assert!(validate_observation(&o)
            .unwrap_err()
            .to_string()
            .contains("complete effective CPUID policy"));
    }
}

// The host-kernel field was removed from the policy; historical proofs carry it.
const PREVIOUS_HOST_KERNEL: &str =
    "66111c2888091bd64e96e71a6a9e831ab5496d6c6272add5d81490396bfa3cdc";

#[test]
fn previous_full_sorted_policy_proof_remains_not_comparable_and_immutable() {
    let mut previous = required_policy().unwrap();
    previous.version = "amd-zen3-engine168ad-v1".into();
    previous.guest_kernel =
        "e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847".into();
    previous.engine = "168ad5db0e63919a3c04dbfed6c22f133aa14600747db4f64ebc3f3dc2c37c7d".into();
    // Reconstruct the old recorded proof only; production never sorts observations.
    previous
        .effective_cpuid
        .sort_by_key(|e| (e.function, e.index, e.flags));
    previous_proof_is_not_comparable(
        &previous,
        "411dc2b0286a7d8d96b142d95363500388c37f8e417e0e5483e48b2beb98c3d3",
        true,
    );
}

#[test]
fn host_kernel_pinned_policy_proof_is_not_comparable_and_immutable() {
    let mut previous = required_policy().unwrap();
    previous.version = "amd-zen3-engine52eb-v1".into();
    previous.engine = "52eb41490f521a0104ea4ab06297cbdbdd7bc9a68261febf6c5cc19854bacdcf".into();
    previous.guest_kernel =
        "e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847".into();
    previous_proof_is_not_comparable(
        &previous,
        "f9cb6aaa79501417f214b0070ec5ec38e9336567dbf92ff827fbf37da494d87d",
        true,
    );
}

#[test]
fn engine52eb_v2_policy_proof_is_not_comparable_and_immutable() {
    let mut previous = required_policy().unwrap();
    previous.version = "amd-zen3-engine52eb-v2".into();
    previous.engine = "52eb41490f521a0104ea4ab06297cbdbdd7bc9a68261febf6c5cc19854bacdcf".into();
    previous.guest_kernel =
        "e8ece7cba2d5fc4924da83e39cc672ae2516c117f8f76ecfa04aeaa98e697847".into();
    previous_proof_is_not_comparable(
        &previous,
        "c6810457fc18589220c53f99fbdef1b439ad7302ba7cb3a264ffbec78cbe4575",
        false,
    );
}

#[test]
fn engine391f_v3_policy_proof_is_not_comparable_and_immutable() {
    let mut previous = required_policy().unwrap();
    previous.version = "amd-zen3-engine391f-v3".into();
    previous.engine = "391ff19696f1bfbad144892bcaa1f831ff19f06549561420b3d17060bf316938".into();
    previous_proof_is_not_comparable(
        &previous,
        "500db94b49ffd28e4b803bedcc342fa5223f7e5d03e5e5268215309060822e14",
        false,
    );
}

fn previous_proof_is_not_comparable(
    previous: &RequiredPolicy,
    fingerprint: &str,
    had_host_kernel: bool,
) {
    use seedbox::{orchestration::*, store::ContentStore, vmm::VmmExecutor};
    use std::fs;
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("previous-full-proof");
    fs::create_dir(&original).unwrap();
    let mut record = serde_json::from_value::<RunRecord>(
        serde_json::from_slice::<serde_json::Value>(include_bytes!(
            "fixtures/formats/demo41-fail-v2.json"
        ))
        .unwrap()["record"]
            .clone(),
    )
    .unwrap();
    let microcode = format!("\"microcode\":{},", previous.microcode);
    let old_fields = serde_json::to_string(previous).unwrap();
    let old_fields = if had_host_kernel {
        old_fields.replacen(
            &microcode,
            &format!("{microcode}\"host_kernel\":\"{PREVIOUS_HOST_KERNEL}\","),
            1,
        )
    } else {
        old_fields
    };
    let old_fingerprint = ContentDigest::of(old_fields.as_bytes());
    assert_eq!(old_fingerprint.hex(), fingerprint);
    assert_ne!(
        bindings().unwrap()["host_policy_fingerprint"],
        "8da34a9c7dfdd133bf4a93cf5b5cc7046ad30985e2352e9d21212ec8333024a5"
    );
    assert_ne!(
        old_fingerprint.hex(),
        bindings().unwrap()["host_policy_fingerprint"]
    );
    record.identity.inputs.profile.engine =
        ContentDigest::try_from(previous.engine.clone()).unwrap();
    record.identity.inputs.kernel = ContentDigest::try_from(previous.guest_kernel.clone()).unwrap();
    record
        .identity
        .inputs
        .profile
        .semantic_parameters
        .extend(BTreeMap::from([
            ("host_policy".into(), previous.version.clone()),
            ("host_policy_fields".into(), old_fields),
            (
                "host_policy_fingerprint".into(),
                old_fingerprint.hex().into(),
            ),
        ]));
    record.run_id = ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "sha256": ContentDigest::of(&serde_json::to_vec(&record).unwrap()),
        "record": record,
    }))
    .unwrap();
    fs::write(original.join("manifest.json"), &manifest).unwrap();
    inspect(&original).unwrap();
    let engine = root.path().join("unused-engine");
    fs::write(&engine, b"not executed").unwrap();
    let store = ContentStore::new(root.path().join("objects"));
    let executor = VmmExecutor::new(
        &engine,
        &store,
        None,
        512,
        WitnessMode::Exits,
        seedbox::counter::CounterProfile::Zen,
    )
    .unwrap();
    let runner = Orchestrator::new(store, root.path().join("runs"));
    for allow_engine_change in [false, true] {
        let result = runner
            .replay_with_options(
                &original,
                &executor,
                ReplayOptions {
                    allow_engine_change,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(result.record.outcome, Outcome::Error);
        assert_eq!(
            result.record.replay.unwrap().status,
            ReplayStatus::NotComparable
        );
        assert!(result
            .record
            .error
            .unwrap()
            .contains("host admission proof incompatible"));
        assert!(!result.directory.join("output").exists());
        assert!(!result.directory.join("inputs").exists());
        assert_eq!(fs::read(original.join("manifest.json")).unwrap(), manifest);
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
        let executor = VmmExecutor::new(
            &engine,
            &store,
            None,
            512,
            WitnessMode::Exits,
            seedbox::counter::CounterProfile::Zen,
        )
        .unwrap();
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

#[test]
fn successor_release_rejects_previous_exact_engine_and_policy_without_migration() {
    let mut previous = profile();
    previous.engine = ContentDigest::try_from(
        "168ad5db0e63919a3c04dbfed6c22f133aa14600747db4f64ebc3f3dc2c37c7d".to_owned(),
    )
    .unwrap();
    assert!(validate_profile(&previous).is_err());
    previous = profile();
    previous
        .semantic_parameters
        .insert("host_policy".into(), "amd-zen3-engine168ad-v1".into());
    assert!(validate_profile(&previous)
        .unwrap_err()
        .to_string()
        .contains("host admission proof incompatible"));
    assert_eq!(
        ENGINE,
        "62adf0563a0a20f6bc5ea23c15060347d84b7ea2d6c7adc7eb85edcd46b0b84c"
    );
}
