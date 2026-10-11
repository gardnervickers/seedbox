//! Console editing, immutable definitions and filesystem persistence.
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use seedbox_api::{TimelineEntry, TimelineOrigin};
use seedbox_tui::{Console, TimelineDraft, TimelineMetadata, Update};
use std::fs;
fn entry(id: u32) -> TimelineEntry {
    TimelineEntry {
        id: format!("program:entry-{id}"),
        at_ns: u64::from(id),
        action: serde_json::json!({"action":"future_fault","novel":{"parameter":id}}),
        origin: TimelineOrigin::Program,
        position: None,
        fault_id: Some(id),
    }
}
fn draft() -> TimelineDraft {
    TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123)
}
fn key(c: char) -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
}
fn resource() -> seedbox_api::Simulation {
    let spec = serde_json::from_value(
        serde_json::json!({"seed":42,"oracle":{"image":"example/oracle:latest"}}),
    )
    .unwrap();
    let mut resource = seedbox_api::Simulation::queued(
        seedbox_api::Metadata {
            name: "test".into(),
            labels: Default::default(),
            annotations: Default::default(),
            id: "k7m2p9qx".parse().unwrap(),
            revision: 1,
            created_at_unix_ms: 10,
            deletion_requested_at_unix_ms: None,
            parent: None,
            checkpoint: None,
        },
        spec,
    );
    resource.status.phase = seedbox_api::Phase::Completed;
    resource
}
#[test]
fn one_working_copy_saves_immutable_full_definitions_and_snapshots_submission() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("directory");
    let mut draft = draft();
    let base = serde_json::json!({"apiVersion":"seedbox/v1", "kind":"Simulation", "metadata":{"name":"test"},
            "spec":{"architecture":"aarch64","overrides":[
                {"id":"entry-1","atNs":1,"action":{"action":"future"}},
                {"id":"entry-2","atNs":2,"action":{"action":"future"}}]}});
    let mut encoded = serde_json::to_value(&draft).unwrap();
    encoded["definition"] = base.clone();
    draft = serde_json::from_value(encoded).unwrap();
    draft.merge(&[entry(1), entry(2)]).unwrap();
    assert!(serde_json::to_value(&draft).unwrap()["working"].is_null());
    draft.toggle(0);
    assert_eq!(serde_json::to_value(&draft).unwrap()["definition"], base);
    draft.rename("reproduction".into());
    let submitted = draft.submission_snapshot().unwrap();
    assert_eq!(submitted["metadata"]["name"], "reproduction");
    assert_eq!(submitted["spec"]["overrides"].as_array().unwrap().len(), 1);
    let first = draft.save_definition(&directory).unwrap();
    let saved_bytes = fs::read(&first).unwrap();
    draft.toggle(1);
    assert_eq!(submitted["spec"]["overrides"].as_array().unwrap().len(), 1);
    assert_eq!(fs::read(&first).unwrap(), saved_bytes);
    let second = draft.save_definition(&directory).unwrap();
    assert_ne!(first, second);
    assert_eq!(fs::read(&first).unwrap(), saved_bytes);
    let (restored, _) = TimelineDraft::load_all(&directory).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(
        restored[0].submission_snapshot().unwrap()["spec"]["overrides"],
        serde_json::json!([])
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn persistence_and_failed_save_retry_preserve_unknown_actions_and_metadata() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("directory");
    fs::write(&directory, b"blocked").unwrap();
    let mut draft = draft();
    draft.merge(&[entry(1)]).unwrap();
    draft.toggle(0);
    draft.set_metadata(TimelineMetadata {
        name: "Failure".into(),
        application: "test".into(),
        seed: Some(42),
        recorded_at_unix_ms: 123,
    });
    assert!(draft.save(&directory).is_err());
    assert!(draft.is_unsaved());
    assert!(draft.save_error().is_some());
    fs::remove_file(&directory).unwrap();
    draft.retry_save(&directory).unwrap();
    assert!(!draft.is_unsaved());
    assert!(draft.save_error().is_none());
    let restored = TimelineDraft::load_all(&directory).unwrap().0.remove(0);
    assert_eq!(restored.entries(), draft.entries());
    assert_eq!(restored.metadata(), draft.metadata());
    assert_eq!(restored.modified_count(), 1);
    fs::write(draft.path(&directory), b"broken").unwrap();
    let (loaded, skipped) = TimelineDraft::load_all(&directory).unwrap();
    assert!(loaded.is_empty());
    assert_eq!(skipped.len(), 1);
    let mut fresh = TimelineDraft::new("k7m2p9qx".parse().unwrap(), 123);
    assert!(fresh.save(&directory).is_err());
    assert!(fresh.save_error().unwrap().contains("move it aside"));
    assert_eq!(fs::read(draft.path(&directory)).unwrap(), b"broken");
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn console_edit_failure_survives_refresh_and_retry_persists_edits() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("timelines");
    let mut console = Console::new(Some("k7m2p9qx".parse().unwrap()), directory.clone()).unwrap();
    console.apply(Update::Resource(resource()));
    console.apply(Update::Timeline(seedbox_api::FaultTimeline {
        simulation: "k7m2p9qx".parse().unwrap(),
        entries: vec![entry(1), entry(2)],
    }));
    console.input(key('3'));
    fs::create_dir_all(directory.parent().unwrap()).unwrap();
    fs::remove_dir_all(&directory).unwrap();
    fs::write(&directory, b"blocked").unwrap();
    console.input(key('d'));
    assert!(console.timelines()[0].is_unsaved());
    assert!(console.timelines()[0].save_error().is_some());
    console.apply(Update::Timeline(seedbox_api::FaultTimeline {
        simulation: "k7m2p9qx".parse().unwrap(),
        entries: vec![entry(1), entry(2)],
    }));
    assert!(console.timelines()[0].is_unsaved());
    assert_eq!(console.timelines()[0].modified_count(), 1);
    fs::remove_file(&directory).unwrap();
    console.apply(Update::Timeline(seedbox_api::FaultTimeline {
        simulation: "k7m2p9qx".parse().unwrap(),
        entries: vec![entry(1), entry(2)],
    }));
    assert!(!console.timelines()[0].is_unsaved());
    let (saved, _) = TimelineDraft::load_all(&directory).unwrap();
    assert_eq!(saved[0].modified_count(), 1);
}
#[test]
fn unknown_admitted_fault_is_persisted_before_immediate_quit() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("timelines");
    let mut console = Console::new(Some("k7m2p9qx".parse().unwrap()), directory.clone()).unwrap();
    console.apply(Update::Resource(resource()));
    console.apply(Update::FaultReceipt {
        title:"future_action".into(), request:serde_json::json!({"command":{"type":"injectFault","action":{"action":"future_action","newField":{"value":7}}}}).to_string(),
        result:Ok(serde_json::json!({"state":{"type":"acknowledged","error":null,
            "intervention":{"requestId":"new-request","position":{"boundaryIndex":4,"completedRounds":3,"simulatedTimeNs":100},
            "plan":{"id":1,"at_ns":100,"deadline_ns":200,"action":"future_action","newField":{"value":7}}}}})),
    });
    console.input(key('q'));
    assert!(console.quit_requested());
    let (saved, _) = TimelineDraft::load_all(&directory).unwrap();
    assert_eq!(saved[0].entries()[0].id, "program:new-request");
    assert_eq!(
        saved[0].entries()[0].action,
        serde_json::json!({"action":"future_action","newField":{"value":7}})
    );
}
