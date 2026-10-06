//! Test-only bridge from the Python suite to the existing YAML/OCI preparation
//! API. This is an ignored Cargo test target, never part of the seedbox CLI.
use seedbox::{
    config,
    execution::*,
    oci::OciTools,
    preparation::OciPreparer,
    store::ContentStore,
    vmm::{self, VmmExecutor},
};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    workloads: BTreeMap<String, PathBuf>,
    kernel: PathBuf,
    vmm: PathBuf,
    guest_init: PathBuf,
    busybox: PathBuf,
    output: PathBuf,
}

#[test]
#[ignore = "Python acceptance setup invokes this with explicit OCI inputs; no guests are executed"]
fn prepare_acceptance_workloads() {
    let request =
        std::env::var_os("SEEDBOX_ACCEPTANCE_REQUEST").expect("explicit preparation request");
    let request: Request = serde_json::from_slice(&fs::read(request).unwrap()).unwrap();
    fs::create_dir(&request.output).unwrap();
    let output = request.output.canonicalize().unwrap();
    let store = ContentStore::new(output.join("objects"));
    let profile = VmmExecutor::new(&request.vmm, &store, 0, 1024, WitnessMode::Exits)
        .unwrap()
        .profile();
    let preparer = OciPreparer {
        tools: OciTools {
            docker: "podman".into(),
            ..OciTools::default()
        },
        kernel: request.kernel,
        engine: request.vmm,
        init: request.guest_init,
        busybox: request.busybox,
        profile,
    };
    for (name, source) in request.workloads {
        assert!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
                && name != "."
                && name != ".."
        );
        let simulation = config::load(&source, &BTreeMap::new()).unwrap();
        let prepared = preparer.prepare(&simulation, &store).unwrap();
        let directory = output.join(name);
        fs::create_dir(&directory).unwrap();
        let mut artifacts = BTreeMap::new();
        for hash in prepared.digests().collect::<BTreeSet<_>>() {
            let path = store.materialize(hash, &directory.join("inputs")).unwrap();
            // OCI preparation runs as root; the Python harness must be able to
            // hash these exported inputs before handing them to batch.sh.
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
            artifacts.insert(hash.clone(), path);
        }
        let world = vmm::world(&ExecutionRequest {
            seed: 0,
            limits: &simulation.limits,
            network: &simulation.network,
            schedule: &simulation.schedule,
            check: &simulation.check,
            prepared: &prepared,
            artifacts: &artifacts,
            output: &directory,
            cancellation: &CancellationToken::default(),
            performance_parameters: &BTreeMap::new(),
        })
        .unwrap();
        fs::write(
            directory.join("world.json"),
            serde_json::to_vec_pretty(&world).unwrap(),
        )
        .unwrap();
        let yaml = store.put_file(&source).unwrap();
        fs::write(
            directory.join("prepared.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "configuration": simulation, "inputs": prepared, "yaml": yaml,
            }))
            .unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn acceptance_workloads_use_the_supported_yaml_preparer_contract() {
    let sim = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("harness");
    let cases: serde_json::Value =
        serde_json::from_slice(&fs::read(sim.join("acceptance/cases.json")).unwrap()).unwrap();
    for (name, source) in cases["scenarios"].as_object().unwrap() {
        let simulation =
            config::load(&sim.join(source.as_str().unwrap()), &BTreeMap::new()).unwrap();
        assert!(simulation.services.contains_key(&simulation.check.service));
        if let Some(kills) = cases["faults"].get(name) {
            for member in kills["members"].as_array().unwrap() {
                assert!(simulation.services.contains_key(member.as_str().unwrap()));
            }
            assert!(kills["end_ns"].as_u64().unwrap() < simulation.limits.simulated_time_ns);
        }
    }
}
