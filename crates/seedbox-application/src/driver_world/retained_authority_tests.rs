//! Metadata/launch-adaptation fixtures only: no executable or guest is started.
use super::*;
use crate::{
    driver_world::{DriverBackend, DriverExecutor},
    execution::{Backend, CancellationToken, ExecutionProfile, SemanticParameters, WitnessMode},
};

fn profile(backend: DriverBackend) -> ExecutionProfile {
    let mut parameters = SemanticParameters::new(128);
    parameters.engine = Some(DriverExecutor::descriptor(backend, "x86_64"));
    ExecutionProfile {
        backend: Backend::Other(
            match backend {
                DriverBackend::Pv => "driver-pv",
                DriverBackend::Tcg => "driver-tcg",
            }
            .into(),
        ),
        engine: ContentDigest::of(b"metadata fixture"),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: parameters,
    }
}
fn inventory(backend: DriverBackend, artifact: &Artifact) -> wire::Inventory {
    match backend {
        DriverBackend::Pv => wire::Inventory::Pv {
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
            blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
            epoch_seconds: 86400,
        },
        DriverBackend::Tcg => wire::Inventory::Tcg {
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
fn fixture(root: &Path, backend: DriverBackend) -> (PreparedInputs, Prepared, Artifact) {
    let bytes = b"metadata fixture";
    let source = root.join("source");
    fs::write(&source, bytes).unwrap();
    let digest = ContentDigest::of(bytes);
    let artifact = Artifact {
        sha256: digest.hex().into(),
        length: bytes.len() as u64,
        artifact: source.to_str().unwrap().into(),
    };
    let inputs = PreparedInputs {
        wall_clock_epoch: None,
        kernel: digest,
        profile: profile(backend),
        images: Default::default(),
        placement: Default::default(),
        guests: Default::default(),
        auxiliary: Default::default(),
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
fn pairs() -> [(DriverBackend, DriverBackend); 2] {
    [
        (DriverBackend::Pv, DriverBackend::Tcg),
        (DriverBackend::Tcg, DriverBackend::Pv),
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
        mismatch(retain_prepared(&mut inputs, &prepared, &store).unwrap_err());
        assert!(!store_path.exists(), "mismatch must precede retention");
        assert!(inputs.auxiliary.is_empty());
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
        let simulation = crate::config::parse("name: metadata\nservices:\n  oracle: {image: fixture:metadata}\nx-seedbox:\n  version: 1\n  check: {service: oracle}\n", &root.path().join("compose.yaml"), &Default::default()).unwrap();
        let cancellation = CancellationToken::default();
        let performance = BTreeMap::new();
        let request = ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            replay_interventions: None,
            observations: None,
            seed: 42,
            program: &[],
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
            crate::worker_process::WorkerLaunch::prepare(&request)
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
