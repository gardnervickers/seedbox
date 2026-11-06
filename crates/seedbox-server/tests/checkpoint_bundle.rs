//! A checkpoint file restores its inputs on an empty server without the source
//! resource, registering nothing and executing nothing. Only captured
//! checkpoints become checkpoint files.
#![cfg(unix)]
use seedbox_api::*;
use seedbox_application::{definition, execution::PreparedInputs, store::ContentStore};
use seedbox_artifacts::{
    archive::{self, Options},
    execution::*,
    record::*,
};
use seedbox_server::test_support;
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{collections::BTreeMap, sync::Arc};

struct NeverExecute;
impl BackendProvider for NeverExecute {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: seedbox_spec::backend::CpuBackend,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("checkpoint import must not prepare images or execute")
    }
    fn retained(
        &self,
        _: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn seedbox_application::execution::ExecutionBackend>>
    {
        panic!("checkpoint import must not launch retained guests")
    }
}

#[test]
fn checkpoint_file_restores_inputs_on_empty_server_without_source_resource() {
    let source = tempfile::tempdir().unwrap();
    let store = ContentStore::with_ephemeral_evidence(
        source.path().join("cache"),
        source.path().join("evidence"),
    );
    store.put(b"catalogue-only-binary").unwrap();
    let InputSource::Resolved { definition } =
        test_support::resolved_request("archive-source").spec.inputs
    else {
        unreachable!()
    };
    let mut definition = *definition;
    let ram = store.put(b"retained checkpoint RAM").unwrap();
    definition.start = seedbox_spec::definition::Start::Checkpoint {
        manifest: serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[]}}}, "ram":{"path":"asset:ram", "sha256":ram.hex()}}),
        assets: BTreeMap::from([(
            "ram".into(),
            seedbox_spec::definition::Asset(format!("sha256:{}", ram.hex())),
        )]),
    };
    let mut bytes = Vec::new();
    archive::export_checkpoint(&definition, &store, &mut bytes, &Options::default()).unwrap();
    drop(store);
    drop(source);

    let destination = tempfile::tempdir().unwrap();
    let manager = Arc::new(Manager::new(Catalogue::in_memory()));
    let store = ContentStore::with_ephemeral_evidence(
        destination.path().join("cache"),
        destination.path().join("evidence"),
    );
    let execution = ExecutionService::new(
        manager.clone(),
        destination.path().into(),
        store.clone(),
        Arc::new(NeverExecute),
    )
    .unwrap();
    let restored = execution
        .import_checkpoint_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    assert_eq!(restored, definition);
    assert_eq!(
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap(),
        0
    );
    assert_eq!(store.read(&ram).unwrap(), b"retained checkpoint RAM");
    let mut request = test_support::resolved_request("new-from-file");
    request.spec = SimulationSpec::resolved(restored).unwrap();
    let new_run = manager.submit(request).unwrap().0;
    assert!(new_run.metadata.parent.is_none());
    assert_eq!(new_run.status.phase, Phase::Queued);
}

