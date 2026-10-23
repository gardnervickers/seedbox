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
            progress_budget: std::num::NonZeroU32::new(32).unwrap().into(),
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
        },
        machines: vec![],
        spawnable: vec![],
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
    }
}

#[test]
fn request_keeps_checksum_mapping_import_and_worker_option_error_order() {
    use crate::execution::PreparedImage;
    use seedbox_artifacts::image::Launch;
    use std::os::unix::fs::PermissionsExt;

    for (case, expected) in [
        ("checksum_before_mapping", "SHA-256 differs"),
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
        if let wire::Inventory::Tcg {
            kernel, formatter, ..
        } = &mut prepared.inventory
        {
            match case {
                "checksum_before_mapping" => *formatter = Some(missing),
                "mapping_before_workers" => *kernel = missing,
                _ => {}
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
        if case == "checksum_before_mapping" {
            let path = &artifacts[&digest];
            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
            let mut changed = bytes.to_vec();
            changed[0] ^= 1;
            fs::write(path, changed).unwrap();
        }
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
