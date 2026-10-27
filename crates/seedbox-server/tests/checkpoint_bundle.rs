//! A checkpoint file restores its inputs on an empty server without the source
//! resource, registering nothing and executing nothing.
#![cfg(unix)]
use seedbox_api::*;
use seedbox_application::{execution::PreparedInputs, store::ContentStore};
use seedbox_artifacts::{
    archive::{self, Options},
    execution::*,
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
