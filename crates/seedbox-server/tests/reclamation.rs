//! Deleting resources reclaims the cache objects only they referenced.
#![cfg(unix)]
mod support;
use seedbox_api::*;
use seedbox_application::{
    definition,
    execution::{CancellationToken, PreparedInputs},
    store::{ContentDigest, ContentStore},
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{collections::BTreeSet, fs, path::Path, sync::Arc};

struct NeverExecute;
impl BackendProvider for NeverExecute {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
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

/// A server whose completed `source` captured checkpoint `capture` natively.
fn server(root: &Path) -> (Arc<Manager>, ExecutionService, ContentStore, SimulationId) {
    let store = ContentStore::with_ephemeral_evidence(root.join("cache"), root.join("evidence"))
        .with_owned_ledger(root.join("owned"));
    // Inputs the cache already holds are never reclaimed.
    store.put(b"catalogue-only-binary").unwrap();
    let mut catalogue = Catalogue::in_memory();
    let request = support::resolved_request("source");
    let mut simulation = catalogue.submit(request).unwrap().0;
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
    let captured = CheckpointStatus {
        request_id: "capture".into(),
        position: Default::default(),
        state: CheckpointState::Captured,
    };
    let state = ControlState::Acknowledged {
        status: PlaybackStatus {
            position: Default::default(),
            rate: PlaybackRate::Paused,
            achieved_rate: None,
            stepping: false,
            terminal: false,
            application_ready: true,
        },
        error: None,
        intervention: None,
        detail: None,
        checkpoint: Some(captured),
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
    let memory =
        std::path::absolute(attempt.join("output/machines/.arm-checkpoint-capture")).unwrap();
    fs::write(&memory, RAM).unwrap();
    let native = serde_json::json!({"state": {"world": {
        "input_program": {"inputs": [], "next": 0, "admissions": [], "rejections": [], "failed": false},
        "machines": [{"path": memory, "sha256": ContentDigest::of(RAM).hex()}],
    }}});
    fs::write(
        attempt.join("output/machines/checkpoint-capture.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    for journal in [
        "interventions.json",
        "checkpoint-capture-interventions.json",
    ] {
        seedbox_artifacts::interventions::Journal::initialize(&attempt.join(journal)).unwrap();
    }
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
            "interventions": null,
            "attempt": std::path::absolute(&attempt).unwrap(),
        }))
        .unwrap(),
    )
    .unwrap();
    (manager, execution, store, id)
}

/// Submit and complete a branch from `capture` without executing it.
fn branch(
    manager: &Manager,
    execution: &ExecutionService,
    source: &SimulationId,
    name: &str,
) -> SimulationId {
    let resource = execution.checkpoint_definition(source, "capture").unwrap();
    let mut request = support::resolved_request(name);
    request.spec = resource.spec;
    let id = manager.submit(request).unwrap().0.metadata.id;
    manager.begin(&id).unwrap();
    manager
        .update(&id, |s| s.status.phase = Phase::Completed)
        .unwrap();
    id
}

#[test]
fn deleting_a_branch_and_its_source_returns_the_cache_to_its_baseline() {
    let root = tempfile::tempdir().unwrap();
    let (manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    let ram = ContentDigest::of(RAM);
    let branched = branch(&manager, &execution, &source, "branch");
    // A capture is the session's own file: linked into the session store,
    // never the cache, and owned by the resources naming it.
    let session_object = root.path().join("evidence/sha256").join(ram.hex());
    assert!(session_object.exists());
    assert!(!cache(root.path()).contains(ram.hex()));
    assert_eq!(store.owned_objects(), BTreeSet::from([ram.clone()]));
    execution.delete(&branched).unwrap();
    // The source can still be branched: its exported capture stays.
    assert!(session_object.exists());
    execution.delete(&source).unwrap();
    assert!(!session_object.exists());
    assert_eq!(cache(root.path()), baseline);
    assert!(store.owned_objects().is_empty());
    assert_eq!(
        fs::read_dir(root.path().join("owned")).map_or(0, |d| d.count()),
        0
    );
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
fn deleting_an_unbranched_checkpoint_reclaims_its_export() {
    let root = tempfile::tempdir().unwrap();
    let (_manager, execution, store, source) = server(root.path());
    let baseline = cache(root.path());
    execution.checkpoint_definition(&source, "capture").unwrap();
    assert!(store.verify(&ContentDigest::of(RAM)).is_ok());
    execution.delete_checkpoint(&source, "capture").unwrap();
    assert_eq!(cache(root.path()), baseline);
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
