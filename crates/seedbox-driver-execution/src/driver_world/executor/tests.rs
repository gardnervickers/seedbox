use super::*;
fn profile() -> ExecutionProfile {
    let mut parameters = crate::execution::SemanticParameters::new();
    parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
    ExecutionProfile {
        backend: "driver-tcg".into(),
        witness_mode: crate::execution::WitnessMode::Exits,
        semantic_parameters: parameters,
    }
}
#[test]
fn backend_authority_rejects_conflicting_constructor_backend_before_worker_launch() {
    for (backend, tag, other) in [
        (CpuBackend::Pv, "driver-pv", "driver-tcg"),
        (CpuBackend::Tcg, "driver-tcg", "driver-pv"),
    ] {
        let mut matching = profile();
        matching.backend = tag.into();
        matching.semantic_parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
        DriverExecutor::new(matching.clone(), backend, 1).unwrap();
        matching.backend = other.into();
        let error = DriverExecutor::new(matching, backend, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("backend argument differs from validated execution profile"),
            "{error}"
        );
    }
}

#[test]
fn backend_authority_requires_contract_and_matching_constructor_argument() {
    for (backend, opposite, tag) in [
        (CpuBackend::Pv, CpuBackend::Tcg, "driver-pv"),
        (CpuBackend::Tcg, CpuBackend::Pv, "driver-tcg"),
    ] {
        let mut matching = profile();
        matching.backend = tag.into();
        matching.semantic_parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
        DriverExecutor::new(matching.clone(), backend, 1).unwrap();
        let mut legacy = matching.clone();
        legacy.semantic_parameters.engine = None;
        let error = DriverExecutor::new(legacy, opposite, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("recorded driver-world-v1 contract"),
            "{error}"
        );
        // Change the independent argument,
        // not a profile label: it must not choose a different actual executor.
        let error = DriverExecutor::new(matching, opposite, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains("backend argument differs from validated execution profile"),
            "{error}"
        );
    }
}

#[test]
fn missing_witness_is_an_error_without_erasing_the_actual_oracle_verdict_or_observations() {
    let mut report = incomplete(StopReason::Completed);
    report.simulated_time_ns = Some(73);
    report.workload_exit = Some(crate::execution::WorkloadExit {
        service: "oracle".into(),
        code: 19,
    });
    report.faults.push(crate::execution::FaultEvent {
        requested_at_ns: 3,
        applied_at_ns: 5,
        service: "worker".into(),
        action: "crash".into(),
        detail: "acknowledged".into(),
    });
    report.detail = Some("physical oracle exited without AppOutcome".into());
    let result = decode_report(&serde_json::to_vec(&report).unwrap()).unwrap();
    assert_eq!(result.reason, StopReason::Error);
    assert_eq!(result.workload_exit, report.workload_exit);
    assert_eq!(result.simulated_time_ns, report.simulated_time_ns);
    assert_eq!(result.faults, report.faults);
    assert_eq!(result.witness, None);
    assert_eq!(result.full_state, None);
    assert_eq!(result.detail.as_deref(), Some("physical oracle exited without AppOutcome; retained Driver worker omitted its required native execution witness"));
}
#[test]
fn native_capture_error_is_preserved_without_replacing_it_with_missing_witness() {
    let mut report = incomplete(StopReason::Error);
    report.detail = Some("native CPU evidence query failed".into());
    assert_eq!(
        decode_report(&serde_json::to_vec(&report).unwrap()).unwrap(),
        report
    );
    let mut success = incomplete(StopReason::Completed);
    success.detail = None;
    let decoded = decode_report(&serde_json::to_vec(&success).unwrap()).unwrap();
    assert_eq!(decoded.reason, StopReason::Error);
    assert_eq!(
        decoded.detail.as_deref(),
        Some("retained Driver worker omitted its required native execution witness")
    );
}
#[test]
fn malformed_report_or_digest_is_not_accepted_as_native_evidence() {
    assert!(decode_report(b"not JSON").is_err());
    let mut report = serde_json::to_value(incomplete(StopReason::Completed)).unwrap();
    report["witness"] = "not-a-sha256".into();
    assert!(decode_report(&serde_json::to_vec(&report).unwrap()).is_err());
    report["witness"] = serde_json::Value::Null;
    report["unknown_native_state"] = true.into();
    assert!(decode_report(&serde_json::to_vec(&report).unwrap()).is_err());
}
#[test]
fn controller_scope_follows_driver_host_for_pv_and_tcg() {
    let mut pv = profile();
    pv.backend = "driver-pv".into();
    pv.semantic_parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
    let pv = DriverExecutor::new(pv, CpuBackend::Pv, 1).unwrap();
    if crate::driver_world::platform::Host::current().runs_driver() {
        pv.preflight(None).unwrap();
        pv.admit_execution(false).unwrap();
    } else {
        let error = pv.preflight(None).unwrap_err().to_string();
        assert!(
            error.contains("requires Linux x86-64 (KVM) or macOS aarch64 (HVF)")
                && error.contains("legacy service path"),
            "{error}"
        );
        assert!(pv.admit_execution(false).is_err());
    }
    assert_eq!(
        pv.admit_execution(true).is_ok(),
        crate::driver_world::platform::Host::current().runs_driver()
    );
    let tcg = DriverExecutor::new(profile(), CpuBackend::Tcg, 1).unwrap();
    if crate::driver_world::platform::Host::current().runs_driver() {
        tcg.admit_execution(true).unwrap();
    } else {
        let error = tcg.admit_execution(true).unwrap_err().to_string();
        assert!(
            error.contains("requires Linux x86-64 (KVM) or macOS aarch64 (HVF)"),
            "{error}"
        );
    }
    tcg.preflight(None).unwrap();
    tcg.admit_execution(false).unwrap();
}

