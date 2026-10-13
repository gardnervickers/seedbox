#![cfg(unix)]
mod support;
use seedbox_api::*;
use seedbox_application::{definition, store::ContentStore};
use seedbox_artifacts::{
    archive::{self, Options, Snapshot},
    execution::*,
    record::*,
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{collections::BTreeMap, path::Path, sync::Arc};

struct NeverExecute;
impl BackendProvider for NeverExecute {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("bundle import must not prepare images or execute")
    }
    fn retained(
        &self,
        _: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn seedbox_application::execution::ExecutionBackend>>
    {
        panic!("bundle import must not launch retained guests")
    }
}
fn service(root: &Path) -> (Arc<Manager>, Arc<ExecutionService>, ContentStore) {
    let manager = Arc::new(Manager::new(Catalogue::in_memory()));
    let store = ContentStore::with_ephemeral_evidence(root.join("cache"), root.join("evidence"))
        .with_owned_ledger(root.join("owned"));
    let execution = Arc::new(
        ExecutionService::new(
            manager.clone(),
            root.into(),
            store.clone(),
            Arc::new(NeverExecute),
        )
        .unwrap(),
    );
    (manager, execution, store)
}
fn fixture(store: &ContentStore, checkpoint: bool) -> Snapshot {
    store.put(b"catalogue-only-binary").unwrap();
    let manager = Manager::new(Catalogue::in_memory());
    let resource = manager
        .submit(support::resolved_request("archive-source"))
        .unwrap()
        .0;
    let id = resource.metadata.id;
    manager.begin(&id).unwrap();
    let simulation = manager
        .update(&id, |s| {
            s.status.phase = Phase::Completed;
            s.status.oracle_exit_code = Some(0);
            s.status.termination = Some(ExecutionTermination::OracleExited);
        })
        .unwrap();
    let InputSource::Resolved { definition: def } = &simulation.spec.inputs else {
        unreachable!()
    };
    let def = def.as_ref().clone();
    let inputs = definition::prepared(&def, store).unwrap();
    let identity = RunIdentity {
        version: FORMAT_VERSION,
        seed: def.seed,
        configuration: simulation.spec.configuration.clone(),
        inputs,
        program: Vec::new(),
        interventions: None,
    };
    let diagnostic = store
        .put_unsynced(b"raw worker diagnostic\x00\xff")
        .unwrap();
    let record = RunRecord {
        run_id: seedbox_artifacts::store::ContentDigest::of(
            &serde_json::to_vec(&identity).unwrap(),
        ),
        identity,
        attempt: "untrusted-original-attempt".into(),
        performance_parameters: BTreeMap::new(),
        engine: None,
        log_policy: Default::default(),
        omitted_logs: BTreeMap::from([(
            "worker-stderr".into(),
            "truncated at retention limit".into(),
        )]),
        outcome: Outcome::Pass,
        report: Some(ExecutionReport {
            host_wait_ns: 0,
            simulated_time_ns: Some(25),
            reason: StopReason::Completed,
            workload_exit: Some(WorkloadExit {
                service: "oracle".into(),
                code: 0,
            }),
            witness: Some(seedbox_artifacts::store::ContentDigest::of(b"witness")),
            full_state: None,
            faults: Vec::new(),
            detail: Some("retained report diagnostic".into()),
        }),
        error: None,
        logs: BTreeMap::from([("worker-stdout".into(), diagnostic)]),
        replay_of: None,
        replay: None,
    };
    let logs = vec![
        LogDelivery::Gap { first: 1, last: 3 },
        LogDelivery::Entry {
            sequence: 4,
            entry: LogEntry::Bytes {
                machine: "oracle".into(),
                incarnation: 0,
                simulated_time_ns: 25,
                stream: LogStream::Stdout,
                bytes: vec![0, 255, b'a'],
            },
        },
        LogDelivery::Closed,
    ];
    let events = vec![
        EventDelivery::Gap { first: 1, last: 2 },
        EventDelivery::Event {
            sequence: 3,
            observation: SimulationEvent {
                simulation: id.clone(),
                revision: 3,
                simulated_time_ns: Some(25),
                position: None,
                event: EventKind::SourceUnavailable,
            },
        },
        EventDelivery::Closed,
    ];
    let mut snapshot = Snapshot {
        world: manager.world(&id).unwrap(),
        simulation,
        record: Some(record),
        checkpoints: BTreeMap::new(),
        controls: Vec::new(),
        observations: BTreeMap::from([
            (
                "logs".into(),
                store
                    .put_unsynced(&serde_json::to_vec(&logs).unwrap())
                    .unwrap(),
            ),
            (
                "events".into(),
                store
                    .put_unsynced(&serde_json::to_vec(&events).unwrap())
                    .unwrap(),
            ),
        ]),
        omissions: BTreeMap::from([("frames".into(), "not retained".into())]),
    };
    let machine_logs = vec![
        LogDelivery::Gap { first: 1, last: 7 },
        LogDelivery::Entry {
            sequence: 8,
            entry: match &logs[1] {
                LogDelivery::Entry { entry, .. } => entry.clone(),
                _ => unreachable!(),
            },
        },
        LogDelivery::Closed,
    ];
    snapshot.observations.insert(
        "logs:oracle".into(),
        store
            .put_unsynced(&serde_json::to_vec(&machine_logs).unwrap())
            .unwrap(),
    );
    if checkpoint {
        let mut def = def.clone();
        let ram = store.put(b"retained checkpoint RAM").unwrap();
        def.start = seedbox_spec::definition::Start::Checkpoint {
            manifest: serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[],"failed":false}}}, "ram":{"path":"asset:ram", "sha256":ram.hex()}}),
            assets: BTreeMap::from([(
                "ram".into(),
                seedbox_spec::definition::Asset(format!("sha256:{}", ram.hex())),
            )]),
        };
        snapshot.checkpoints.insert("capture".into(), def);
        snapshot.controls.push(ControlReceipt {
            request: ControlRequest {
                request_id: "capture".into(),
                command: ControlCommand::Checkpoint,
            },
            revision: 2,
            state: ControlState::Acknowledged {
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
                checkpoint: Some(CheckpointStatus {
                    request_id: "capture".into(),
                    position: Default::default(),
                    state: CheckpointState::Captured,
                }),
            },
        });
    }
    snapshot
}
fn encode(snapshot: &Snapshot, store: &ContentStore) -> Vec<u8> {
    let mut bytes = Vec::new();
    archive::export(snapshot, store, &mut bytes, &Options::default()).unwrap();
    bytes
}

