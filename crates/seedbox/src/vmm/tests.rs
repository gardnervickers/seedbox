use super::*;
use crate::admission::{required_policy, CoreObservation};

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
#[test]
fn internal_world_refusal_precedes_retention_or_input_reading() {
    let temporary = tempfile::tempdir().unwrap();
    let store_path = temporary.path().join("objects");
    let store = ContentStore::new(&store_path);
    let missing = temporary.path().join("missing");
    let expected = "built-in Compose world execution requires Linux x86-64; ARM world integration is not available yet";
    let result = VmmExecutor::new_internal(&missing, &store, None, 512, WitnessMode::Exits);
    assert_eq!(result.unwrap_err().to_string(), expected);
    assert!(!store_path.exists());
    assert_eq!(
        VmmExecutor::doctor_internal(&missing, &missing, None)
            .unwrap_err()
            .to_string(),
        expected
    );
    assert!(!store_path.exists());
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn internal_doctor_reports_simulator_version() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("worker");
    std::fs::write(&executable, b"combined executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let kernel = ContentDigest::of(b"kernel");
    let executor =
        VmmExecutor::new_internal(&executable, &store, None, 512, WitnessMode::Exits).unwrap();
    let report = executor
        .doctor_report(
            kernel,
            AdmissionProbe {
                cpu: Some(8),
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
fn replay_preserves_historical_policy_data_without_current_defaults() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("retained-worker");
    std::fs::write(&executable, b"retained executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let profile = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"retained executable"),
        None,
        512,
        WitnessMode::Exits,
    )
    .unwrap()
    .profile;
    let before = serde_json::to_vec(&profile).unwrap();
    let executor = VmmExecutor::from_retained(&executable, &store, profile, None).unwrap();
    assert_eq!(serde_json::to_vec(&executor.profile).unwrap(), before);
    crate::admission::validate_profile(&executor.profile).unwrap();
    assert_eq!(
        crate::engine::recorded_launch(&executor.profile).unwrap(),
        crate::engine::ExecutableKind::Standalone
    );
}

#[test]
fn explicit_override_uses_replacement_contract_in_both_directions() {
    let _lock = crate::preparation_command::PROCESS_TEST_LOCK
        .lock()
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let combined = directory.path().join("combined");
    let standalone = directory.path().join("standalone");
    std::fs::write(
        &combined,
        b"#!/bin/sh\nprintf 'seedbox 0.2.0-replacement\\n'\n",
    )
    .unwrap();
    std::fs::write(&standalone, b"#!/bin/sh\nexit 2\n").unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    let original = VmmExecutor::new(&standalone, &store, None, 512, WitnessMode::Exits)
        .unwrap()
        .with_page_timer(true)
        .with_deferred_tx(true)
        .with_torn_writes(true);
    let combined =
        VmmExecutor::from_override(&combined, &store, original.profile.clone(), None).unwrap();
    assert_eq!(
        combined
            .profile
            .semantic_parameters
            .engine
            .as_ref()
            .unwrap()
            .simulator_version,
        "0.2.0-replacement"
    );
    assert_eq!(
        engine_command(Path::new("engine"), &combined.profile)
            .unwrap()
            .get_args()
            .collect::<Vec<_>>(),
        ["__engine", crate::engine::INTERNAL_WORKER_V1]
    );
    assert_eq!(
        recorded_features(&combined.profile).unwrap(),
        seedbox_engine_core::launch::MachineFeatures {
            page_timer: true,
            deferred_tx: true,
        }
    );
    assert!(combined.profile.semantic_parameters.torn_writes);
    let standalone =
        VmmExecutor::from_override(&standalone, &store, combined.profile, None).unwrap();
    assert_eq!(standalone.profile, original.profile);
    assert_eq!(
        engine_command(Path::new("engine"), &standalone.profile)
            .unwrap()
            .get_args()
            .count(),
        0
    );
    // Explicit fresh runs perform the same detection; retained replay does not.
    let detected = VmmExecutor::new_detected(
        &directory.path().join("combined"),
        &store,
        None,
        512,
        WitnessMode::Exits,
    )
    .unwrap();
    assert_eq!(
        crate::engine::recorded_launch(&detected.profile).unwrap(),
        crate::engine::ExecutableKind::InternalWorker
    );
}

#[test]
fn retained_worker_launch_preserves_timer_and_deferred_tx_options() {
    let executor = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"engine"),
        None,
        512,
        WitnessMode::Exits,
    )
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
fn internal_profile_retains_identity_and_replays_through_worker_contract() {
    use crate::engine::{recorded_launch, ExecutableKind, INTERNAL_WORKER_V1};
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("retained-worker");
    std::fs::write(&executable, b"combined executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    let executor =
        VmmExecutor::new_internal(&executable, &store, None, 512, WitnessMode::Exits).unwrap();
    // A retained Linux profile is inspectable on a Mac without host admission.
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    let executor = {
        let mut executor =
            VmmExecutor::new(&executable, &store, None, 512, WitnessMode::Exits).unwrap();
        executor.profile.semantic_parameters.engine = Some(crate::engine::kvm_descriptor(
            crate::engine::ExecutableKind::InternalWorker,
            crate::engine::SIMULATOR_VERSION.into(),
        ));
        executor
    };
    assert_eq!(
        recorded_launch(&executor.profile).unwrap(),
        ExecutableKind::InternalWorker
    );
    let bytes = serde_json::to_vec(&executor.profile).unwrap();
    let retained: ExecutionProfile = serde_json::from_slice(&bytes).unwrap();
    let replay = VmmExecutor::from_retained(&executable, &store, retained, Some(8)).unwrap();
    assert_eq!(serde_json::to_vec(&replay.profile).unwrap(), bytes);
    for profile in [&executor.profile, &replay.profile] {
        let command = engine_command(&executable, profile).unwrap();
        assert_eq!(command.get_program(), executable.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["__engine", INTERNAL_WORKER_V1]
        );
    }
    // Historical standalone profiles keep their launch contract.
    let legacy = VmmExecutor::new(&executable, &store, None, 512, WitnessMode::Exits).unwrap();
    assert_eq!(
        recorded_launch(&legacy.profile).unwrap(),
        ExecutableKind::Standalone
    );
}

#[test]
fn recorded_descriptor_is_data_and_unknown_settings_fail() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("old-worker");
    std::fs::write(&executable, b"retained worker").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let mut profile = VmmExecutor::new(&executable, &store, None, 512, WitnessMode::Exits)
        .unwrap()
        .profile;
    profile.semantic_parameters.engine = Some(crate::engine::kvm_descriptor(
        crate::engine::ExecutableKind::InternalWorker,
        "0.1.0-dev.1".into(),
    ));
    let original = serde_json::to_vec(&profile).unwrap();
    let executor = VmmExecutor::from_retained(&executable, &store, profile.clone(), None).unwrap();
    assert_eq!(serde_json::to_vec(&executor.profile).unwrap(), original);
    assert_eq!(
        crate::engine::recorded_launch(&executor.profile).unwrap(),
        crate::engine::ExecutableKind::InternalWorker
    );
    let mut json = serde_json::to_value(&profile).unwrap();
    json["semantic_parameters"]["timebase"] = "counted-branches-v1".into();
    assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
}

#[test]
fn replay_override_changes_only_retained_executable_digest() {
    let temporary = tempfile::tempdir().unwrap();
    let executable = temporary.path().join("replacement-worker");
    std::fs::write(&executable, b"replacement executable").unwrap();
    let store = ContentStore::new(temporary.path().join("objects"));
    let profile = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"original executable"),
        None,
        512,
        WitnessMode::Exits,
    )
    .unwrap()
    .profile;
    let mut expected = profile.clone();
    expected.engine = ContentDigest::of(b"replacement executable");
    let executor = VmmExecutor::from_retained(&executable, &store, profile, None).unwrap();
    assert_eq!(executor.profile, expected);
}

