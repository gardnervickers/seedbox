#![cfg(unix)]

mod support;
use futures_util::StreamExt;

use seedbox_api::{ApiVersion, ErrorCode};
use seedbox_client::{Client, Error};
use seedbox_server::{
    catalogue::Catalogue,
    http::{router, ApiState},
};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

#[tokio::test]
async fn unix_client_submits_describes_and_retries_without_duplicate_work() {
    let temp = tempfile::tempdir().unwrap();
    let catalogue = Arc::new(seedbox_server::manager::Manager::new(
        Catalogue::open(&temp.path().join("catalogue")).unwrap(),
    ));
    let (work, mut pending) = mpsc::channel(4);
    let socket = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(ApiState::new(catalogue, work)))
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .unwrap();
    });
    let client = Client::connect_to(&socket).unwrap();
    assert_eq!(client.info().await.unwrap().api_version, ApiVersion::V1);
    let request = support::request();
    let simulation = client.submit(&request).await.unwrap();
    assert_eq!(pending.recv().await.unwrap(), simulation.metadata.id);
    assert_eq!(
        client.describe(&simulation.metadata.id).await.unwrap(),
        simulation
    );
    assert_eq!(client.submit(&request).await.unwrap(), simulation);
    assert!(pending.try_recv().is_err());
    let page = client.list(None).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].metadata.id, simulation.metadata.id);
    assert!(page.next_cursor.is_none());
    let world = client.world(&simulation.metadata.id).await.unwrap();
    assert_eq!(world.simulation, simulation.metadata.id);
    assert_eq!(
        world
            .machines
            .iter()
            .map(|m| m.name.as_str())
            .collect::<Vec<_>>(),
        ["oracle"]
    );
    assert!(world.machines[0].oracle);
    assert!(world.links.is_empty() && world.faults.is_empty());
    let catalogue = client.fault_types(&simulation.metadata.id).await.unwrap();
    assert_eq!(catalogue.simulation, simulation.metadata.id);
    assert_eq!(catalogue.faults.len(), 15);
    let stop = catalogue.faults.iter().find(|f| f.name == "stop").unwrap();
    assert_eq!(
        stop.schema["properties"]["machine"]["enum"],
        serde_json::json!(["oracle"])
    );
    assert_eq!(stop.schema["properties"]["action"]["const"], "stop");
    assert_eq!(stop.schema["additionalProperties"], false);
    assert_eq!(
        stop.schema["required"],
        serde_json::json!(["action", "machine"])
    );
    let shape = catalogue.faults.iter().find(|f| f.name == "shape").unwrap();
    assert_eq!(
        shape.schema["properties"]["from"]["items"]["enum"],
        serde_json::json!([])
    );
    assert_eq!(
        shape.schema["properties"]["loss_ppm"]["maximum"],
        seedbox_control::MAX_PPM
    );
    assert_eq!(shape.schema["properties"]["loss_ppm"]["x-unit"], "ppm");
    assert_eq!(stop.category, seedbox_api::FaultCategory::Application);
    let preview = client
        .preview_fault(
            &simulation.metadata.id,
            &serde_json::json!({"action":"stop","machine":"oracle"}),
        )
        .await
        .unwrap();
    assert_eq!(preview.machines, ["oracle"]);
    assert_eq!(preview.revision, world.revision);
    assert_eq!(preview.category, seedbox_api::FaultCategory::Application);
    assert!(preview
        .limitations
        .iter()
        .any(|text| text.contains("not guaranteed")));
    assert_eq!(client.world(&simulation.metadata.id).await.unwrap(), world);
    assert_eq!(
        client.describe(&simulation.metadata.id).await.unwrap(),
        simulation
    );
    assert!(pending.try_recv().is_err());
    assert!(matches!(
        client
            .preview_fault(
                &simulation.metadata.id,
                &serde_json::json!({"action":"stop","machine":"missing"})
            )
            .await,
        Err(Error::Rejected {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    let unknown = "zzzzzzzz".parse().unwrap();
    assert!(matches!(
        client.fault_types(&unknown).await,
        Err(Error::Rejected {
            code: ErrorCode::NotFound,
            ..
        })
    ));

    let mut conflicting = request;
    conflicting.spec.seed = Some(999);
    assert!(matches!(
        client.submit(&conflicting).await,
        Err(Error::Rejected {
            code: ErrorCode::Conflict,
            ..
        })
    ));

    let mut watch = client.watch(&simulation.metadata.id).await.unwrap();
    assert_eq!(watch.next().await.unwrap().unwrap(), simulation);
    let mut events = client.events(&simulation.metadata.id).await.unwrap();
    let mut all_logs = client.all_logs(&simulation.metadata.id).await.unwrap();
    let mut logs = client
        .logs(&simulation.metadata.id, "oracle")
        .await
        .unwrap();
    assert!(matches!(
        client.logs(&simulation.metadata.id, "missing/other").await,
        Err(Error::Rejected {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    assert!(matches!(
        client.playback(&simulation.metadata.id).await,
        Err(Error::Rejected {
            code: ErrorCode::Unavailable,
            ..
        })
    ));
    let stopped = client.stop(&simulation.metadata.id).await.unwrap();
    assert_eq!(stopped.status.phase, seedbox_api::Phase::Completed);
    assert_eq!(
        stopped.status.verdict(),
        seedbox_api::TestVerdict::Incomplete
    );
    assert_eq!(watch.next().await.unwrap().unwrap(), stopped);
    assert!(watch.next().await.is_none());
    assert_eq!(client.stop(&simulation.metadata.id).await.unwrap(), stopped);
    assert_eq!(
        logs.next().await.unwrap().unwrap(),
        seedbox_api::LogDelivery::Closed
    );
    assert!(logs.next().await.is_none());
    assert_eq!(
        all_logs.next().await.unwrap().unwrap(),
        seedbox_api::LogDelivery::Closed
    );
    assert!(all_logs.next().await.is_none());

    let mut observed = Vec::new();
    while let Some(delivery) = events.next().await {
        match delivery.unwrap() {
            seedbox_api::EventDelivery::Event { observation, .. } => {
                assert_eq!(observation.revision, stopped.metadata.revision);
                assert_eq!(observation.simulation, simulation.metadata.id);
                assert_eq!(observation.simulated_time_ns, None);
                assert_eq!(observation.position, None);
                observed.push(observation.event);
            }
            seedbox_api::EventDelivery::Closed => break,
            seedbox_api::EventDelivery::Gap { .. } => panic!("unexpected gap"),
        }
    }
    assert_eq!(
        observed,
        vec![
            seedbox_api::EventKind::ExecutionTerminated {
                termination: seedbox_api::ExecutionTermination::Stopped
            },
            seedbox_api::EventKind::LifecycleChanged {
                phase: seedbox_api::Phase::Completed,
                stage: None
            },
            seedbox_api::EventKind::StopRequested,
        ]
    );
    assert!(events.next().await.is_none());
    // A completed simulation keeps its event history: a late reader replays
    // it in order, then sees Closed.
    let mut late = client.events(&simulation.metadata.id).await.unwrap();
    let mut replayed = Vec::new();
    loop {
        match late.next().await.unwrap().unwrap() {
            seedbox_api::EventDelivery::Event { observation, .. } => {
                replayed.push(observation.event)
            }
            seedbox_api::EventDelivery::Closed => break,
            other => panic!("unexpected delivery {other:?}"),
        }
    }
    assert!(replayed.ends_with(&observed));
    assert!(late.next().await.is_none());

    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn status_refreshes_elapsed_time_and_preserves_failed_preparation_timing() {
    use seedbox_api::{ExecutionTermination, Phase, Stage, TestVerdict};
    use seedbox_server::manager::Manager;
    let temp = tempfile::tempdir().unwrap();
    let manager = Arc::new(Manager::new(
        Catalogue::open(&temp.path().join("catalogue")).unwrap(),
    ));
    let (work, mut pending) = mpsc::channel(4);
    let socket = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = oneshot::channel();
    let router = router(ApiState::new(manager.clone(), work));
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .unwrap();
    });
    let client = Client::connect_to(&socket).unwrap();
    let accepted = client.submit(&support::request()).await.unwrap();
    let id = pending.recv().await.unwrap();
    assert_eq!(id, accepted.metadata.id);
    let (preparing, _) = manager.begin(&id).unwrap().unwrap();
    let mut watch = client.watch(&id).await.unwrap();
    let first = watch.next().await.unwrap().unwrap();
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), watch.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.metadata.revision, preparing.metadata.revision);
    assert_eq!(second.metadata.revision, preparing.metadata.revision);
    assert!(
        second.status.stage_timings[&Stage::Preparing].elapsed_wall_ns
            > first.status.stage_timings[&Stage::Preparing].elapsed_wall_ns
    );
    assert!(!second.status.stage_timings[&Stage::Preparing].complete);
    let listed = client.list(None).await.unwrap();
    assert!(
        listed.items[0].status.stage_timings[&Stage::Preparing].elapsed_wall_ns
            >= second.status.stage_timings[&Stage::Preparing].elapsed_wall_ns
    );
    // Reads do not turn a ticking display duration into catalogue writes.
    assert_eq!(manager.read(|c| c.get(&id).cloned()).unwrap(), preparing);
    manager
        .update(&id, |s| {
            s.status.phase = Phase::Completed;
            s.status.stage = None;
            s.status.termination = Some(ExecutionTermination::Error {
                message: "preparation failed".into(),
            });
        })
        .unwrap();
    let finished = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let resource = watch.next().await.unwrap().unwrap();
            if resource.status.phase == Phase::Completed {
                break resource;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(finished.status.verdict(), TestVerdict::Incomplete);
    assert!(finished.status.stage_timings[&Stage::Preparing].complete);
    assert!(
        finished.status.stage_timings[&Stage::Preparing].elapsed_wall_ns
            >= second.status.stage_timings[&Stage::Preparing].elapsed_wall_ns
    );
    assert_eq!(client.describe(&id).await.unwrap(), finished);
    assert!(watch.next().await.is_none());
    shutdown.send(()).unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn timeline_reads_owned_journal_in_admission_order_and_retained_inputs() {
    use seedbox_application::{
        execution::{
            CancellationToken, ExecutionBackend, ExecutionProfile, PreparedInputs,
            SemanticParameters,
        },
        store::ContentStore,
    };
    use seedbox_server::execution::{BackendProvider, Backends, ExecutionService};
    struct UnusedProvider;
    impl BackendProvider for UnusedProvider {
        fn project(
            &self,
            _: &seedbox_application::config::Simulation,
            _: &ContentStore,
            _: &CancellationToken,
        ) -> seedbox_application::Result<Backends> {
            panic!("reading a timeline must not prepare inputs")
        }
        fn retained(
            &self,
            _: &PreparedInputs,
            _: &ContentStore,
        ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
            panic!("reading a timeline must not launch execution")
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let manager = Arc::new(seedbox_server::manager::Manager::new(Catalogue::in_memory()));
    let mut request = support::resolved_request("timeline-source");
    request.spec.program = vec![seedbox_api::ProgramInput {
        timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        key: "authored-1".into(),
        at_ns: 100_000_000,
        action: serde_json::json!({"action":"pause","machine":"oracle"}),
    }];
    let resource = manager.submit(request).unwrap().0;
    let id = resource.metadata.id.clone();
    let root = temp.path().join("execution");
    let store = ContentStore::new(temp.path().join("store"));
    let execution = Arc::new(
        ExecutionService::new(
            manager.clone(),
            root.clone(),
            store.clone(),
            Arc::new(UnusedProvider),
        )
        .unwrap(),
    );
    let (work, mut pending) = mpsc::channel(4);
    let retained_manager = manager.clone();
    let socket = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router(ApiState::new(manager, work).with_execution(execution)),
        )
        .with_graceful_shutdown(async {
            let _ = done.await;
        })
        .await
        .unwrap();
    });
    let client = Client::connect_to(&socket).unwrap();
    let empty = client.timeline(&id).await.unwrap();
    assert_eq!(empty.simulation, id);
    assert_eq!(empty.entries.len(), 1);
    assert_eq!(empty.entries[0].at_ns, 100_000_000);
    assert_eq!(empty.entries[0].action, resource.spec.program[0].action);
    let missing = "zzzzzzzz".parse().unwrap();
    assert!(matches!(
        client.timeline(&missing).await,
        Err(Error::Rejected {
            code: ErrorCode::NotFound,
            ..
        })
    ));
    for phase in [seedbox_api::Phase::Preparing, seedbox_api::Phase::Running] {
        retained_manager
            .update(&id, |resource| resource.status.phase = phase)
            .unwrap();
    }
    let admitted = retained_manager.describe(&id).unwrap();
    let history: seedbox_engine_protocol::intervention::History = serde_json::from_value(serde_json::json!({
        "version": 1,
        "entries": [
            {"requestId":"z-first", "position":{"boundaryIndex":2,"completedRounds":1,"simulatedTimeNs":100},
             "plan":{"id":2,"at_ns":100,"deadline_ns":100,"action":"stop","machine":"oracle"}},
            {"requestId":"a-second", "position":{"boundaryIndex":3,"completedRounds":2,"simulatedTimeNs":200},
             "plan":{"id":4,"at_ns":200,"deadline_ns":200,"action":"restart","machine":"oracle"}}
        ]
    })).unwrap();
    let digest = store.put(&history.encode().unwrap()).unwrap();
    let inputs = PreparedInputs {
        machine_memory_mb: Default::default(),
        wall_clock_epoch: None,
        kernel: digest.clone(),
        images: Default::default(),
        placement: Default::default(),
        guests: Default::default(),
        auxiliary: Default::default(),
        provenance: Default::default(),
        profile: ExecutionProfile {
            backend: seedbox_application::execution::Backend::Other("test".into()),
            engine: digest.clone(),
            witness_mode: seedbox_application::execution::WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(512),
        },
    };
    let descriptor = root.join("inputs").join(format!("{id}.json"));
    std::fs::write(
        &descriptor,
        serde_json::to_vec(&serde_json::json!({
            "checkpoint": null, "prepared": inputs, "interventions": digest, "attempt": null,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(client.timeline(&id).await.unwrap().entries.len(), 3);
    assert_eq!(
        client.timeline(&id).await.unwrap().entries[0].id,
        "program:z-first"
    );
    let attempt = root.join("attempt");
    std::fs::create_dir(&attempt).unwrap();
    let journal = attempt.join("interventions.json");
    let mut writer = seedbox_artifacts::interventions::Journal::initialize(&journal).unwrap();
    writer.append(history.entries[0].clone()).unwrap();
    std::fs::write(
        &descriptor,
        serde_json::to_vec(&serde_json::json!({
            "checkpoint": null, "prepared": inputs, "interventions": digest, "attempt": attempt,
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        client.timeline(&id).await.unwrap().entries[0].position,
        Some(history.entries[0].position)
    );
    writer.append(history.entries[1].clone()).unwrap();
    assert_eq!(client.timeline(&id).await.unwrap().entries.len(), 3);
    assert_eq!(
        client.timeline(&id).await.unwrap().entries[0].id,
        "program:z-first"
    );
    let observed = client.describe(&id).await.unwrap();
    assert_eq!(observed.metadata.id, resource.metadata.id);
    assert_eq!(
        observed
            .spec
            .program
            .iter()
            .map(|input| input.key.as_str())
            .collect::<Vec<_>>(),
        vec!["z-first", "a-second", "authored-1"]
    );
    assert_eq!(observed.metadata.revision, admitted.metadata.revision);
    assert_eq!(retained_manager.describe(&id).unwrap(), admitted);
    assert_eq!(resource.spec.program.len(), 1);
    retained_manager
        .update(&id, |resource| {
            resource.status.phase = seedbox_api::Phase::Completed
        })
        .unwrap();
    let completed = retained_manager.describe(&id).unwrap();
    assert_eq!(client.timeline(&id).await.unwrap().entries.len(), 3);
    let exported = client.describe(&id).await.unwrap();
    assert_eq!(exported.spec.program, observed.spec.program);
    assert_eq!(retained_manager.describe(&id).unwrap(), completed);
    let resubmitted = client
        .submit(&seedbox_api::SubmitRequest {
            metadata: Default::default(),
            options: Default::default(),
            request_id: "recorded-definition".into(),
            spec: exported.spec,
        })
        .await
        .unwrap();
    assert_eq!(pending.recv().await.unwrap(), resubmitted.metadata.id);
    assert_eq!(resubmitted.spec.program, observed.spec.program);
    let mut selection = seedbox_api::DeriveRequest {
        request_id: "derive-selected".into(),
        source_created_at_unix_ms: resource.metadata.created_at_unix_ms + 1,
        included: vec!["program:z-first".into()],
        start_paused: true,
    };
    assert!(matches!(
        client.derive(&id, &selection).await,
        Err(Error::Rejected {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    selection.source_created_at_unix_ms -= 1;
    selection.included.push("missing-entry".into());
    assert!(matches!(
        client.derive(&id, &selection).await,
        Err(Error::Rejected {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
    selection.included.pop();
    let derived = client.derive(&id, &selection).await.unwrap();
    assert_eq!(pending.recv().await.unwrap(), derived.metadata.id);
    assert!(derived.options.start_paused);
    assert_eq!(derived.spec.seed, resource.spec.seed);
    assert_eq!(derived.spec.configuration, resource.spec.configuration);
    assert!(matches!(
        derived.spec.inputs,
        seedbox_api::InputSource::Resolved { .. }
    ));
    let generated = serde_json::to_value(&derived.spec).unwrap();
    assert!(generated.get("inputs").is_none() && generated.get("configuration").is_none());
    assert_eq!(
        derived.spec.program,
        vec![seedbox_api::ProgramInput {
            timeout_ns: 0,
            key: "z-first".into(),
            at_ns: 100,
            action: serde_json::json!({"action":"stop","machine":"oracle"}),
        }]
    );
    assert_eq!(client.derive(&id, &selection).await.unwrap(), derived);
    assert!(pending.try_recv().is_err());
    std::fs::remove_file(&journal).unwrap();
    assert!(matches!(
        client.timeline(&id).await,
        Err(Error::Rejected {
            code: ErrorCode::Internal,
            ..
        })
    ));
    let _ = shutdown.send(());
    server.await.unwrap();
}

#[tokio::test]
async fn native_acceptance_submissions_round_trip_over_unix_http() {
    let temp = tempfile::tempdir().unwrap();
    let manager = Arc::new(seedbox_server::manager::Manager::new(
        Catalogue::open(&temp.path().join("catalogue")).unwrap(),
    ));
    let (work, mut pending) = mpsc::channel(4);
    let socket = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (shutdown, done) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(ApiState::new(manager, work)))
            .with_graceful_shutdown(async {
                let _ = done.await;
            })
            .await
            .unwrap();
    });
    let client = Client::connect_to(&socket).unwrap();
    for name in [
        "connections",
        "key-value",
        "mac-capabilities",
        "race",
        "runtime",
        "triggers",
        "volume-crash",
    ] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../harness/acceptance/workloads/{name}.yaml"))
            .canonicalize()
            .unwrap();
        let mut request = support::request();
        request.request_id = format!("native-{name}");
        request.spec.inputs = seedbox_api::InputSource::Project;
        request.spec.configuration = seedbox_application::config::load_for_architecture(
            &path,
            &Default::default(),
            seedbox_application::guest::Architecture::Amd64,
        )
        .unwrap();
        let simulation = client
            .submit(&request)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(pending.recv().await.unwrap(), simulation.metadata.id);
        assert_eq!(
            serde_json::to_value(&simulation.spec).unwrap(),
            serde_json::to_value(&request.spec).unwrap(),
            "{name}"
        );
        assert_eq!(
            client.describe(&simulation.metadata.id).await.unwrap(),
            simulation
        );
    }
    shutdown.send(()).unwrap();
    server.await.unwrap();
}