#[tokio::test]
async fn import_roundtrip_is_independent_preserves_evidence_gaps_and_branches_without_execution() {
    let source = tempfile::tempdir().unwrap();
    let (_, original, store) = service(source.path());
    let snapshot = fixture(&store, true);
    let imported = original
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    let mut bytes = Vec::new();
    original
        .export_bundle(&imported.metadata.id, &mut bytes, &Options::default())
        .unwrap();
    original.delete(&imported.metadata.id).unwrap();
    drop(original);
    drop(store);
    drop(source);
    let destination = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(destination.path());
    let imported = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    assert_ne!(imported.metadata.id, snapshot.simulation.metadata.id);
    assert_eq!(imported.status.phase, Phase::Completed);
    assert!(imported.metadata.parent.is_none() && imported.metadata.checkpoint.is_none());
    assert_eq!(
        execution.describe(&imported.metadata.id).unwrap().spec,
        snapshot.simulation.spec
    );
    assert_eq!(
        manager.world(&imported.metadata.id).unwrap().machines,
        snapshot.world.machines
    );
    let mut logs = manager.all_logs(&imported.metadata.id, false).unwrap();
    assert!(matches!(
        logs.recv().await,
        seedbox_server::stream::Delivery::Gap { first: 1, last: 3 }
    ));
    let seedbox_server::stream::Delivery::Record(record) = logs.recv().await else {
        panic!("retained bytes missing")
    };
    assert!(matches!(&record.payload, LogEntry::Bytes { bytes, .. } if bytes == &[0,255,b'a']));
    assert!(matches!(
        logs.recv().await,
        seedbox_server::stream::Delivery::Closed
    ));
    let mut machine_logs = manager
        .logs(&imported.metadata.id, "oracle", false)
        .unwrap();
    assert!(matches!(
        machine_logs.recv().await,
        seedbox_server::stream::Delivery::Gap { first: 1, last: 7 }
    ));
    let seedbox_server::stream::Delivery::Record(machine) = machine_logs.recv().await else {
        panic!("machine log missing")
    };
    assert_eq!(machine.sequence, 8);
    assert_eq!(machine.payload, record.payload);
    let mut events = manager.events(&imported.metadata.id, false).unwrap();
    assert!(matches!(
        events.recv().await,
        seedbox_server::stream::Delivery::Gap { first: 1, last: 2 }
    ));
    let seedbox_server::stream::Delivery::Record(event) = events.recv().await else {
        panic!("retained event missing")
    };
    assert_eq!(event.payload.simulation, imported.metadata.id);
    let capture = execution
        .checkpoint_definition(&imported.metadata.id, "capture")
        .unwrap();
    let InputSource::Resolved { definition } = &capture.spec.inputs else {
        unreachable!()
    };
    let materialized = tempfile::tempdir().unwrap();
    let path =
        seedbox_application::definition::materialize_start(definition, &store, materialized.path())
            .unwrap()
            .unwrap();
    let manifest: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(path).unwrap()).unwrap();
    assert_eq!(
        std::fs::read(manifest["ram"]["path"].as_str().unwrap()).unwrap(),
        b"retained checkpoint RAM"
    );
    let mut branch = support::resolved_request("branch");
    branch.spec = imported.spec.clone();
    branch.spec.inputs = InputSource::Checkpoint {
        simulation: imported.metadata.id.clone(),
        checkpoint: "capture".into(),
    };
    let branch = manager.submit(branch).unwrap().0;
    assert!(
        matches!(&branch.spec.inputs, InputSource::Resolved { definition } if matches!(definition.start, seedbox_spec::definition::Start::Checkpoint { .. }))
    );
    assert!(branch.metadata.parent.is_none());
    execution
        .delete_checkpoint(&imported.metadata.id, "capture")
        .unwrap();
    assert!(execution
        .checkpoint_definition(&imported.metadata.id, "capture")
        .is_err());
    execution.delete(&imported.metadata.id).unwrap();
    assert!(
        manager.describe(&branch.metadata.id).is_ok(),
        "portable branch must outlive its source"
    );
}

