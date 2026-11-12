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
                allow_crashes: false,
            },
            machines: vec![],
            spawned: vec![],
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
    let error = materialize_factory(&invalid, None, &output, None)
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
fn controller_must_be_the_dedicated_oracle_before_any_host_effects() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("must-not-exist");
    let inventory = inventory(Some(wire::BackendPolicy {
        controller: Some("retained-worker".into()),
        ..Default::default()
    }));
    let mut request = request(inventory, output.clone());
    let mut peer = request.prepared.oracle.clone();
    peer.name = "retained-worker".into();
    request.prepared.machines.push(peer);
    let error = open(&request)
        .err()
        .expect("a retained peer is not the oracle")
        .to_string();
    assert!(error.contains("must name the retained oracle"), "{error}");
    assert!(!output.exists());
}
#[test]
fn tcg_controller_requests_are_admitted_only_on_driver_hosts() {
    let root = tempfile::tempdir().unwrap();
    let tcg = inventory(Some(wire::BackendPolicy {
        controller: Some("oracle".into()),
        ..Default::default()
    }));
    let output = root.path().join("output");
    let error = open(&request(tcg.clone(), output.clone()))
        .err()
        .expect("the policy-test inputs are not runnable")
        .to_string();
    if crate::driver_world::platform::Host::current().runs_driver() {
        // Admitted: the request reaches materialization and fails only on its
        // placeholder inputs, never on the controller gate.
        crate::driver_world::policy::admit(&tcg).unwrap();
        crate::driver_world::policy::admit_restore(&tcg).unwrap();
        assert!(
            !error.contains("incomplete") && !error.contains("requires Linux x86-64"),
            "{error}"
        );
    } else {
        assert!(
            error.contains("requires Linux x86-64 (KVM) or macOS aarch64 (HVF)"),
            "{error}"
        );
        assert!(
            !output.exists(),
            "refusal must precede materialization, scratch files or native construction"
        );
    }
}

struct Mock;
impl seedbox_machine::machine::Machine for Mock {
    type Context = ();
    type Error = String;
    type Snapshot = ();
    fn run(
        &mut self,
        _: &mut seedbox_machine::machine::RunContext<'_, ()>,
        _: Timestamp,
    ) -> std::result::Result<seedbox_machine::machine::StopReason, String> {
        Ok(seedbox_machine::machine::StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<(), String> {
        Ok(())
    }
}
/// A one-millisecond-quantum capture of a mock oracle, after `configure`.
fn saved_checkpoint(
    configure: impl FnOnce(&mut Driver<Mock>),
) -> seedbox_driver::driver::Checkpoint<seedbox_driver::driver::SnapshotReference> {
    use seedbox_machine::definition::{Launch, PreparedApplication, Resources};
    let oracle = MachineDefinition {
        name: "oracle".into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar::Builder::new(Vec::new()).into_inner().unwrap())
                .unwrap(),
            launch: Launch {
                argv: vec!["/application".into()],
                environment: BTreeMap::new(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    };
    let mut source = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(1_000_000),
        1,
        0,
        || Ok(()),
        oracle,
        vec![],
        vec![],
        NetworkConfig::default(),
        |_| Ok(Mock),
    )
    .unwrap();
    configure(&mut source);
    source
        .checkpoint()
        .unwrap()
        .try_map_snapshots(|()| {
            Ok::<_, String>(seedbox_driver::driver::SnapshotReference::new([0; 32], 1))
        })
        .unwrap()
}
#[test]
fn checkpoint_quantum_must_match_without_changing_saved_state() {
    let saved = saved_checkpoint(|_| {});
    let mut before = Vec::new();
    saved.write_to(&mut before).unwrap();
    validate_checkpoint_quantum(&saved, 1_000_000).unwrap();
    let error = validate_checkpoint_quantum(&saved, 2_000_000)
        .unwrap_err()
        .to_string();
    assert!(error.contains("checkpoint quantum differs"), "{error}");
    assert_eq!(saved.quantum(), Duration::from_nanos(1_000_000));
    let mut after = Vec::new();
    saved.write_to(&mut after).unwrap();
    assert_eq!(before, after);
    validate_checkpoint_quantum(&saved, 1_000_000).unwrap();
}
#[test]
fn checkpoint_allow_crashes_must_match_the_requested_machines() {
    let root = tempfile::tempdir().unwrap();
    let mut request = request(inventory(None), root.path().join("must-not-exist"));
    let allowed = saved_checkpoint(|driver| {
        driver.allow_crashes(["oracle".into()].into()).unwrap();
    });
    let plain = saved_checkpoint(|_| {});
    validate_checkpoint_allow_crashes(&plain, &request.prepared).unwrap();
    let error = validate_checkpoint_allow_crashes(&allowed, &request.prepared).unwrap_err();
    assert!(matches!(error, Error::Setup(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "allowCrashes for oracle differs from the checkpoint; start fresh to change allowCrashes"
    );
    request.prepared.oracle.allow_crashes = true;
    validate_checkpoint_allow_crashes(&allowed, &request.prepared).unwrap();
    assert!(validate_checkpoint_allow_crashes(&plain, &request.prepared).is_err());
}
#[test]
fn programs_beyond_the_engine_are_refused_before_any_machine_boots() {
    let root = tempfile::tempdir().unwrap();
    // The fixture inventory is TCG, which emulates x86_64.
    let mut request = request(inventory(None), root.path().join("must-not-exist"));
    assert_eq!(
        super::super::actions::engine(&request.prepared.inventory),
        seedbox_driver::driver::faults::Engine {
            backend: seedbox_spec::backend::CpuBackend::Tcg,
            architecture: seedbox_spec::definition::Architecture::X86_64,
        }
    );
    request.simulated_time_ns = 100;
    request.program = vec![serde_json::from_value(serde_json::json!({
        "key": "slow", "at_ns": 10,
        "action": {"action": "disk_latency", "handle": "slow", "machine": "oracle", "request": "any", "latency_ns": 1000},
    }))
    .unwrap()];
    assert_eq!(
        program(&request).unwrap_err().to_string(),
        "input slow: disk_latency needs delayed block completion, which x86_64 TCG does not provide"
    );
}
#[test]
fn injected_faults_meet_the_engine_check_before_compilation() {
    let preempt: seedbox_engine_protocol::control::Action = serde_json::from_value(
        serde_json::json!({"action": "preempt", "handle": "p", "machine": "oracle", "ppm": 10}),
    )
    .unwrap();
    let error = super::super::actions::translate(&inventory(None), &preempt, 10).unwrap_err();
    assert_eq!(
        error,
        "preempt needs the guest yield hook, which x86_64 TCG does not provide"
    );
}
