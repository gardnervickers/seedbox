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

/// A mock machine that emits its one pending response, if any, when it first runs.
#[derive(Default)]
struct Mock(Option<seedbox_machine::pv::Response>);
impl seedbox_machine::machine::Machine for Mock {
    type Context = ();
    type Error = String;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut seedbox_machine::machine::RunContext<'_, ()>,
        _: Timestamp,
    ) -> std::result::Result<seedbox_machine::machine::StopReason, String> {
        if let Some(payload) = self.0.take() {
            let at = Timestamp::from_nanos(context.clock.now());
            context
                .outbox
                .emit(seedbox_machine::pv::Output { at, payload })
                .map_err(|error| format!("{error:?}"))?;
        }
        Ok(seedbox_machine::machine::StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<(), String> {
        Ok(())
    }
}
fn mock_definition(name: &str) -> MachineDefinition {
    use seedbox_machine::definition::{Launch, PreparedApplication, Resources};
    MachineDefinition {
        name: name.into(),
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
    }
}
/// A one-millisecond-quantum capture of a mock oracle, after `configure`.
fn saved_checkpoint(
    configure: impl FnOnce(&mut Driver<Mock>),
) -> seedbox_driver::driver::Checkpoint<seedbox_driver::driver::SnapshotReference> {
    let mut source = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(1_000_000),
        1,
        0,
        || Ok(()),
        mock_definition("oracle"),
        vec![],
        vec![],
        NetworkConfig::default(),
        |_| Ok(Mock::default()),
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
/// Network latency of [`published_checkpoint`]'s world, the default.
const LATENCY_NS: u64 = 1_000_000;
/// A published checkpoint of a mock controller oracle that allowed `allowed`
/// initial crashes and spawned `worker`, whose definition allows crashes. Its
/// inventory's files live under `root`.
fn published_checkpoint(root: &Path, allowed: &[&str]) -> (Inventory, PathBuf) {
    use sha2::Digest;
    let inputs = root.join("inputs");
    fs::create_dir_all(&inputs).unwrap();
    let artifact = |name: &str| {
        let path = inputs.join(name);
        let bytes = format!("placeholder {name}").into_bytes();
        fs::write(&path, &bytes).unwrap();
        Artifact {
            sha256: hex::encode(sha2::Sha256::digest(&bytes)),
            length: bytes.len() as u64,
            artifact: path.to_str().unwrap().into(),
        }
    };
    let inventory = Inventory::Tcg {
        policy: Some(wire::BackendPolicy {
            controller: Some("oracle".into()),
            ..Default::default()
        }),
        executable: artifact("executable"),
        kernel: artifact("kernel"),
        init: artifact("init"),
        command_line: artifact("command-line"),
        firmware: artifact("firmware"),
        runtime: artifact("runtime"),
        formatter: None,
        environments: Default::default(),
        boot_archives: Default::default(),
        epoch_seconds: 86400,
    };
    let spawn = seedbox_machine::pv::Response::SimulationControl {
        id: seedbox_machine::pv::SimulationControlId {
            session: std::num::NonZeroU32::MIN,
            request: std::num::NonZeroU32::MIN,
        },
        payload: br#"{"version":1,"id":1,"op":"spawn","machine":"worker","spec":{}}"#.to_vec(),
    };
    let mut oracle = Some(spawn);
    let mut driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(1_000_000),
        1,
        0,
        || Ok(()),
        mock_definition("oracle"),
        vec![],
        vec![seedbox_driver::driver::SpawnDefinition {
            spec: "{}".into(),
            definition: mock_definition("worker"),
            allow_crashes: true,
        }],
        super::super::controller::resolve(&inventory, "oracle", LATENCY_NS)
            .unwrap()
            .configure(NetworkConfig::default().into())
            .unwrap(),
        move |_| Ok(Mock(oracle.take())),
    )
    .unwrap();
    driver
        .allow_crashes(allowed.iter().map(|name| name.to_string()).collect())
        .unwrap();
    let mut state = State {
        world: wire::World {
            seed: 0,
            machine_names: vec!["oracle".into(), "worker".into()],
            input_program: Default::default(),
            intervention_next: 0,
            readiness: wire::Readiness {
                initial: vec!["oracle".into()],
                seen: vec![],
                latched: false,
            },
            publication: Default::default(),
            position: Default::default(),
        },
    };
    // Step until the spawn's admission adds `worker`, a positive Driver event.
    let mut spawned = false;
    for quantum in 1..=20 {
        driver
            .step(Timestamp::from_nanos(quantum * 1_000_000))
            .unwrap();
        let events = driver.drain_observations().events;
        state.world.publication.session.next_driver_sequence += events.len() as u64;
        spawned |= events.iter().any(|event| {
            matches!(
                event.kind,
                seedbox_driver::driver::EventKind::MachineAdded {
                    cause: seedbox_driver::driver::Cause::Fault(_),
                    ..
                }
            )
        });
        if spawned {
            break;
        }
    }
    assert!(spawned, "the oracle's spawn of worker was not admitted");
    let output = root.join("checkpoints");
    fs::create_dir_all(&output).unwrap();
    let manifest = checkpoint::capture(
        &mut driver,
        checkpoint::Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "allow-crashes",
        |state, batch| {
            state.world.publication.session.next_driver_sequence += batch.events.len() as u64;
            Ok(())
        },
        |(), _| Ok(()),
    )
    .unwrap();
    (inventory, manifest)
}
#[test]
fn restore_refuses_edited_initial_allow_crashes_before_materializing_anything() {
    let root = tempfile::tempdir().unwrap();
    for (allowed, oracle_allowed, refused) in [
        (&["oracle"][..], false, true),
        (&[][..], true, true),
        (&["oracle"][..], true, false),
        (&[][..], false, false),
    ] {
        let case = root
            .path()
            .join(format!("{}-{oracle_allowed}", allowed.len()));
        let (inventory, manifest) = published_checkpoint(&case, allowed);
        let envelope = checkpoint::load(&manifest).unwrap();
        let saved =
            checkpoint::open(&envelope, driver_limits(envelope.driver.length).unwrap()).unwrap();
        // The spawned machine's own allowance is captured beside the initial set.
        let mut expected: std::collections::BTreeSet<String> =
            allowed.iter().map(|name| name.to_string()).collect();
        expected.insert("worker".into());
        assert_eq!(saved.allowed_crashes(), &expected);
        let output = case.join("restored");
        let mut request = request(inventory, output.clone());
        request.prepared.quantum_ns = 1_000_000;
        request.prepared.network.latency_ns = LATENCY_NS;
        request.prepared.oracle.allow_crashes = oracle_allowed;
        let error = restore(&request, &manifest)
            .err()
            .expect("placeholder inputs cannot restore")
            .to_string();
        let gate = "allowCrashes for oracle differs from the checkpoint; start fresh to change allowCrashes";
        // A matching set passes the gate and reaches runtime materialization,
        // where the placeholder runtime archive fails; a refusal precedes it.
        if refused {
            assert_eq!(error, gate);
        } else {
            assert!(
                error.starts_with("driver runtime ") && error.contains("build-assets"),
                "{error}"
            );
        }
        assert!(!output.exists(), "{error}");
    }
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