#[test]
fn imported_comparisons_use_complete_resolved_semantics_without_rewriting_records() {
    let directory = tempfile::tempdir().unwrap();
    let (_, execution, store) = service(directory.path());
    let mut snapshot = fixture(&store, false);
    let InputSource::Resolved { definition } = &mut snapshot.simulation.spec.inputs else {
        unreachable!()
    };
    definition.overrides = vec![seedbox_spec::definition::Override {
        id: "external".into(),
        at_ns: 10,
        action: serde_json::json!({"action":"clock_skew","handle":"clock-effect","machine":"oracle","offset_ns":5}),
        timeout_ns: 500,
    }];
    snapshot.simulation.spec = SimulationSpec::resolved(definition.as_ref().clone());
    let original_record = snapshot.record.clone().unwrap();
    let baseline = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    let fresh = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    assert_eq!(
        execution
            .compare(&baseline.metadata.id, &fresh.metadata.id)
            .unwrap()
            .status,
        ComparisonStatus::Match
    );
    let mut replay = snapshot.clone();
    let record = replay.record.as_mut().unwrap();
    record.identity.program = crate_program(&replay.simulation.spec);
    record.run_id =
        seedbox_artifacts::store::ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    assert_ne!(record.identity, original_record.identity);
    let replayed = execution
        .import_bundle(encode(&replay, &store).as_slice(), &Options::default())
        .unwrap();
    assert_eq!(
        execution
            .compare(&baseline.metadata.id, &replayed.metadata.id)
            .unwrap()
            .status,
        ComparisonStatus::Match
    );
    for change in 0..8 {
        let mut changed = replay.clone();
        let InputSource::Resolved { definition } = &mut changed.simulation.spec.inputs else {
            unreachable!()
        };
        match change {
            0 => definition.seed += 1,
            1 => {
                definition.overrides[0].action = serde_json::json!({"action":"clock_skew","handle":"clock-effect","machine":"oracle","offset_ns":6})
            }
            2 => definition.overrides[0].timeout_ns += 1,
            3 => definition.clock.epoch_seconds += 1,
            4 => definition.simulator.disk.torn_writes = !definition.simulator.disk.torn_writes,
            5 => definition.overrides.insert(
                0,
                seedbox_spec::definition::Override {
                    id: "earlier".into(),
                    at_ns: 10,
                    action: definition.overrides[0].action.clone(),
                    timeout_ns: 500,
                },
            ),
            6 => definition.network.latency_ns += 1,
            7 => {
                definition.start = seedbox_spec::definition::Start::Checkpoint {
                    manifest: serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[],"failed":false}}}}),
                    assets: BTreeMap::new(),
                }
            }
            _ => unreachable!(),
        }
        changed.simulation.spec = SimulationSpec::resolved(definition.as_ref().clone());
        let changed = execution
            .import_bundle(encode(&changed, &store).as_slice(), &Options::default())
            .unwrap();
        assert_eq!(
            execution
                .compare(&baseline.metadata.id, &changed.metadata.id)
                .unwrap()
                .status,
            ComparisonStatus::NotComparable,
            "change {change}"
        );
    }
    for missing in [false, true] {
        let mut changed = snapshot.clone();
        let record = changed.record.as_mut().unwrap();
        record.report.as_mut().unwrap().witness = if missing {
            None
        } else {
            Some(seedbox_artifacts::store::ContentDigest::of(
                b"different witness",
            ))
        };
        let changed = execution
            .import_bundle(encode(&changed, &store).as_slice(), &Options::default())
            .unwrap();
        assert_eq!(
            execution
                .compare(&baseline.metadata.id, &changed.metadata.id)
                .unwrap()
                .status,
            if missing {
                ComparisonStatus::NotComparable
            } else {
                ComparisonStatus::Mismatch
            }
        );
    }
    let mut unavailable = snapshot.clone();
    unavailable.record = None;
    unavailable
        .omissions
        .insert("record".into(), "execution evidence unavailable".into());
    let unavailable = execution
        .import_bundle(encode(&unavailable, &store).as_slice(), &Options::default())
        .unwrap();
    assert_eq!(
        execution
            .compare(&baseline.metadata.id, &unavailable.metadata.id)
            .unwrap()
            .status,
        ComparisonStatus::NotComparable
    );
    let mut exported = Vec::new();
    execution
        .export_bundle(&baseline.metadata.id, &mut exported, &Options::default())
        .unwrap();
    let archived = archive::import(exported.as_slice(), &store, &Options::default()).unwrap();
    assert_eq!(archived.record.unwrap(), original_record);
}
fn crate_program(spec: &SimulationSpec) -> Vec<seedbox_engine_protocol::input_program::TimedInput> {
    spec.program
        .iter()
        .map(|input| seedbox_engine_protocol::input_program::TimedInput {
            key: input.key.clone(),
            at_ns: input.at_ns,
            action: serde_json::from_value(input.action.clone()).unwrap(),
            timeout_ns: input.timeout_ns,
        })
        .collect()
}

#[test]
fn malformed_import_never_publishes_catalogue_resources_and_active_export_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(directory.path());
    let snapshot = fixture(&store, true);
    let bytes = encode(&snapshot, &store);
    for input in [&bytes[..bytes.len() / 2], b"not an archive".as_slice()] {
        assert!(execution.import_bundle(input, &Options::default()).is_err());
        assert_eq!(
            manager
                .read(|catalogue| Ok(catalogue.list().count()))
                .unwrap(),
            0
        );
    }
    let mut bad = snapshot.clone();
    bad.controls.clear();
    assert!(execution
        .import_bundle(encode(&bad, &store).as_slice(), &Options::default())
        .is_err());
    assert_eq!(
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap(),
        0
    );
    let mut bad = snapshot.clone();
    bad.observations.insert(
        "logs".into(),
        store
            .put(b"[{\"type\":\"gap\",\"first\":2,\"last\":3},{\"type\":\"closed\"}]")
            .unwrap(),
    );
    assert!(execution
        .import_bundle(encode(&bad, &store).as_slice(), &Options::default())
        .is_err());
    assert_eq!(
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap(),
        0
    );
    let queued = manager
        .submit(support::resolved_request("active"))
        .unwrap()
        .0;
    assert!(execution
        .export_bundle(&queued.metadata.id, Vec::new(), &Options::default())
        .unwrap_err()
        .to_string()
        .contains("completed"));
}