#[test]
fn capacity_admission_holds_all_reservations_and_releases_them_on_every_exit() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Reservation<'a>(&'a AtomicUsize);
    impl Drop for Reservation<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    for (available, refuse) in [(3, false), (2, false), (3, true)] {
        // An unrelated owner must survive both capacity and admission errors.
        let held = AtomicUsize::new(1);
        let mut checked = false;
        let result = with_capacity(
            3,
            || {
                if held.load(Ordering::SeqCst) > available {
                    return Err(Error::Setup("fixture capacity unavailable".into()));
                }
                held.fetch_add(1, Ordering::SeqCst);
                Ok(Reservation(&held))
            },
            |reservations| {
                checked = true;
                assert_eq!(reservations.len(), 3);
                assert_eq!(held.load(Ordering::SeqCst), 4);
                if refuse {
                    Err(Error::Setup("fixture core admission refused".into()))
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(held.load(Ordering::SeqCst), 1);
        assert_eq!(checked, available == 3);
        assert_eq!(result.is_ok(), available == 3 && !refuse);
        if available == 2 {
            assert!(result
                .unwrap_err()
                .to_string()
                .contains("--workers 3: cannot reserve 3 distinct eligible physical cores"));
        }
    }
}

#[test]
fn parallel_preflight_preserves_the_experimental_admission_gate() {
    let pinned = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"engine"),
        None,
        512,
        WitnessMode::Exits,
    )
    .unwrap();
    for workers in [0, 33] {
        assert!(pinned
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
    let ordinary = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"engine"),
        None,
        512,
        WitnessMode::Exits,
    )
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
            VmmExecutor::from_engine_digest(engine.clone(), None, 512, WitnessMode::Exits).unwrap();
        let inputs = PreparedInputs {
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
                    cpu: Some(8),
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
fn doctor_report_preserves_actual_selected_core_and_exact_admitted_observations() {
    let executor = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"engine"),
        None,
        1024,
        WitnessMode::Exits,
    )
    .unwrap();
    let observations = vec![observed(2), observed(18)];
    let report = executor
        .doctor_report(
            ContentDigest::of(b"kernel"),
            AdmissionProbe {
                cpu: Some(18),
                observations: observations.clone(),
                result: Ok(()),
            },
        )
        .unwrap();
    assert_eq!(report.cpu, 18); // Selected CPU need not be the first sorted sibling.
    assert_eq!(report.cpus, [2, 18]);
    assert_eq!(report.observations, observations);
    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(
        json["observations"],
        serde_json::to_value(&observations).unwrap()
    );
    assert_eq!(json["cpu"], 18);
    assert!(executor.admission.lock().unwrap().is_empty()); // No cached/second observation supplied the receipt.
}
#[test]
fn no_doctor_success_report_on_probe_cleanup_error_or_capability_drift() {
    let executor = VmmExecutor::from_engine_digest(
        ContentDigest::of(b"engine"),
        Some(2),
        1024,
        WitnessMode::Exits,
    )
    .unwrap();
    for case in 0..2 {
        let mut observation = observed(2);
        if case == 1 {
            observation.immediate_exit = 0;
        }
        let result = if case == 0 {
            Err(Error::Execution(
                "host preflight cleanup failed: affinity readback mismatch".into(),
            ))
        } else {
            Ok(())
        };
        assert!(executor
            .doctor_report(
                ContentDigest::of(b"kernel"),
                AdmissionProbe {
                    cpu: Some(2),
                    observations: vec![observation],
                    result
                }
            )
            .is_err());
    }
}
