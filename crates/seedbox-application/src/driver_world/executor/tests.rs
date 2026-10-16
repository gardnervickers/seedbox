use super::*;
fn profile() -> ExecutionProfile {
    let mut parameters = crate::execution::SemanticParameters::new(128);
    parameters.engine = Some(DriverExecutor::descriptor(DriverBackend::Tcg, "x86_64"));
    ExecutionProfile {
        backend: crate::execution::Backend::Other("driver-tcg".into()),
        engine: crate::store::ContentDigest::of(b"fixture engine"),
        witness_mode: crate::execution::WitnessMode::Exits,
        semantic_parameters: parameters,
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
fn pv_controller_scope_is_linux_x86_only_and_tcg_control_is_refused() {
    let mut pv = profile();
    pv.backend = crate::execution::Backend::Other("driver-pv".into());
    pv.semantic_parameters.engine = Some(DriverExecutor::descriptor(DriverBackend::Pv, "x86_64"));
    let pv = DriverExecutor::new(pv, DriverBackend::Pv, 1).unwrap();
    if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        pv.preflight(None).unwrap();
        pv.admit_execution(false).unwrap();
    } else {
        let error = pv.preflight(None).unwrap_err().to_string();
        assert!(
            error.contains("incomplete") && error.contains("legacy service path"),
            "{error}"
        );
        assert!(pv.admit_execution(false).is_err());
    }
    assert_eq!(
        pv.admit_execution(true).is_ok(),
        cfg!(all(target_os = "linux", target_arch = "x86_64"))
    );
    let tcg = DriverExecutor::new(profile(), DriverBackend::Tcg, 1).unwrap();
    let error = tcg.admit_execution(true).unwrap_err().to_string();
    assert!(
        error.contains("source authorization")
            && error.contains("control latency")
            && error.contains("same resolved controller"),
        "{error}"
    );
    tcg.preflight(None).unwrap();
    tcg.admit_execution(false).unwrap();
}
#[test]
fn legacy_only_toggles_on_new_contract_are_named_actionable_errors_not_backend_fallbacks() {
    for field in ["page_timer", "deferred_tx"] {
        let mut profile = profile();
        if field == "page_timer" {
            profile.semantic_parameters.page_timer = true
        } else {
            profile.semantic_parameters.deferred_tx = true
        }
        let error = DriverExecutor::new(profile, DriverBackend::Tcg, 1)
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains(&format!("semantic_parameters.{field}")));
        assert!(error.contains(&format!("set {field} to false")));
        assert!(error.contains("original retained legacy simulation"));
    }
}
#[test]
fn driver_constructor_validates_contract_and_worker_range_without_running_tools() {
    let mut legacy = profile();
    legacy.semantic_parameters.engine.as_mut().unwrap().kind =
        crate::engine::ExecutableKind::InternalWorker;
    assert!(DriverExecutor::new(legacy, DriverBackend::Tcg, 1)
        .err()
        .unwrap()
        .to_string()
        .contains("recorded driver-world-v1 contract"));
    for count in [0, 65] {
        assert!(DriverExecutor::new(profile(), DriverBackend::Tcg, count)
            .err()
            .unwrap()
            .to_string()
            .contains("1..64"));
    }
    assert_eq!(
        DriverExecutor::new(profile(), DriverBackend::Tcg, 4)
            .unwrap()
            .performance_parameters()["SEEDBOX_MACHINE_WORKERS"],
        "4"
    );
}

#[test]
fn non_oracle_legacy_controller_refuses_before_retaining_engine_or_preparing_images() {
    let root = tempfile::tempdir().unwrap();
    let store_root = root.path().join("untouched-store");
    let store = crate::store::ContentStore::new(&store_root);
    let mut simulation = crate::config::parse("name: authority\nservices:\n  app: {image: example/app}\n  other: {image: example/other}\nx-seedbox:\n  version: 1\n  check: {service: app}\n", std::path::Path::new("/project/compose.yaml"), &Default::default()).unwrap();
    simulation.control = Some(crate::config::Control {
        service: "other".into(),
    });
    let error = DriverExecutor::for_preparation(
        &root.path().join("missing-engine"),
        &store,
        &simulation,
        crate::execution::WitnessMode::Exits,
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