#[tokio::test]
async fn http_transfers_binary_spools_and_imports_without_enqueuing_work() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    async fn request(
        socket: &Path,
        method: &str,
        path: &str,
        body: &[u8],
        content_type: &str,
    ) -> Vec<u8> {
        let mut stream = tokio::net::UnixStream::connect(socket).await.unwrap();
        let headers = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n", body.len());
        stream.write_all(headers.as_bytes()).await.unwrap();
        if let Err(error) = stream.write_all(body).await {
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
        }
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        response
    }
    let directory = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(directory.path());
    let bytes = encode(&fixture(&store, true), &store);
    let (queue, mut work) = tokio::sync::mpsc::channel(4);
    let socket = directory.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = tokio::sync::oneshot::channel::<()>();
    let api = seedbox_server::http::router(
        seedbox_server::http::ApiState::new(manager.clone(), queue).with_execution(execution),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, api)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .unwrap();
    });
    let response = request(&socket, "POST", "/v1/bundles", &bytes, BUNDLE_CONTENT_TYPE).await;
    assert!(
        response.starts_with(b"HTTP/1.1 201 Created\r\n"),
        "{}",
        String::from_utf8_lossy(&response)
    );
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let imported: seedbox_api::Simulation = serde_json::from_slice(&response[split..]).unwrap();
    assert_eq!(imported.status.phase, Phase::Completed);
    assert!(work.try_recv().is_err());
    let response = request(
        &socket,
        "GET",
        &format!("/v1/simulations/{}/bundle", imported.metadata.id),
        &[],
        BUNDLE_CONTENT_TYPE,
    )
    .await;
    assert!(response.starts_with(b"HTTP/1.1 200 OK\r\n"));
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(String::from_utf8_lossy(&response[..split]).contains(BUNDLE_CONTENT_TYPE));
    archive::import(&response[split..], &store, &Options::default()).unwrap();
    for (path, content_type, payload) in [
        (
            "/v1/bundles?maxBytes=1",
            BUNDLE_CONTENT_TYPE,
            bytes.as_slice(),
        ),
        ("/v1/bundles?maxBytes=0", BUNDLE_CONTENT_TYPE, &[][..]),
        ("/v1/bundles", "application/json", bytes.as_slice()),
        ("/v1/bundles", BUNDLE_CONTENT_TYPE, b"malformed".as_slice()),
    ] {
        let response = request(&socket, "POST", path, payload, content_type).await;
        assert!(
            response.starts_with(b"HTTP/1.1 400 Bad Request\r\n"),
            "{}",
            String::from_utf8_lossy(&response)
        );
    }
    assert_eq!(
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap(),
        1
    );
    assert!(work.try_recv().is_err());
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

