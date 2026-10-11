use super::*;
use crate::admission::{required_policy, CoreObservation};
use crate::engine_adapter::{engine_command, recorded_checkpoint, recorded_features};

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn internal_doctor_reports_simulator_version() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("worker");
    std::fs::write(&executable, b"combined executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let kernel = ContentDigest::of(b"kernel");
    let executor = VmmExecutor::new_internal(&executable, &store, 512, WitnessMode::Exits).unwrap();
    let report = executor
        .doctor_report(
            kernel,
            AdmissionProbe {
                observations: vec![observed(8)],
                result: Ok(()),
            },
        )
        .unwrap();
    assert_eq!(
        report.simulator_version.as_deref(),
        Some(crate::engine::SIMULATOR_VERSION)
    );
}

#[test]
fn retained_worker_launch_preserves_timer_and_deferred_tx_options() {
    let executor =
        VmmExecutor::from_engine_digest(ContentDigest::of(b"engine"), 512, WitnessMode::Exits)
            .unwrap()
            .with_page_timer(true)
            .with_deferred_tx(true);
    for internal in [false, true] {
        let mut profile = executor.profile.clone();
        if internal {
            profile.semantic_parameters.engine = Some(crate::engine::kvm_descriptor(
                crate::engine::ExecutableKind::InternalWorker,
                crate::engine::SIMULATOR_VERSION.into(),
            ));
        }
        let retained: ExecutionProfile =
            serde_json::from_slice(&serde_json::to_vec(&profile).unwrap()).unwrap();
        let command = engine_command(Path::new("retained-engine"), &retained).unwrap();
        let mut expected = Vec::new();
        if internal {
            expected.extend(["__engine", crate::engine::INTERNAL_WORKER_V1]);
        }
        assert_eq!(command.get_args().collect::<Vec<_>>(), expected);
        assert_eq!(
            recorded_features(&retained).unwrap(),
            seedbox_engine_core::launch::MachineFeatures {
                page_timer: true,
                deferred_tx: true,
            }
        );
        // Recorded flags are booleans; a string is not a TX mode.
        let mut json = serde_json::to_value(&profile).unwrap();
        json["semantic_parameters"]["deferred_tx"] = "false".into();
        assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
    }
}

#[test]
fn parallel_preflight_rejects_worker_counts_outside_the_supported_range() {
    let executor =
        VmmExecutor::from_engine_digest(ContentDigest::of(b"engine"), 512, WitnessMode::Exits)
            .unwrap();
    for workers in [0, 33] {
        assert!(executor
            .preflight_workers(workers)
            .unwrap_err()
            .to_string()
            .contains("--workers: use 1..32"));
    }
}

fn observed(cpu: u32) -> CoreObservation {
    let p = required_policy();
    CoreObservation {
        cpu,
        architecture: p.architecture,
        vendor: "AuthenticAMD".into(),
        family: 25,
        model: 33,
        stepping: 0,
        microcode: 0,
        kvm_api: p.kvm_api,
        immediate_exit: p.immediate_exit,
        guest_debug: p.guest_debug,
        sync_regs: 7,
        raw_cpuid: Vec::new(),
    }
}
#[test]
fn replay_reconstructs_checkpoint_profile_without_performance_changes() {
    let ordinary =
        VmmExecutor::from_engine_digest(ContentDigest::of(b"engine"), 512, WitnessMode::Exits)
            .unwrap();
    let slower_watchdog = ordinary.clone().with_watchdog_ms(200).unwrap();
    assert_eq!(slower_watchdog.profile(), ordinary.profile());
    assert_eq!(
        slower_watchdog.performance_parameters()["SEEDBOX_WATCHDOG_MS"],
        "200"
    );
    for invalid in [0, 60001] {
        assert!(ordinary.clone().with_watchdog_ms(invalid).is_err());
    }
    let branch = ordinary.clone().with_checkpoint(500, 42);
    assert_eq!(
        branch.performance_parameters(),
        ordinary.performance_parameters()
    );
    assert_eq!(
        recorded_checkpoint(&branch.profile()).unwrap(),
        Some((500, 42))
    );
    assert_eq!(recorded_checkpoint(&ordinary.profile()).unwrap(), None);
}

#[test]
fn any_engine_and_kernel_pair_is_admitted() {
    let pinned_engine = ContentDigest::of(b"engine");
    let pinned_kernel = ContentDigest::of(b"kernel");
    for (engine, kernel) in [
        (pinned_engine, pinned_kernel),
        (
            ContentDigest::of(b"dev engine"),
            ContentDigest::of(b"kernel"),
        ),
        (
            ContentDigest::of(b"dev engine"),
            ContentDigest::of(b"dev kernel"),
        ),
    ] {
        let backend =
            VmmExecutor::from_engine_digest(engine.clone(), 512, WitnessMode::Exits).unwrap();
        let inputs = PreparedInputs {
            machine_memory_mb: Default::default(),
            wall_clock_epoch: None,
            profile: backend.profile(),
            kernel: kernel.clone(),
            images: BTreeMap::new(),
            placement: BTreeMap::new(),
            guests: BTreeMap::new(),
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        assert_eq!(
            inputs.profile.engine, engine,
            "the run records the engine it used"
        );
        backend.validate_retained(&inputs).unwrap();
        let mut small = inputs.clone();
        small.profile.semantic_parameters.memory_mb = 32;
        assert!(backend.validate_retained(&small).is_err());
        for key in ["guest_abi", "control_channel", "memory_mb"] {
            let mut json = serde_json::to_value(&inputs.profile).unwrap();
            json["semantic_parameters"][key] = "wrong".into();
            assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
        }
        let report = backend
            .doctor_report(
                kernel,
                AdmissionProbe {
                    observations: vec![observed(8)],
                    result: Ok(()),
                },
            )
            .unwrap();
        let json = serde_json::to_value(report).unwrap();
        assert!(json.get("simulator_version").is_none());
        assert!(json.get("qualified").is_none());
    }
}

#[test]
fn doctor_report_preserves_exact_admitted_observations_without_cpu_binding() {
    let executor =
        VmmExecutor::from_engine_digest(ContentDigest::of(b"engine"), 1024, WitnessMode::Exits)
            .unwrap();
    let observations = vec![observed(2), observed(18)];
    let report = executor
        .doctor_report(
            ContentDigest::of(b"kernel"),
            AdmissionProbe {
                observations: observations.clone(),
                result: Ok(()),
            },
        )
        .unwrap();
    assert_eq!(report.observations, observations);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        json["observations"],
        serde_json::to_value(&observations).unwrap()
    );
    assert!(json.get("cpu").is_none());
    assert!(json.get("cpus").is_none());
    assert!(executor.admission.lock().unwrap().is_empty()); // No cached/second observation supplied the receipt.
}
#[test]
fn no_doctor_success_report_on_probe_error_or_capability_drift() {
    let executor =
        VmmExecutor::from_engine_digest(ContentDigest::of(b"engine"), 1024, WitnessMode::Exits)
            .unwrap();
    for case in 0..2 {
        let mut observation = observed(2);
        if case == 1 {
            observation.immediate_exit = 0;
        }
        let result = if case == 0 {
            Err(Error::Execution("KVM probe failed".into()))
        } else {
            Ok(())
        };
        assert!(executor
            .doctor_report(
                ContentDigest::of(b"kernel"),
                AdmissionProbe {
                    observations: vec![observation],
                    result
                }
            )
            .is_err());
    }
}
