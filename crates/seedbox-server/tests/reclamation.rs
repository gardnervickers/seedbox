//! Deleting resources reclaims the cache objects only they referenced.
#![cfg(unix)]
use seedbox_api::*;
use seedbox_application::{
    definition,
    execution::{CancellationToken, PreparedInputs},
    store::{ContentDigest, ContentStore},
};
use seedbox_artifacts::archive::Options;
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
    test_support,
};
use std::{collections::BTreeSet, fs, path::Path, sync::Arc};

struct NeverExecute;
impl BackendProvider for NeverExecute {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: seedbox_spec::backend::CpuBackend,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("reclamation tests never prepare images")
    }
    fn retained(
        &self,
        _: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn seedbox_application::execution::ExecutionBackend>>
    {
        panic!("reclamation tests never execute")
    }
}

const RAM: &[u8] = b"captured checkpoint RAM";

/// The cache objects on disk, by name.
fn cache(root: &Path) -> BTreeSet<String> {
    match fs::read_dir(root.join("cache/sha256")) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(_) => BTreeSet::new(),
    }
}

fn markers(root: &Path) -> usize {
    fs::read_dir(root.join("owned")).map_or(0, |entries| entries.count())
}

/// A server whose completed `source` captured checkpoint `capture` natively.
fn server(root: &Path) -> (Arc<Manager>, ExecutionService, ContentStore, SimulationId) {
    let store = ContentStore::with_ephemeral_evidence(root.join("cache"), root.join("evidence"))
        .with_owned_objects();
    // Inputs the cache already holds are never reclaimed.
    store.put(b"catalogue-only-binary").unwrap();
    let mut catalogue = Catalogue::in_memory();
    let mut simulation = catalogue
        .submit(test_support::resolved_request("source"))
        .unwrap()
        .0;
    let id = simulation.metadata.id.clone();
    for phase in [Phase::Preparing, Phase::Running] {
        simulation.status.phase = phase;
        simulation = catalogue.update(simulation).unwrap();
    }
    let request = ControlRequest {
        request_id: "capture".into(),
        command: ControlCommand::Checkpoint,
    };
    catalogue.accept_control(&id, request).unwrap();
    let state = ControlState::Acknowledged {
        status: PlaybackStatus {
            position: Default::default(),
            rate: PlaybackRate::Paused,
            achieved_rate: None,
            stepping: false,
            before_next_fault: false,
            terminal: false,
            application_ready: true,
        },
        error: None,
        intervention: None,
        detail: None,
        checkpoint: Some(CheckpointStatus {
            request_id: "capture".into(),
            position: Default::default(),
            state: CheckpointState::Captured,
        }),
    };
    catalogue
        .acknowledge_control(&id, "capture", state)
        .unwrap();
    simulation.status.phase = Phase::Completed;
    catalogue.update(simulation).unwrap();
    let manager = Arc::new(Manager::new(catalogue));
    let execution = ExecutionService::new(
        manager.clone(),
        root.into(),
        store.clone(),
        Arc::new(NeverExecute),
    )
    .unwrap();
    let attempt = root.join("runs").join(id.as_str()).join("attempt");
    fs::create_dir_all(attempt.join("output/machines")).unwrap();
    let memory = std::path::absolute(attempt.join("ram.bin")).unwrap();
    fs::write(&memory, RAM).unwrap();
    let native = serde_json::json!({
        "state": {"world": {"input_program": {"inputs": [], "next": 0, "admissions": [], "rejections": []}}},
        "ram": {"path": memory, "sha256": ContentDigest::of(RAM).hex()},
    });
    fs::write(
        attempt.join("output/machines/checkpoint-capture.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    seedbox_artifacts::interventions::Journal::initialize(&attempt.join("interventions.json"))
        .unwrap();
    let InputSource::Resolved {
        definition: resolved,
    } = &manager.describe(&id).unwrap().spec.inputs
    else {
        unreachable!()
    };
    let prepared = definition::prepared(resolved, &store).unwrap();
    fs::write(
        root.join("inputs").join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "checkpoint": null,
            "prepared": prepared,
            "attempt": std::path::absolute(&attempt).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    (manager, execution, store, id)
}

/// Submit and complete `spec` without executing it.
fn complete(manager: &Manager, name: &str, spec: Option<SimulationSpec>) -> SimulationId {
    let mut request = test_support::resolved_request(name);
    if let Some(spec) = spec {
        request.spec = spec;
    }
    let id = manager.submit(request).unwrap().0.metadata.id;
    manager.begin(&id).unwrap();
    manager
        .update(&id, |s| s.status.phase = Phase::Completed)
        .unwrap();
    id
}

/// Submit and complete a branch from `capture` without executing it.
fn branch(
    manager: &Manager,
    execution: &ExecutionService,
    source: &SimulationId,
    name: &str,
) -> SimulationId {
    let resource = execution.checkpoint_definition(source, "capture").unwrap();
    complete(manager, name, Some(resource.spec))
}

#[test]
fn deleting_a_branch_and_its_source_returns_the_cache_to_its_baseline() {
    let root = tempfile::tempdir().unwrap();
    let (manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    let ram = ContentDigest::of(RAM);
    let branched = branch(&manager, &execution, &source, "branch");
    assert!(cache(root.path()).contains(ram.hex()));
    assert_eq!(store.owned_objects(), BTreeSet::from([ram.clone()]));
    execution.delete(&branched).unwrap();
    // The source can still be branched: its exported capture stays.
    assert!(cache(root.path()).contains(ram.hex()));
    execution.delete(&source).unwrap();
    assert_eq!(cache(root.path()), baseline);
    assert!(store.owned_objects().is_empty());
    assert_eq!(markers(root.path()), 0);
}

#[test]
fn deleting_an_unbranched_source_reclaims_its_export() {
    let root = tempfile::tempdir().unwrap();
    let (_manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    execution.checkpoint_definition(&source, "capture").unwrap();
    store.verify(&ContentDigest::of(RAM)).unwrap();
    execution.delete(&source).unwrap();
    assert_eq!(cache(root.path()), baseline);
}

#[test]
fn a_branch_keeps_the_captured_state_it_needs_after_its_source_is_deleted() {
    let root = tempfile::tempdir().unwrap();
    let (manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    let branched = branch(&manager, &execution, &source, "branch");
    let replayed = branch(&manager, &execution, &source, "replay");
    execution.delete(&source).unwrap();
    let InputSource::Resolved {
        definition: resolved,
    } = manager.describe(&branched).unwrap().spec.inputs
    else {
        unreachable!()
    };
    let start = root.path().join("start");
    let manifest = definition::materialize_start(&resolved, &store, &start)
        .unwrap()
        .unwrap();
    assert!(manifest.is_file());
    execution.delete(&branched).unwrap();
    store.verify(&ContentDigest::of(RAM)).unwrap();
    execution.delete(&replayed).unwrap();
    assert_eq!(cache(root.path()), baseline);
}

#[test]
fn a_checkpoint_file_keeps_its_state_until_the_branch_submitted_from_it_is_deleted() {
    let root = tempfile::tempdir().unwrap();
    let (manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    let ram = ContentDigest::of(RAM);
    let mut bytes = Vec::new();
    execution
        .export_checkpoint_bundle(&source, "capture", &mut bytes, &Options::default())
        .unwrap();
    execution.delete(&source).unwrap();
    assert_eq!(cache(root.path()), baseline);
    // Uploading the file owns its state again until a submission names it.
    let unrelated = complete(&manager, "unrelated", None);
    let definition = execution
        .import_checkpoint_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    assert_eq!(store.owned_objects(), BTreeSet::from([ram.clone()]));
    execution.delete(&unrelated).unwrap();
    store.verify(&ram).unwrap();
    let spec = SimulationSpec::resolved(definition).unwrap();
    let branched = complete(&manager, "branch", Some(spec));
    let other = complete(&manager, "other", None);
    execution.delete(&other).unwrap();
    store.verify(&ram).unwrap();
    execution.delete(&branched).unwrap();
    assert_eq!(cache(root.path()), baseline);
    assert_eq!(markers(root.path()), 0);
}

#[test]
fn cache_inputs_with_the_same_bytes_are_never_reclaimed() {
    let root = tempfile::tempdir().unwrap();
    let (manager, execution, store, source) = server(root.path());
    let branched = branch(&manager, &execution, &source, "branch");
    // A cache input with the same bytes adopts the owned object.
    let ram = store.put(RAM).unwrap();
    assert!(store.owned_objects().is_empty());
    execution.delete(&branched).unwrap();
    execution.delete(&source).unwrap();
    store.verify(&ram).unwrap();
}
