use super::*;
use crate::admission::{required_policy, CoreObservation};
use crate::engine::{ExecutableKind, INTERNAL_WORKER_V1};
use crate::engine_adapter::{engine_command, recorded_checkpoint, recorded_features};

fn engine() -> Engine {
    Engine {
        executable: "seedbox".into(),
        revision: Some("0123abcd-dirty".into()),
    }
}

fn executor(memory_mb: u32) -> VmmExecutor {
    VmmExecutor::new(engine(), memory_mb, WitnessMode::Exits).unwrap()
}

#[test]
fn doctor_reports_engine_provenance() {
    let report = executor(512)
        .doctor_report(
            ContentDigest::of(b"kernel"),
            AdmissionProbe {
                observations: vec![observed(8)],
                result: Ok(()),
            },
        )
        .unwrap();
    assert_eq!(
        report.engine.simulator_version,
        crate::engine::SIMULATOR_VERSION
    );
    assert_eq!(report.engine.revision.as_deref(), Some("0123abcd-dirty"));
    assert_eq!(executor(512).engine(), Some(report.engine));
}

#[test]
fn resolved_profiles_run_unchanged_on_the_current_engine() {
    let profile = executor(512)
        .with_page_timer(true)
        .with_deferred_tx(true)
        .with_torn_writes(true)
        .profile;
    let bytes = serde_json::to_vec(&profile).unwrap();
    let retained: ExecutionProfile = serde_json::from_slice(&bytes).unwrap();
    let replay = VmmExecutor::for_profile(engine(), retained).unwrap();
    assert_eq!(serde_json::to_vec(&replay.profile).unwrap(), bytes);
    crate::admission::validate_profile(&replay.profile).unwrap();
    assert_eq!(
        recorded_features(&replay.profile).unwrap(),
        seedbox_engine_core::launch::MachineFeatures {
            page_timer: true,
            deferred_tx: true,
        }
    );
}

#[test]
fn worker_launch_runs_the_current_executable() {
    let command = engine_command(Path::new("seedbox"), ExecutableKind::InternalWorker);
    assert_eq!(command.get_program(), "seedbox");
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["__engine", INTERNAL_WORKER_V1]
    );
}

#[test]
fn profiles_record_no_engine_and_unknown_settings_fail() {
    let profile = executor(512).profile;
    let json = serde_json::to_value(&profile).unwrap();
    assert!(json.get("engine").is_none());
    assert!(json["semantic_parameters"].get("engine").is_none());
    for (path, value) in [
        ("engine", serde_json::json!("sha256:00")),
        ("timebase", serde_json::json!("counted-branches-v1")),
    ] {
        let mut json = json.clone();
        json["semantic_parameters"][path] = value;
        assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
    }
    let mut json = json.clone();
    json["engine"] = "sha256:00".into();
    assert!(serde_json::from_value::<ExecutionProfile>(json).is_err());
}

#[test]
fn parallel_preflight_rejects_worker_counts_outside_the_supported_range() {
    let executor = executor(512);
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
    let ordinary = executor(512);
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
fn any_kernel_is_admitted() {
    let backend = executor(512);
    for kernel in [
        ContentDigest::of(b"kernel"),
        ContentDigest::of(b"dev kernel"),
    ] {
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
        assert!(json.get("qualified").is_none());
    }
}

#[test]
fn doctor_report_preserves_exact_admitted_observations_without_cpu_binding() {
    let executor = executor(1024);
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
    let executor = executor(1024);
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