#[test]
fn driver_constructor_validates_contract_and_worker_range_without_running_tools() {
    let mut legacy = profile();
    legacy.semantic_parameters.engine = None;
    assert!(DriverExecutor::new(legacy, CpuBackend::Tcg, 1)
        .err()
        .unwrap()
        .to_string()
        .contains("recorded driver-world-v1 contract"));
    for count in [0, 65] {
        assert!(DriverExecutor::new(profile(), CpuBackend::Tcg, count)
            .err()
            .unwrap()
            .to_string()
            .contains("1..64"));
    }
    assert_eq!(
        DriverExecutor::new(profile(), CpuBackend::Tcg, 4)
            .unwrap()
            .performance_parameters()["SEEDBOX_MACHINE_WORKERS"],
        "4"
    );
}

#[test]
fn non_oracle_legacy_controller_refuses_before_retaining_engine_or_preparing_images() {
    let root = tempfile::tempdir().unwrap();
    let store_root = root.path().join("untouched-store");
    let mut simulation = crate::fixture_simulation("app");
    simulation.control = Some(crate::config::Control {
        service: "other".into(),
    });
    let error = DriverExecutor::for_preparation(
        &simulation,
        crate::execution::WitnessMode::Exits,
        Default::default(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        error.contains("controller must also be the check service"),
        "{error}"
    );
    assert!(!store_root.exists());
}

#[test]
fn retained_profile_drift_is_refused_before_reading_runtime_or_launching_workers() {
    let executor = DriverExecutor::new(profile(), CpuBackend::Tcg, 1).unwrap();
    for change in [0, 1] {
        let mut recorded = profile();
        if change == 0 {
            recorded.witness_mode = crate::execution::WitnessMode::FullState;
        } else {
            recorded.semantic_parameters.torn_writes = true;
        }
        let inputs = PreparedInputs {
            machine_memory_mb: Default::default(),
            wall_clock_epoch: None,
            images: Default::default(),
            placement: Default::default(),
            guests: Default::default(),
            profile: recorded,
            auxiliary: Default::default(),
            provenance: Default::default(),
        };
        let error = executor.validate_retained(&inputs).unwrap_err().to_string();
        assert!(
            error.contains("retained Driver profile differs from executor"),
            "{error}"
        );
    }
}
