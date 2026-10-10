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
    let mut late = client.events(&simulation.metadata.id).await.unwrap();
    assert_eq!(
        late.next().await.unwrap().unwrap(),
        seedbox_api::EventDelivery::Closed
    );
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
