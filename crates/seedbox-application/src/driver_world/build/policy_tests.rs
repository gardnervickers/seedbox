//! Pure admission/metadata checks; never construct or execute native machines.
use super::*;
fn inventory(policy: Option<wire::BackendPolicy>) -> Inventory {
    let artifact = Artifact {
        sha256: "00".repeat(32),
        length: 0,
        artifact: "/nonexistent-policy-test-input".into(),
    };
    Inventory::Tcg {
        policy,
        executable: artifact.clone(),
        kernel: artifact.clone(),
        init: artifact.clone(),
        command_line: artifact.clone(),
        firmware: artifact.clone(),
        runtime: artifact,
        formatter: None,
        environments: Default::default(),
        boot_archives: Default::default(),
        epoch_seconds: 86400,
    }
}
fn request(inventory: Inventory, output: PathBuf) -> wire::Request {
    let artifact = inventory.artifacts()[0].clone();
    wire::Request {
        prepared: wire::Prepared {
            inventory,
            oracle: wire::Machine {
                name: "oracle".into(),
                memory_bytes: 256 << 20,
                filesystem: artifact,
                process: wire::Process {
                    argv: vec!["/program".into()],
                    environment: Default::default(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
                disk: None,
                interface: None,
            },
            machines: vec![],
            spawnable: vec![],
            quantum_ns: 1,
            network: Default::default(),
        },
        runtime_binding: None,
        seed: 0,
        program: vec![],
        simulated_time_ns: 0,
        host_control: output.join("control"),
        output,
        machine_workers: 1,
        start_paused: false,
        full_state: false,
        observations: None,
        replay_interventions: None,
        resume_checkpoint: None,
    }
}
#[test]
fn native_invalid_wire_policy_is_rejected_before_materialization_and_restore_binding() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("must-not-exist");
    let invalid = inventory(Some(wire::BackendPolicy {
        timer_jitter_ns: 3_600_000_000_001,
        ..Default::default()
    }));
    let valid = inventory(Some(wire::BackendPolicy::default()));
    let error = materialize_factory(&invalid, &output, None)
        .err()
        .expect("invalid native policy must precede artifact reads")
        .to_string();
    assert!(error.contains("timer_jitter_ns"), "{error}");
    for (expected, captured) in [(&invalid, &valid), (&valid, &invalid), (&invalid, &invalid)] {
        let error = validate_policy_binding(expected, captured)
            .unwrap_err()
            .to_string();
        assert!(error.contains("timer_jitter_ns"), "{error}");
    }
    assert!(!output.exists());
    let none = inventory(None);
    assert!(validate_policy_binding(&none, &valid).is_err());
    assert!(validate_policy_binding(&valid, &none).is_err());
    validate_policy_binding(&none, &none).unwrap();
    validate_policy_binding(&valid, &valid).unwrap();
}
#[test]
fn unfinished_pv_and_tcg_controller_requests_fail_closed_before_effects() {
    let root = tempfile::tempdir().unwrap();
    let tcg = inventory(Some(wire::BackendPolicy {
        controller: Some("oracle".into()),
        ..Default::default()
    }));
    let mut pv = serde_json::to_value(tcg.clone()).unwrap();
    pv["backend"] = "pv".into();
    pv["progress"] = pv["kernel"].clone();
    pv["blocks_per_progress_call"] = 1.into();
    pv.as_object_mut().unwrap().remove("firmware");
    let pv: Inventory = serde_json::from_value(pv).unwrap();
    for (index, inventory) in [pv, tcg].into_iter().enumerate() {
        let output = root.path().join(index.to_string());
        let request = request(inventory, output.clone());
        let error = open(&request)
            .err()
            .expect("unfinished candidate must not execute")
            .to_string();
        assert!(
            error.contains("incomplete") && error.contains("legacy service path"),
            "{error}"
        );
        assert!(
            error.contains("authorization")
                && error.contains("latency")
                && error.contains("same resolved controller"),
            "{error}"
        );
        assert!(
            !output.exists(),
            "refusal must precede materialization, scratch files or native construction"
        );
    }
}