/// NEXT-100 case 10, uncertain-upload arm: an import whose client disconnects
/// before the last byte publishes nothing; one whose client disconnects after
/// the whole upload, before the response, publishes at most one complete
/// resource, and exactly one once the import was published before the
/// response was lost; nothing duplicates it (clients never retry an import).
#[tokio::test]
async fn disconnected_imports_publish_nothing_or_one_complete_resource() {
    use tokio::io::AsyncWriteExt;
    let directory = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(directory.path());
    let bytes = encode(&fixture(&store, true), &store);
    let (queue, mut work) = tokio::sync::mpsc::channel(4);
    let socket = directory.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = tokio::sync::oneshot::channel::<()>();
    let api = seedbox_server::http::router(
        seedbox_server::http::ApiState::new(manager.clone(), queue).with_execution(execution),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, api)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .unwrap();
    });
    let connect = |sent: usize| {
        let socket = socket.clone();
        let bytes = bytes.clone();
        async move {
            let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
            let headers = format!("POST /v1/bundles HTTP/1.1\r\nHost: localhost\r\nContent-Type: {BUNDLE_CONTENT_TYPE}\r\nContent-Length: {}\r\n\r\n", bytes.len());
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&bytes[..sent]).await.unwrap();
            stream.flush().await.unwrap();
            stream
        }
    };
    // Disconnect without reading any response.
    let upload = |sent: usize| {
        let connection = connect(sent);
        async move { drop(connection.await) }
    };
    let count = || {
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap()
    };
    let settled = || async {
        // Let any spawned import finish; its count must then stay fixed.
        let mut last = count();
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let now = count();
            if now == last {
                continue;
            }
            last = now;
        }
        last
    };
    for sent in [0, 1, bytes.len() / 2, bytes.len() - 1] {
        upload(sent).await;
        assert_eq!(
            settled().await,
            0,
            "{sent} of {} bytes published",
            bytes.len()
        );
    }
    upload(bytes.len()).await;
    let published = settled().await;
    assert!(published <= 1, "one upload published {published} resources");
    eprintln!("a full upload without its response published {published} resource(s)");
    if published == 1 {
        let id = manager
            .read(|catalogue| Ok(catalogue.list().next().unwrap().metadata.id.clone()))
            .unwrap();
        let resource = manager.describe(&id).unwrap();
        assert_eq!(resource.status.phase, Phase::Completed);
        assert!(manager
            .read(|catalogue| catalogue.checkpoints(&id))
            .unwrap()
            .iter()
            .any(|r| r.request.request_id == "capture"));
    }
    // The response is lost after the import was published: exactly one resource.
    let before = count();
    let connection = connect(bytes.len()).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while count() == before {
        assert!(
            std::time::Instant::now() < deadline,
            "full upload never imported"
        );
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    drop(connection);
    assert_eq!(settled().await, before + 1);
    assert!(work.try_recv().is_err(), "an import enqueued execution");
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[test]
fn export_captures_owned_native_checkpoint_files_and_preserves_consumed_override_cursor() {
    let source = tempfile::tempdir().unwrap();
    let store = ContentStore::with_ephemeral_evidence(
        source.path().join("cache"),
        source.path().join("evidence"),
    );
    let fixture = fixture(&store, true);
    let mut catalogue = Catalogue::in_memory();
    let mut request = support::resolved_request("native-source");
    request.spec = fixture.simulation.spec.clone();
    let mut simulation = catalogue.submit(request).unwrap().0;
    let id = simulation.metadata.id.clone();
    for phase in [Phase::Preparing, Phase::Running] {
        simulation.status.phase = phase;
        simulation = catalogue.update(simulation).unwrap();
    }
    let control = fixture.controls[0].clone();
    catalogue.accept_control(&id, control.request).unwrap();
    catalogue
        .acknowledge_control(&id, "capture", control.state)
        .unwrap();
    simulation.status.phase = Phase::Completed;
    catalogue.update(simulation).unwrap();
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
    let memory = attempt.join("ram.bin");
    std::fs::write(&memory, b"retained checkpoint RAM").unwrap();
    let ram = seedbox_artifacts::store::ContentDigest::of(b"retained checkpoint RAM");
    let native = serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[],"failed":false}}}, "ram":{"path":std::path::absolute(&memory).unwrap(), "sha256":ram.hex()}});
    std::fs::write(
        attempt.join("output/machines/checkpoint-capture.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    let record = fixture.record.as_ref().unwrap();
    write_record(&attempt, record).unwrap();
    seedbox_artifacts::interventions::Journal::initialize(&attempt.join("interventions.json"))
        .unwrap();
    seedbox_artifacts::interventions::Journal::initialize(
        &attempt.join("checkpoint-capture-interventions.json"),
    )
    .unwrap();
    std::fs::write(source.path().join("inputs").join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({"checkpoint":null,"prepared":record.identity.inputs,"interventions":null,"attempt":std::path::absolute(&attempt).unwrap()})).unwrap()).unwrap();
    let mut bytes = Vec::new();
    execution
        .export_bundle(&id, &mut bytes, &Options::default())
        .unwrap();
    let imported_here = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    assert_eq!(
        execution
            .compare(&id, &imported_here.metadata.id)
            .unwrap()
            .status,
        ComparisonStatus::Match
    );
    let portable = archive::import(bytes.as_slice(), &store, &Options::default()).unwrap();
    assert!(portable.omissions.contains_key("logs") && portable.omissions.contains_key("events"));
    let seedbox_spec::definition::Start::Checkpoint { manifest, .. } =
        &portable.checkpoints["capture"].start
    else {
        unreachable!()
    };
    assert!(manifest["ram"]["path"]
        .as_str()
        .unwrap()
        .starts_with("asset:"));
    execution.delete(&id).unwrap();
    drop(execution);
    drop(store);
    drop(source);
    let destination = tempfile::tempdir().unwrap();
    let (_, execution, store) = service(destination.path());
    let imported = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    execution
        .checkpoint_definition(&imported.metadata.id, "capture")
        .unwrap();
    let mut snapshot = archive::import(bytes.as_slice(), &store, &Options::default()).unwrap();
    let definition = snapshot.checkpoints.get_mut("capture").unwrap();
    let action = serde_json::json!({"action":"pause_process","machine":"oracle"});
    let input = seedbox_spec::definition::Override {
        id: "consumed".into(),
        at_ns: 10,
        action: action.clone(),
        timeout_ns: 500,
    };
    definition.overrides = vec![input.clone()];
    let seedbox_spec::definition::Start::Checkpoint { manifest, .. } = &mut definition.start else {
        unreachable!()
    };
    manifest["state"]["world"]["input_program"] = serde_json::json!({"inputs":[{"key":"consumed","at_ns":10,"action":action,"timeout_ns":500}],"next":1,"failed":false,"rejections":[],"admissions":[{"key":"consumed","plan":{"id":1,"at_ns":10,"deadline_ns":510,"action":"pause_process","machine":"oracle"}}]});
    let InputSource::Resolved { definition: main } = &mut snapshot.simulation.spec.inputs else {
        unreachable!()
    };
    main.overrides = vec![input];
    snapshot.simulation.spec = SimulationSpec::resolved(main.as_ref().clone());
    let imported = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    let resource = execution
        .checkpoint_definition(&imported.metadata.id, "capture")
        .unwrap();
    let InputSource::Resolved { definition } = resource.spec.inputs else {
        unreachable!()
    };
    definition
        .start
        .validate_overrides(&definition.overrides)
        .unwrap();
    let mut edited = definition.overrides.clone();
    edited[0].timeout_ns += 1;
    assert!(definition.start.validate_overrides(&edited).is_err());
    let mut bad = snapshot.clone();
    let seedbox_spec::definition::Start::Checkpoint { manifest, .. } =
        &mut bad.checkpoints.get_mut("capture").unwrap().start
    else {
        unreachable!()
    };
    manifest["ram"]["path"] = serde_json::json!("/tmp/untrusted");
    assert!(archive::export(&bad, &store, Vec::new(), &Options::default()).is_err());
}

