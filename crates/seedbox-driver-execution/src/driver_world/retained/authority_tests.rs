//! Metadata/launch-adaptation fixtures only: no executable or guest is started.
use super::*;
use crate::{
    driver_world::DriverExecutor,
    execution::{CancellationToken, ExecutionProfile, SemanticParameters, WitnessMode},
};

fn profile(backend: CpuBackend) -> ExecutionProfile {
    let mut parameters = SemanticParameters::new();
    parameters.engine = Some(DriverExecutor::descriptor("x86_64"));
    ExecutionProfile {
        backend: backend.recorded_name().into(),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: parameters,
    }
}
fn inventory(backend: CpuBackend, artifact: &Artifact) -> wire::Inventory {
    match backend {
        CpuBackend::Pv => wire::Inventory::Pv {
            policy: None,
            executable: artifact.clone(),
            kernel: artifact.clone(),
            init: artifact.clone(),
            command_line: artifact.clone(),
            progress: artifact.clone(),
            runtime: artifact.clone(),
            formatter: None,
            environments: Default::default(),
            boot_archives: Default::default(),
            progress_budget: Default::default(),
            epoch_seconds: 86400,
        },
        CpuBackend::Tcg => wire::Inventory::Tcg {
            policy: None,
            executable: artifact.clone(),
            kernel: artifact.clone(),
            init: artifact.clone(),
            command_line: artifact.clone(),
            firmware: artifact.clone(),
            runtime: artifact.clone(),
            formatter: None,
            environments: Default::default(),
            boot_archives: Default::default(),
            epoch_seconds: 86400,
        },
    }
}
fn fixture(root: &Path, backend: CpuBackend) -> (PreparedInputs, Prepared, Artifact) {
    fixture_bytes(root, backend, b"metadata fixture")
}
fn fixture_bytes(
    root: &Path,
    backend: CpuBackend,
    bytes: &[u8],
) -> (PreparedInputs, Prepared, Artifact) {
    let source = root.join("source");
    fs::write(&source, bytes).unwrap();
    let digest = ContentDigest::of(bytes);
    let artifact = Artifact {
        sha256: digest.hex().into(),
        length: bytes.len() as u64,
        artifact: source.to_str().unwrap().into(),
    };
    let inputs = PreparedInputs {
        machine_memory_mb: BTreeMap::from([("oracle".into(), 128)]),
        wall_clock_epoch: None,
        profile: profile(backend),
        images: Default::default(),
        placement: Default::default(),
        guests: Default::default(),
        auxiliary: BTreeMap::from([("driver-world/executable".into(), digest)]),
        provenance: Default::default(),
    };
    let prepared = Prepared {
        inventory: inventory(backend, &artifact),
        oracle: wire::Machine {
            name: "oracle".into(),
            memory_bytes: 128 << 20,
            filesystem: artifact.clone(),
            process: wire::Process {
                argv: vec!["/fixture".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "".into(),
            },
            disk: None,
            interface: None,
            allow_crashes: false,
        },
        machines: vec![],
        spawned: vec![],
        quantum_ns: 1000,
        network: wire::Network {
            latency_ns: 1000,
            ..Default::default()
        },
    };
    (inputs, prepared, artifact)
}
fn pairs() -> [(CpuBackend, CpuBackend); 2] {
    [
        (CpuBackend::Pv, CpuBackend::Tcg),
        (CpuBackend::Tcg, CpuBackend::Pv),
    ]
}
fn mismatch(error: crate::Error) {
    let error = error.to_string();
    assert!(
        error.contains("inventory backend differs from validated execution profile"),
        "{error}"
    );
}

#[test]
fn backend_authority_checks_actual_inventory_before_retaining_artifacts() {
    for (backend, opposite) in pairs() {
        let root = tempfile::tempdir().unwrap();
        let store_path = root.path().join("untouched-store");
        let store = ContentStore::new(&store_path);
        let (mut inputs, mut prepared, artifact) = fixture(root.path(), backend);
        validate_inventory(&inputs.profile, &prepared.inventory).unwrap();
        prepared.inventory = inventory(opposite, &artifact);
        let original = inputs.auxiliary.clone();
        mismatch(retain_prepared(&mut inputs, &prepared, &store).unwrap_err());
        assert!(!store_path.exists(), "mismatch must precede retention");
        assert_eq!(inputs.auxiliary, original);
    }
}

#[test]
fn backend_authority_checks_saved_inventory_in_runtime_and_both_launch_paths() {
    for (backend, opposite) in pairs() {
        let root = tempfile::tempdir().unwrap();
        let store = ContentStore::new(root.path().join("store"));
        let (mut inputs, mut prepared, artifact) = fixture(root.path(), backend);
        retain_prepared(&mut inputs, &prepared, &store).unwrap();
        assert_eq!(
            runtime_digest(&inputs, &store).unwrap(),
            ContentDigest::of(b"metadata fixture")
        );
        // Corrupt the ACTUAL factory variant while leaving both profile fields
        // consistent. Retained input need not have passed our retention helper.
        prepared.inventory = inventory(opposite, &artifact);
        let manifest = store.put(&serde_json::to_vec(&prepared).unwrap()).unwrap();
        inputs.auxiliary.insert(MANIFEST.into(), manifest);
        mismatch(runtime_digest(&inputs, &store).unwrap_err());

        let artifacts = inputs
            .digests()
            .enumerate()
            .map(|(index, digest)| {
                let path = store
                    .materialize(digest, &root.path().join(format!("input-{index}")))
                    .unwrap();
                (digest.clone(), path)
            })
            .collect();
        let output = root.path().join("out");
        fs::create_dir(&output).unwrap();
        let simulation = crate::fixture_simulation("oracle");
        let cancellation = CancellationToken::default();
        let performance = BTreeMap::new();
        let request = ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            observations: None,
            seed: 42,
            program: &[],
            quantum_ns: simulation.simulator.quantum_ns,
            progress_budget: simulation.simulator.progress_budget,
            allow_crashes: &Default::default(),
            limits: &simulation.limits,
            network: &simulation.network,
            schedule: &simulation.schedule,
            control: None,
            check: &simulation.check,
            prepared: &inputs,
            artifacts: &artifacts,
            output: &output,
            cancellation: &cancellation,
            performance_parameters: &performance,
            spawner: None,
        };
        mismatch(
            super::request(&request, &output.join("host.sock"), None)
                .err()
                .unwrap(),
        );
        // Generic WorkerLaunch used to pass backend=None and bypass comparison.
        // There is now NO optional CPU argument; both launch routes use profile.
        mismatch(
            crate::worker_process::WorkerLaunch::prepare_driver_with_runtime(&request, None)
                .err()
                .unwrap(),
        );
        mismatch(
            crate::worker_process::WorkerLaunch::prepare_driver_with_runtime(&request, None)
                .err()
                .unwrap(),
        );
        assert!(!output
            .join(crate::worker_process::WORKER_REQUEST_FILE)
            .exists());
    }
}