/// NEXT-100 case 97, server arm: a checkpoint the worker acknowledged only as
/// requested whose run ends before capture, and one refused because the run
/// completed, are never exported or branchable as checkpoints, even when the
/// engine had already written its native checkpoint file.
#[test]
fn terminated_pending_checkpoints_never_become_branchable() {
    let source = tempfile::tempdir().unwrap();
    let store = ContentStore::with_ephemeral_evidence(
        source.path().join("cache"),
        source.path().join("evidence"),
    );
    store.put(b"catalogue-only-binary").unwrap();
    let mut catalogue = Catalogue::in_memory();
    let mut simulation = catalogue
        .submit(test_support::resolved_request("pending-source"))
        .unwrap()
        .0;
    let id = simulation.metadata.id.clone();
    for phase in [Phase::Preparing, Phase::Running] {
        simulation.status.phase = phase;
        simulation = catalogue.update(simulation).unwrap();
    }
    let checkpoint = |name: &str| ControlRequest {
        request_id: name.into(),
        command: ControlCommand::Checkpoint,
    };
    catalogue
        .accept_control(&id, checkpoint("refused"))
        .unwrap();
    catalogue
        .acknowledge_control(
            &id,
            "refused",
            ControlState::Acknowledged {
                status: PlaybackStatus {
                    position: Default::default(),
                    rate: PlaybackRate::Paused,
                    achieved_rate: None,
                    stepping: false,
                    before_next_fault: false,
                    terminal: true,
                    application_ready: true,
                },
                error: Some(seedbox_engine_protocol::host_control::ControlError::Completed),
                intervention: None,
                detail: None,
                checkpoint: None,
            },
        )
        .unwrap();
    // The worker acknowledged this one as requested only, so the server keeps
    // its receipt accepted (worker::Error::Pending) until the run ends.
    catalogue
        .accept_control(&id, checkpoint("pending"))
        .unwrap();
    let manager = Arc::new(Manager::new(catalogue));
    let execution = ExecutionService::new(
        manager.clone(),
        source.path().into(),
        store.clone(),
        Arc::new(NeverExecute),
    )
    .unwrap();
    let attempt = source
        .path()
        .join("runs")
        .join(id.as_str())
        .join("owned-attempt");
    std::fs::create_dir_all(attempt.join("output/machines")).unwrap();
    // The engine published its capture, then ended before acknowledging it.
    let memory = attempt.join("ram.bin");
    std::fs::write(&memory, b"published but unacknowledged RAM").unwrap();
    let ram = seedbox_artifacts::store::ContentDigest::of(b"published but unacknowledged RAM");
    let native = serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[]}}}, "ram":{"path":std::path::absolute(&memory).unwrap(), "sha256":ram.hex()}});
    std::fs::write(
        attempt.join("output/machines/checkpoint-pending.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    let InputSource::Resolved { definition: def } = &simulation.spec.inputs else {
        unreachable!()
    };
    let identity = RunIdentity {
        version: FORMAT_VERSION,
        seed: def.seed,
        configuration: simulation.spec.configuration.clone(),
        inputs: definition::prepared(def, &store).unwrap(),
        program: Vec::new(),
    };
    let record = RunRecord {
        run_id: seedbox_artifacts::store::ContentDigest::of(
            &serde_json::to_vec(&identity).unwrap(),
        ),
        identity,
        attempt: "owned-attempt".into(),
        performance_parameters: BTreeMap::new(),
        log_policy: Default::default(),
        omitted_logs: BTreeMap::new(),
        outcome: Outcome::Pass,
        report: None,
        error: None,
        logs: BTreeMap::new(),
    };
    manager
        .retain_result(&id, def.clone(), record.clone())
        .unwrap();
    manager
        .update(&id, |resource| resource.status.phase = Phase::Completed)
        .unwrap();
    write_record(&attempt, &record).unwrap();
    seedbox_artifacts::interventions::Journal::initialize(&attempt.join("interventions.json"))
        .unwrap();
    std::fs::write(source.path().join("inputs").join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({"checkpoint":null,"prepared":record.identity.inputs,"attempt":std::path::absolute(&attempt).unwrap()})).unwrap()).unwrap();
    let receipts = manager
        .read(|catalogue| catalogue.checkpoints(&id))
        .unwrap();
    assert_eq!(receipts.len(), 2);
    let pending = receipts
        .iter()
        .find(|receipt| receipt.request.request_id == "pending")
        .unwrap();
    assert_eq!(pending.state, ControlState::Interrupted);
    for name in ["pending", "refused"] {
        assert!(
            execution.checkpoint_definition(&id, name).is_err(),
            "{name}"
        );
        assert!(execution
            .export_checkpoint_bundle(&id, name, Vec::new(), &Options::default())
            .is_err());
    }
    execution.delete(&id).unwrap();
}