/// NEXT-100 case 97, server arm: a checkpoint acknowledged as requested
/// whose run ends before capture, and one refused because the run completed,
/// are never exported, imported or branchable as checkpoints, even when the
/// engine had already written its native checkpoint file before the ack.
#[test]
fn terminated_pending_checkpoints_never_become_branchable() {
    let source = tempfile::tempdir().unwrap();
    let store = ContentStore::with_ephemeral_evidence(
        source.path().join("cache"),
        source.path().join("evidence"),
    );
    let fixture = fixture(&store, false);
    let mut catalogue = Catalogue::in_memory();
    let mut request = support::resolved_request("pending-source");
    request.spec = fixture.simulation.spec.clone();
    let mut simulation = catalogue.submit(request).unwrap().0;
    let id = simulation.metadata.id.clone();
    for phase in [Phase::Preparing, Phase::Running] {
        simulation.status.phase = phase;
        simulation = catalogue.update(simulation).unwrap();
    }
    let checkpoint = |name: &str| ControlRequest {
        request_id: name.into(),
        command: ControlCommand::Checkpoint,
    };
    let status = PlaybackStatus {
        position: Default::default(),
        rate: PlaybackRate::Paused,
        achieved_rate: None,
        stepping: false,
        terminal: true,
        application_ready: true,
    };
    catalogue
        .accept_control(&id, checkpoint("refused"))
        .unwrap();
    catalogue
        .acknowledge_control(
            &id,
            "refused",
            ControlState::Acknowledged {
                status,
                error: Some(seedbox_engine_protocol::host_control::ControlError::Completed),
                intervention: None,
                detail: None,
                checkpoint: None,
            },
        )
        .unwrap();
    // Acknowledged by the worker as requested only: the receipt stays Accepted.
    catalogue
        .accept_control(&id, checkpoint("pending"))
        .unwrap();
    simulation.status.phase = Phase::Completed;
    catalogue.update(simulation).unwrap();
    let receipts = catalogue.checkpoints(&id).unwrap();
    assert_eq!(receipts.len(), 2);
    let pending = receipts
        .iter()
        .find(|r| r.request.request_id == "pending")
        .unwrap();
    assert_eq!(pending.state, ControlState::Interrupted);

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
    let native = serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[],"failed":false}}}, "ram":{"path":std::path::absolute(&memory).unwrap(), "sha256":ram.hex()}});
    std::fs::write(
        attempt.join("output/machines/checkpoint-pending.json"),
        serde_json::to_vec(&native).unwrap(),
    )
    .unwrap();
    let record = fixture.record.as_ref().unwrap();
    write_record(&attempt, record).unwrap();
    seedbox_artifacts::interventions::Journal::initialize(&attempt.join("interventions.json"))
        .unwrap();
    std::fs::write(source.path().join("inputs").join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({"checkpoint":null,"prepared":record.identity.inputs,"interventions":null,"attempt":std::path::absolute(&attempt).unwrap()})).unwrap()).unwrap();
    for name in ["pending", "refused"] {
        assert!(
            execution.checkpoint_definition(&id, name).is_err(),
            "{name}"
        );
    }
    let mut bytes = Vec::new();
    execution
        .export_bundle(&id, &mut bytes, &Options::default())
        .unwrap();
    let portable = archive::import(bytes.as_slice(), &store, &Options::default()).unwrap();
    assert!(portable.checkpoints.is_empty());
    for name in ["pending", "refused"] {
        assert_eq!(
            portable
                .omissions
                .get(&format!("checkpoint:{name}"))
                .map(String::as_str),
            Some("capture unavailable"),
            "{:?}",
            portable.omissions
        );
    }
    execution.delete(&id).unwrap();
    drop(execution);
    let destination = tempfile::tempdir().unwrap();
    let (manager, execution, _) = service(destination.path());
    let imported = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap()
        .metadata
        .id;
    let receipts = manager
        .read(|catalogue| catalogue.checkpoints(&imported))
        .unwrap();
    assert_eq!(receipts.len(), 2);
    for receipt in &receipts {
        assert!(
            !matches!(
                &receipt.state,
                ControlState::Acknowledged {
                    checkpoint: Some(CheckpointStatus {
                        state: CheckpointState::Captured,
                        ..
                    }),
                    ..
                }
            ),
            "{receipt:?}"
        );
        assert!(execution
            .checkpoint_definition(&imported, &receipt.request.request_id)
            .is_err());
    }
}

#[test]
fn export_holds_a_stable_snapshot_until_spooled_before_deletion() {
    struct PausedWriter {
        bytes: Vec<u8>,
        entered: Option<std::sync::mpsc::Sender<()>>,
        release: std::sync::mpsc::Receiver<()>,
    }
    impl std::io::Write for PausedWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if let Some(entered) = self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv().unwrap();
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (_, execution, store) = service(directory.path());
    let snapshot = fixture(&store, true);
    let resource = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    let (entered, started) = std::sync::mpsc::channel();
    let (release, continue_export) = std::sync::mpsc::channel();
    let exporter = execution.clone();
    let id = resource.metadata.id.clone();
    let export = std::thread::spawn(move || {
        let mut output = PausedWriter {
            bytes: Vec::new(),
            entered: Some(entered),
            release: continue_export,
        };
        exporter
            .export_bundle(&id, &mut output, &Options::default())
            .unwrap();
        output.bytes
    });
    started.recv().unwrap();
    let deleter = execution.clone();
    let id = resource.metadata.id.clone();
    let (deleted, done) = std::sync::mpsc::channel();
    let deletion = std::thread::spawn(move || {
        deleter.delete_checkpoint(&id, "capture").unwrap();
        deleter.delete(&id).unwrap();
        deleted.send(()).unwrap();
    });
    assert!(
        done.recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "deletion raced an unfinished export"
    );
    release.send(()).unwrap();
    let bytes = export.join().unwrap();
    deletion.join().unwrap();
    done.recv().unwrap();
    let restored = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap();
    execution
        .checkpoint_definition(&restored.metadata.id, "capture")
        .unwrap();
}