#[test]
fn retained_worker_launch_serializes_the_driver_contract_without_starting_it() {
    use crate::execution::PreparedImage;
    use seedbox_artifacts::image::Launch;
    for backend in [CpuBackend::Pv, CpuBackend::Tcg] {
        let root = tempfile::tempdir().unwrap();
        let store = ContentStore::new(root.path().join("store"));
        let (mut inputs, prepared, artifact) = fixture(root.path(), backend);
        let digest = ContentDigest::try_from(artifact.sha256).unwrap();
        inputs.wall_clock_epoch = Some(86400);
        inputs.guests.insert("oracle".into(), digest.clone());
        inputs.images.insert(
            "oracle".into(),
            PreparedImage {
                manifest: digest.clone(),
                config: digest.clone(),
                layers: vec![],
                launch: Launch {
                    argv: prepared.oracle.process.argv.clone(),
                    environment: Default::default(),
                    working_dir: "/".into(),
                    user: "".into(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        );
        retain_prepared(&mut inputs, &prepared, &store).unwrap();
        let artifacts = inputs
            .digests()
            .enumerate()
            .map(|(index, digest)| {
                let path = store
                    .materialize(digest, &root.path().join(format!("input-{index}")))
                    .unwrap();
                (digest.clone(), path)
            })
            .collect();
        let output = root.path().join("output");
        fs::create_dir(&output).unwrap();
        let simulation = crate::fixture_simulation("oracle");
        let cancellation = CancellationToken::default();
        let performance = BTreeMap::from([("SEEDBOX_MACHINE_WORKERS".into(), "4".into())]);
        let request = ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            observations: None,
            seed: 42,
            program: &[],
            quantum_ns: simulation.simulator.quantum_ns,
            progress_budget: simulation.simulator.progress_budget,
            allow_crashes: &Default::default(),
            limits: &simulation.limits,
            network: &simulation.network,
            schedule: &simulation.schedule,
            control: None,
            check: &simulation.check,
            prepared: &inputs,
            artifacts: &artifacts,
            output: &output,
            cancellation: &cancellation,
            performance_parameters: &performance,
            spawner: None,
        };
        let launch =
            crate::worker_process::WorkerLaunch::prepare_driver_with_runtime(&request, None)
                .unwrap();
        let args: Vec<_> = launch
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..3],
            ["__engine", crate::engine::DRIVER_WORLD_V1, "--owner-pipe"]
        );
        let retained: wire::Request = serde_json::from_slice(
            &fs::read(output.join(crate::worker_process::WORKER_REQUEST_FILE)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            retained.prepared.oracle.process.argv,
            prepared.oracle.process.argv
        );
        assert_eq!(retained.seed, 42);
        assert_eq!(retained.machine_workers, 4);
        assert!(retained.start_paused);
        assert!(!output.join("world.json").exists());
        drop(launch);
        assert!(!output.join("host-control").exists());
        // An input this engine cannot carry out is the launch's own refusal,
        // naming the input, before any worker request is written.
        fs::remove_file(output.join(crate::worker_process::WORKER_REQUEST_FILE)).unwrap();
        let program: Vec<seedbox_engine_protocol::input_program::TimedInput> =
            serde_json::from_value(serde_json::json!([{
                "key": "race-on",
                "at_ns": 0,
                "action": {"action": "preempt", "handle": "race", "machine": "oracle", "ppm": 10},
            }]))
            .unwrap();
        let refused = ExecutionRequest {
            program: &program,
            ..request
        };
        let yields = backend == CpuBackend::Pv && cfg!(target_arch = "aarch64");
        match crate::worker_process::WorkerLaunch::prepare_driver_with_runtime(&refused, None) {
            Ok(_) => assert!(yields, "{backend:?} launched a preempt program"),
            Err(error) => {
                assert!(!yields, "{error}");
                let (engine, lacks) = match backend {
                    CpuBackend::Pv => ("x86_64 PV", "the guest yield hook"),
                    CpuBackend::Tcg => ("x86_64 TCG", "the guest yield hook"),
                };
                assert!(matches!(error, crate::Error::Setup(_)), "{error:?}");
                assert_eq!(
                    error.to_string(),
                    format!(
                        "input race-on: preempt needs {lacks}, which {engine} does not provide"
                    )
                );
                assert!(!output
                    .join(crate::worker_process::WORKER_REQUEST_FILE)
                    .exists());
            }
        }
    }
}

#[test]
fn request_keeps_mapping_import_and_worker_option_error_order() {
    use crate::execution::PreparedImage;
    use seedbox_artifacts::image::Launch;

    for (case, expected) in [
        ("mapping_before_workers", "was not retained/materialized"),
        (
            "import_before_workers",
            "truncated complete newc boot archive",
        ),
        ("invalid_workers", "worker count must be in 1..64"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = ContentStore::new(root.path().join("store"));
        let bytes: &[u8] = if case == "import_before_workers" {
            b"070701"
        } else {
            b"metadata fixture"
        };
        let (mut inputs, mut prepared, artifact) =
            fixture_bytes(root.path(), CpuBackend::Tcg, bytes);
        let digest = ContentDigest::try_from(artifact.sha256.clone()).unwrap();
        inputs.wall_clock_epoch = Some(86400);
        inputs.guests.insert("oracle".into(), digest.clone());
        inputs.images.insert(
            "oracle".into(),
            PreparedImage {
                manifest: digest.clone(),
                config: digest.clone(),
                layers: vec![],
                launch: Launch {
                    argv: prepared.oracle.process.argv.clone(),
                    environment: Default::default(),
                    working_dir: "/".into(),
                    user: "".into(),
                    volumes: vec![],
                    disk_volumes: vec![],
                },
            },
        );
        retain_prepared(&mut inputs, &prepared, &store).unwrap();
        let missing = Artifact {
            sha256: "f".repeat(64),
            ..artifact
        };
        if let wire::Inventory::Tcg { kernel, .. } = &mut prepared.inventory {
            if case == "mapping_before_workers" {
                *kernel = missing;
            }
        }
        let manifest = store.put(&serde_json::to_vec(&prepared).unwrap()).unwrap();
        inputs.auxiliary.insert(MANIFEST.into(), manifest);
        let artifacts: BTreeMap<_, _> = inputs
            .digests()
            .enumerate()
            .map(|(index, digest)| {
                let path = store
                    .materialize(digest, &root.path().join(format!("input-{index}")))
                    .unwrap();
                (digest.clone(), path)
            })
            .collect();
        let output = root.path().join("out");
        fs::create_dir(&output).unwrap();
        let simulation = crate::fixture_simulation("oracle");
        let cancellation = CancellationToken::default();
        let performance = BTreeMap::from([("SEEDBOX_MACHINE_WORKERS".into(), "0".into())]);
        let request = ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            observations: None,
            seed: 42,
            program: &[],
            quantum_ns: simulation.simulator.quantum_ns,
            progress_budget: simulation.simulator.progress_budget,
            allow_crashes: &Default::default(),
            limits: &simulation.limits,
            network: &simulation.network,
            schedule: &simulation.schedule,
            control: None,
            check: &simulation.check,
            prepared: &inputs,
            artifacts: &artifacts,
            output: &output,
            cancellation: &cancellation,
            performance_parameters: &performance,
            spawner: None,
        };
        let error = super::request(&request, &output.join("host.sock"), None)
            .expect_err("fixture must reject invalid input")
            .to_string();
        assert!(error.contains(expected), "{case}: {error}");
        assert!(!output
            .join(crate::worker_process::WORKER_REQUEST_FILE)
            .exists());
    }
}

#[test]
fn spawned_machines_join_the_retained_manifest_once_by_name() {
    let root = tempfile::tempdir().unwrap();
    let store = ContentStore::new(root.path().join("store"));
    let (mut inputs, prepared, artifact) = fixture(root.path(), CpuBackend::Pv);
    retain_prepared(&mut inputs, &prepared, &store).unwrap();
    let file = |name: &str, bytes: &[u8]| {
        let path = root.path().join(name);
        fs::write(&path, bytes).unwrap();
        Artifact {
            sha256: ContentDigest::of(bytes).hex().into(),
            length: bytes.len() as u64,
            artifact: path.to_str().unwrap().into(),
        }
    };
    let spawned = |filesystem: Artifact| wire::Spawned {
        spec: r#"{"image":"replica"}"#.into(),
        machine: wire::Machine {
            name: "replica".into(),
            filesystem,
            ..prepared.oracle.clone()
        },
    };
    let environment = wire::Environment {
        hostname: "replica".into(),
        hosts: "10.0.0.3 replica\n".into(),
        anonymous_volumes: vec![],
        named_volumes: vec![],
        configs: BTreeMap::from([("/etc/role".into(), file("role", b"replica"))]),
        limits: wire::GuestLimits {
            max_map_count: std::num::NonZeroU32::MIN,
            nofile: std::num::NonZeroU32::MIN,
        },
        control_socket: false,
    };
    let filesystem = file("replica-filesystem", b"replica filesystem");
    retain_spawned(
        &mut inputs,
        vec![(spawned(filesystem.clone()), environment.clone())],
        &store,
    )
    .unwrap();
    let manifest = |inputs: &PreparedInputs| -> Prepared {
        serde_json::from_slice(&store.read(&inputs.auxiliary[MANIFEST]).unwrap()).unwrap()
    };
    let retained = manifest(&inputs);
    assert_eq!(retained.spawned.len(), 1);
    assert_eq!(retained.spawned[0].spec, r#"{"image":"replica"}"#);
    assert_eq!(
        retained.spawned[0].machine.filesystem.artifact,
        format!("asset:{}", filesystem.sha256)
    );
    let wire::Inventory::Pv { environments, .. } = &retained.inventory else {
        panic!("fixture is PV");
    };
    assert_eq!(environments["replica"].hostname, "replica");
    assert!(environments["replica"].configs["/etc/role"]
        .artifact
        .starts_with("asset:"));
    // Every stored object stays in the closure, the initial ones included.
    let objects = Closure::from_inputs(&inputs).unwrap().unwrap().objects;
    for sha in [&artifact.sha256, &filesystem.sha256] {
        assert!(objects.iter().any(|object| object.hex() == sha.as_str()));
    }
    // A machine the manifest already holds stays as it is.
    let before = inputs.auxiliary.clone();
    let other = file("other-filesystem", b"other filesystem");
    retain_spawned(&mut inputs, vec![(spawned(other), environment)], &store).unwrap();
    assert_eq!(inputs.auxiliary, before);
}

#[test]
fn stored_artifacts_retain_by_reference_only_when_the_store_holds_them() {
    let root = tempfile::tempdir().unwrap();
    let store = ContentStore::new(root.path().join("store"));
    let (inputs, prepared, artifact) = fixture(root.path(), CpuBackend::Pv);
    let mut from_paths = inputs.clone();
    retain_prepared(&mut from_paths, &prepared, &store).unwrap();
    // A reference to a stored object retains the same closure without its source.
    fs::remove_file(&artifact.artifact).unwrap();
    let referenced = |artifact: Artifact| {
        let mut prepared = prepared.clone();
        visit(&mut prepared, |slot| {
            *slot = artifact.clone();
            Ok(())
        })
        .unwrap();
        prepared
    };
    let stored = Artifact {
        artifact: format!("asset:{}", artifact.sha256),
        ..artifact.clone()
    };
    let mut from_store = inputs.clone();
    retain_prepared(&mut from_store, &referenced(stored.clone()), &store).unwrap();
    assert_eq!(from_store.auxiliary, from_paths.auxiliary);
    // It must name a complete object of its encoded length.
    let error = retain_prepared(
        &mut inputs.clone(),
        &referenced(Artifact {
            length: stored.length + 1,
            ..stored.clone()
        }),
        &store,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("encoded length differs"),
        "{error}"
    );
    let absent = ContentDigest::of(b"never stored");
    let error = retain_prepared(
        &mut inputs.clone(),
        &referenced(Artifact {
            sha256: absent.hex().into(),
            artifact: format!("asset:{}", absent.hex()),
            ..stored.clone()
        }),
        &store,
    )
    .unwrap_err();
    assert!(error.to_string().contains(absent.hex()), "{error}");
    // And its bytes must still be the ones it is named for. A new store
    // instance has verified nothing yet.
    let object = root.path().join("store/sha256").join(&stored.sha256);
    let mut bytes = fs::read(&object).unwrap();
    bytes[0] ^= 1;
    fs::write(&object, bytes).unwrap();
    let error = retain_prepared(
        &mut inputs.clone(),
        &referenced(stored),
        &ContentStore::new(root.path().join("store")),
    )
    .unwrap_err();
    assert!(error.to_string().contains("hash mismatch"), "{error}");
}

/// What a resubmitted definition can change about a prepared two-machine
/// world: the oracle and `db`, which mounts one named volume.
struct Edited {
    inputs: PreparedInputs,
    progress_budget: seedbox_spec::progress::ProgressBudget,
    allow_crashes: BTreeSet<String>,
    control: Option<crate::config::Control>,
}
fn adapt_edited(backend: CpuBackend, edit: impl FnOnce(&mut Edited)) -> Result<wire::Request> {
    use crate::execution::PreparedImage;
    use seedbox_artifacts::image::Launch;
    let root = tempfile::tempdir().unwrap();
    let store = ContentStore::new(root.path().join("store"));
    let (mut inputs, mut prepared, artifact) = fixture(root.path(), backend);
    let digest = ContentDigest::try_from(artifact.sha256).unwrap();
    let data = seedbox_spec::VolumeMount {
        source: "data".into(),
        target: "/data".into(),
        size_bytes: None,
    };
    let environment = |name: &str, named_volumes| wire::Environment {
        hostname: name.into(),
        hosts: format!("10.0.0.1 {name}\n"),
        anonymous_volumes: vec![],
        named_volumes,
        configs: BTreeMap::new(),
        limits: wire::GuestLimits {
            max_map_count: std::num::NonZeroU32::MIN,
            nofile: std::num::NonZeroU32::MIN,
        },
        control_socket: false,
    };
    prepared.machines.push(wire::Machine {
        name: "db".into(),
        disk: Some(wire::Disk::Zero { bytes: 1 << 30 }),
        ..prepared.oracle.clone()
    });
    let (wire::Inventory::Pv { environments, .. } | wire::Inventory::Tcg { environments, .. }) =
        &mut prepared.inventory;
    environments.insert("oracle".into(), environment("oracle", vec![]));
    environments.insert(
        "db".into(),
        environment(
            "db",
            vec![wire::NamedVolume {
                name: "data".into(),
                target: "/data".into(),
            }],
        ),
    );
    inputs.wall_clock_epoch = Some(86400);
    for (name, disk_volumes) in [("oracle", vec![]), ("db", vec![data])] {
        inputs.machine_memory_mb.insert(name.into(), 128);
        inputs.guests.insert(name.into(), digest.clone());
        inputs.placement.insert(name.into(), name.into());
        inputs.images.insert(
            name.into(),
            PreparedImage {
                manifest: digest.clone(),
                config: digest.clone(),
                layers: vec![],
                launch: Launch {
                    argv: prepared.oracle.process.argv.clone(),
                    environment: Default::default(),
                    working_dir: "/".into(),
                    user: "".into(),
                    volumes: vec![],
                    disk_volumes,
                },
            },
        );
    }
    retain_prepared(&mut inputs, &prepared, &store).unwrap();
    let artifacts = inputs
        .digests()
        .enumerate()
        .map(|(index, digest)| {
            let path = store
                .materialize(digest, &root.path().join(format!("input-{index}")))
                .unwrap();
            (digest.clone(), path)
        })
        .collect();
    let mut edited = Edited {
        inputs,
        progress_budget: Default::default(),
        allow_crashes: BTreeSet::new(),
        control: None,
    };
    edit(&mut edited);
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let simulation = crate::fixture_simulation("oracle");
    let cancellation = CancellationToken::default();
    let performance = BTreeMap::new();
    let request = ExecutionRequest {
        start_paused: true,
        resume_checkpoint: None,
        intervention_history: None,
        observations: None,
        seed: 42,
        program: &[],
        quantum_ns: simulation.simulator.quantum_ns,
        progress_budget: edited.progress_budget,
        allow_crashes: &edited.allow_crashes,
        limits: &simulation.limits,
        network: &simulation.network,
        schedule: &simulation.schedule,
        control: edited.control.as_ref(),
        check: &simulation.check,
        prepared: &edited.inputs,
        artifacts: &artifacts,
        output: &output,
        cancellation: &cancellation,
        performance_parameters: &performance,
        spawner: None,
    };
    super::request(&request, &output.join("host.sock"), None)
}
fn refused(backend: CpuBackend, edit: impl FnOnce(&mut Edited)) -> String {
    let error = adapt_edited(backend, edit).expect_err("the edit must be refused");
    assert!(matches!(error, Error::Setup(_)), "{error:?}");
    error.to_string()
}

#[test]
fn unedited_definitions_adapt_their_prepared_world() {
    for backend in [CpuBackend::Pv, CpuBackend::Tcg] {
        let worker = adapt_edited(backend, |_| {}).unwrap();
        assert!(!worker.prepared.oracle.allow_crashes);
        assert!(!worker.prepared.machines[0].allow_crashes);
        assert_eq!(
            worker.prepared.machines[0].disk,
            Some(wire::Disk::Zero { bytes: 1 << 30 })
        );
    }
}

#[test]
fn edited_allow_crashes_reach_the_worker_request() {
    for backend in [CpuBackend::Pv, CpuBackend::Tcg] {
        let worker = adapt_edited(backend, |edited| {
            edited.allow_crashes.insert("db".into());
        })
        .unwrap();
        assert!(!worker.prepared.oracle.allow_crashes);
        assert_eq!(worker.prepared.machines[0].name, "db");
        assert!(worker.prepared.machines[0].allow_crashes);
    }
}

#[test]
fn edits_the_prepared_boot_archives_embed_are_refused_by_field() {
    let budget = |blocks| std::num::NonZeroU32::new(blocks).unwrap().into();
    let error = refused(CpuBackend::Pv, |edited| edited.progress_budget = budget(17));
    assert_eq!(
        error,
        "simulator.blocksPerProgressCall 17 differs from the prepared 10000; a resolved definition replays prepared boot archives that embed it, so it cannot be changed"
    );
    // TCG has no translator budget, so its recorded value changes nothing.
    adapt_edited(CpuBackend::Tcg, |edited| {
        edited.progress_budget = budget(17)
    })
    .unwrap();
    for backend in [CpuBackend::Pv, CpuBackend::Tcg] {
        let error = refused(backend, |edited| {
            edited.inputs.wall_clock_epoch = Some(86401);
        });
        assert!(
            error.starts_with("clock.epochSeconds 86401 differs from the prepared 86400;"),
            "{error}"
        );
        let error = refused(backend, |edited| {
            edited.control = Some(crate::config::Control {
                service: "oracle".into(),
            });
        });
        assert!(
            error.starts_with("simulator.oracleControl true differs from the prepared false;"),
            "{error}"
        );
        let error = refused(backend, |edited| {
            let launch = &mut edited.inputs.images.get_mut("db").unwrap().launch;
            launch.disk_volumes[0].size_bytes = Some(2 << 30);
        });
        assert!(
            error.starts_with("machines.db.volumes differs from the prepared machine;"),
            "{error}"
        );
        let error = refused(backend, |edited| {
            let launch = &mut edited.inputs.images.get_mut("db").unwrap().launch;
            launch.disk_volumes[0].target = "/elsewhere".into();
        });
        assert!(error.starts_with("machines.db.volumes differs"), "{error}");
        let error = refused(backend, |edited| {
            let volumes = edited.inputs.images["db"].launch.disk_volumes.clone();
            edited
                .inputs
                .images
                .get_mut("oracle")
                .unwrap()
                .launch
                .disk_volumes = volumes;
        });
        assert!(error.starts_with("oracle.volumes differs"), "{error}");
        let error = refused(backend, |edited| {
            let launch = &mut edited.inputs.images.get_mut("oracle").unwrap().launch;
            launch.volumes.push("/cache".into());
        });
        assert!(
            error.starts_with("oracle.anonymousVolumes differs"),
            "{error}"
        );
        let error = refused(backend, |edited| {
            let db = edited.inputs.images["db"].clone();
            edited.inputs.images.insert("extra".into(), db);
        });
        assert!(
            error.starts_with("machines.extra is not in the prepared world;"),
            "{error}"
        );
    }
}