#[tokio::test]
async fn independently_sequenced_machine_histories_survive_shared_budget_eviction_and_filtering() {
    use seedbox_server::stream::{Delivery, History, Stream, Subscription};
    async fn deliveries(mut subscription: Subscription<LogEntry>) -> Vec<LogDelivery> {
        let mut rows = Vec::new();
        loop {
            match subscription.recv().await {
                Delivery::Record(record) => rows.push(LogDelivery::Entry {
                    sequence: record.sequence,
                    entry: record.payload.clone(),
                }),
                Delivery::Gap { first, last } => rows.push(LogDelivery::Gap { first, last }),
                Delivery::Closed => {
                    rows.push(LogDelivery::Closed);
                    return rows;
                }
            }
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(directory.path());
    let mut snapshot = fixture(&store, false);
    let InputSource::Resolved { definition } = &mut snapshot.simulation.spec.inputs else {
        unreachable!()
    };
    let mut app = definition.oracle.clone();
    app.name = None;
    definition.machines.insert("app".into(), app);
    snapshot.simulation.spec = SimulationSpec::resolved(definition.as_ref().clone());
    let record = snapshot.record.as_mut().unwrap();
    let InputSource::Resolved { definition } = &snapshot.simulation.spec.inputs else {
        unreachable!()
    };
    record.identity.inputs = definition::prepared(definition, &store).unwrap();
    record.identity.configuration = snapshot.simulation.spec.configuration.clone();
    record.run_id =
        seedbox_artifacts::store::ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
    let history = Arc::new(History::new(700));
    let stream = || {
        Stream::new(
            std::num::NonZeroUsize::MIN,
            std::num::NonZeroUsize::new(4095).unwrap(),
        )
        .with_history(history.clone())
    };
    let all = stream();
    let oracle = stream();
    let app = stream();
    for round in 0..10 {
        let machine = if round % 2 == 0 { "oracle" } else { "app" };
        let entry = LogEntry::Bytes {
            machine: machine.into(),
            incarnation: 0,
            simulated_time_ns: round,
            stream: LogStream::Stdout,
            bytes: vec![round as u8; 40],
        };
        all.publish(entry.clone()).unwrap();
        if machine == "oracle" {
            oracle.publish(entry).unwrap();
        } else {
            app.publish(entry).unwrap();
        }
    }
    let aggregate = deliveries(all.snapshot().unwrap()).await;
    let oracle_rows = deliveries(oracle.snapshot().unwrap()).await;
    let app_rows = deliveries(app.snapshot().unwrap()).await;
    assert!(matches!(aggregate[0], LogDelivery::Gap { .. }));
    assert!(matches!(oracle_rows[0], LogDelivery::Gap { .. }));
    assert!(matches!(app_rows[0], LogDelivery::Gap { .. }));
    assert_ne!(aggregate, oracle_rows);
    for (key, rows) in [
        ("logs", &aggregate),
        ("logs:oracle", &oracle_rows),
        ("logs:app", &app_rows),
    ] {
        snapshot.observations.insert(
            key.into(),
            store
                .put_unsynced(&serde_json::to_vec(rows).unwrap())
                .unwrap(),
        );
    }
    let imported = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    assert_eq!(
        deliveries(manager.all_logs(&imported.metadata.id, false).unwrap()).await,
        aggregate
    );
    assert_eq!(
        deliveries(
            manager
                .logs(&imported.metadata.id, "oracle", false)
                .unwrap()
        )
        .await,
        oracle_rows
    );
    assert_eq!(
        deliveries(manager.logs(&imported.metadata.id, "app", false).unwrap()).await,
        app_rows
    );
    for rows in [&oracle_rows, &app_rows] {
        let expected = if std::ptr::eq(rows, &oracle_rows) {
            "oracle"
        } else {
            "app"
        };
        assert!(rows.iter().all(|row| !matches!(row, LogDelivery::Entry { entry: LogEntry::Bytes { machine, .. }, .. } if machine != expected)));
    }
    let mut exported = Vec::new();
    execution
        .export_bundle(&imported.metadata.id, &mut exported, &Options::default())
        .unwrap();
    let roundtrip = archive::import(exported.as_slice(), &store, &Options::default()).unwrap();
    for key in ["logs", "logs:oracle", "logs:app"] {
        assert_eq!(roundtrip.observations[key], snapshot.observations[key]);
    }
    snapshot.observations.insert(
        "logs:app".into(),
        snapshot.observations["logs:oracle"].clone(),
    );
    assert!(execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .is_err());
    assert_eq!(
        manager
            .read(|catalogue| Ok(catalogue.list().count()))
            .unwrap(),
        1
    );
}

#[test]
fn comparison_ignores_only_provenance_and_retains_assets_architecture_and_semantics() {
    let directory = tempfile::tempdir().unwrap();
    let (_, execution, store) = service(directory.path());
    let mut snapshot = fixture(&store, false);
    let InputSource::Resolved { definition } = &mut snapshot.simulation.spec.inputs else {
        unreachable!()
    };
    definition.oracle.image = Some(seedbox_spec::definition::Image {
        tag: Some("source:tag".into()),
        digest: "image-provenance".into(),
    });
    definition.runtime.execution["semantic_parameters"]["hvf"] = serde_json::json!({"boot_policy":"fixed","entropy_policy":"fixed","device_policy":"fixed","host_release":"before","cpu_features":"before","launch_max_skew_ns":0});
    snapshot.simulation.spec = SimulationSpec::resolved(definition.as_ref().clone());
    let baseline = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    for change in 0..6 {
        let mut changed = snapshot.clone();
        let InputSource::Resolved { definition } = &mut changed.simulation.spec.inputs else {
            unreachable!()
        };
        match change {
            0 => {
                definition.oracle.image = Some(seedbox_spec::definition::Image {
                    tag: Some("other:tag".into()),
                    digest: "other image provenance".into(),
                });
                for key in ["host_release", "cpu_features"] {
                    definition.runtime.execution["semantic_parameters"]["hvf"][key] =
                        "after".into();
                }
            }
            1 => definition.runtime.execution["semantic_parameters"]["deferred_tx"] = true.into(),
            2 => {
                definition.runtime.execution["semantic_parameters"]["hvf"]["entropy_policy"] =
                    "changed".into()
            }
            3 => {
                definition.oracle.boot_archive = seedbox_spec::definition::Asset(format!(
                    "sha256:{}",
                    store.put(b"different guest bytes").unwrap().hex()
                ))
            }
            4 => definition.architecture = seedbox_spec::definition::Architecture::Aarch64,
            5 => definition.runtime.execution["backend"] = "different-backend".into(),
            _ => unreachable!(),
        }
        changed.simulation.spec = SimulationSpec::resolved(definition.as_ref().clone());
        let changed = execution
            .import_bundle(encode(&changed, &store).as_slice(), &Options::default())
            .unwrap();
        assert_eq!(
            execution
                .compare(&baseline.metadata.id, &changed.metadata.id)
                .unwrap()
                .status,
            if change == 0 {
                ComparisonStatus::Match
            } else {
                ComparisonStatus::NotComparable
            },
            "change {change}"
        );
    }
}

#[test]
fn prepared_reference_to_imported_checkpoint_starts_fresh_before_override_validation() {
    let directory = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(directory.path());
    let mut snapshot = fixture(&store, true);
    let mut definition = snapshot.checkpoints["capture"].clone();
    let action = serde_json::json!({"action":"pause_process","machine":"oracle"});
    definition.overrides = vec![seedbox_spec::definition::Override {
        id: "consumed".into(),
        at_ns: 10,
        action: action.clone(),
        timeout_ns: 500,
    }];
    let seedbox_spec::definition::Start::Checkpoint { manifest, .. } = &mut definition.start else {
        unreachable!()
    };
    manifest["state"]["world"]["input_program"] = serde_json::json!({
        "inputs":[{"key":"consumed","at_ns":10,"action":action,"timeout_ns":500}],
        "next":1,"failed":false,"rejections":[],
        "admissions":[{"key":"consumed","plan":{"id":1,"at_ns":10,"deadline_ns":510,"action":"pause_process","machine":"oracle"}}]
    });
    snapshot.simulation.spec = SimulationSpec::resolved(definition);
    let imported = execution
        .import_bundle(encode(&snapshot, &store).as_slice(), &Options::default())
        .unwrap();
    let mut retained = support::resolved_request("checkpoint-retained");
    retained.spec = imported.spec.clone();
    retained.spec.inputs = InputSource::Retained {
        simulation: imported.metadata.id.clone(),
    };
    let retained = manager.submit(retained).unwrap().0;
    assert!(
        matches!(retained.spec.inputs, InputSource::Resolved { definition } if matches!(definition.start, seedbox_spec::definition::Start::Checkpoint { .. }))
    );
    let mut prepared = support::resolved_request("checkpoint-prepared-fresh");
    prepared.spec = imported.spec.clone();
    prepared.spec.inputs = InputSource::Prepared {
        simulation: imported.metadata.id.clone(),
    };
    prepared.spec.seed = Some(456);
    prepared.spec.program.clear(); // Removing consumed overrides is allowed for a fresh start.
    let prepared = manager.submit(prepared).unwrap().0;
    let InputSource::Resolved { definition } = prepared.spec.inputs else {
        unreachable!()
    };
    assert!(matches!(
        definition.start,
        seedbox_spec::definition::Start::Fresh
    ));
    assert!(definition.overrides.is_empty());
    assert_eq!(definition.seed, 456);
    assert!(prepared.metadata.parent.is_none());
    execution.delete(&imported.metadata.id).unwrap();
    assert!(manager.describe(&prepared.metadata.id).is_ok());
}

#[test]
fn deleting_imports_reclaims_only_binaries_they_brought_and_branches_still_need() {
    let source = tempfile::tempdir().unwrap();
    let store = ContentStore::new(source.path());
    let bytes = encode(&fixture(&store, true), &store);
    let destination = tempfile::tempdir().unwrap();
    let (manager, execution, store) = service(destination.path());
    // The kernel and boot archive are already cache inputs here.
    store.put(b"catalogue-only-binary").unwrap();
    let objects = || -> std::collections::BTreeSet<_> {
        std::fs::read_dir(destination.path().join("cache/sha256"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect()
    };
    let baseline = objects();
    let ram = seedbox_artifacts::store::ContentDigest::of(b"retained checkpoint RAM");
    let imported = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap()
        .metadata
        .id;
    assert_eq!(store.owned_objects(), [ram.clone()].into());
    let resource = execution
        .checkpoint_definition(&imported, "capture")
        .unwrap();
    let mut request = support::resolved_request("imported-branch");
    request.spec = resource.spec;
    let branch = manager.submit(request).unwrap().0.metadata.id;
    manager.begin(&branch).unwrap();
    manager
        .update(&branch, |s| s.status.phase = Phase::Completed)
        .unwrap();
    execution.delete(&imported).unwrap();
    store.verify(&ram).unwrap();
    execution.delete(&branch).unwrap();
    assert_eq!(objects(), baseline);
    // A second import of the same bundle owns its binaries again.
    let again = execution
        .import_bundle(bytes.as_slice(), &Options::default())
        .unwrap()
        .metadata
        .id;
    execution.delete_checkpoint(&again, "capture").unwrap();
    assert_eq!(objects(), baseline);
}
