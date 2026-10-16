use super::*;
use futures_util::StreamExt;
use std::time::Duration;

// All elapsed-time limits below are failure-only hang guards. Gate/cancellation
// observations, decoded records and injected clock steps are the success oracles.
#[tokio::test]
async fn dropping_an_unpolled_body_cancels_its_blocking_worker() {
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (observed, finished) = tokio::sync::oneshot::channel();
    let response = response(CancellationToken::default(), move |progress| {
        entered.send(()).unwrap();
        // test-time: allow(hang-guard) -- Explicit release gates this callback; 5s only bounds a stuck/contended test thread, never establishes cancellation by elapsed absence.
        let _ = gate.recv_timeout(Duration::from_secs(5));
        observed.send(progress.advance(64 * 1024).is_err()).unwrap();
        Err(HttpError::internal("gate done"))
    });
    ready.await.unwrap();
    drop(response);
    release.send(()).unwrap();
    // test-time: allow(hang-guard) -- Await the positive callback cancellation observation; retain 5s loaded-runtime scheduling room, with the received boolean deciding success.
    assert!(tokio::time::timeout(Duration::from_secs(5), finished)
        .await
        .unwrap()
        .unwrap());
}

#[tokio::test]
async fn parent_shutdown_stops_export_without_dropping_its_body() {
    let shutdown = CancellationToken::default();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let response = response(shutdown.clone(), move |progress| {
        entered.send(()).unwrap();
        // test-time: allow(hang-guard) -- Explicit release follows parent cancellation; this existing 5s guard only prevents a broken fixture gate wedging the worker.
        let _ = gate.recv_timeout(Duration::from_secs(5));
        assert!(progress.advance(0).is_err());
        Err(HttpError::internal("gate done"))
    });
    ready.await.unwrap();
    shutdown.cancel();
    release.send(()).unwrap();
    // test-time: allow(hang-guard) -- Bound positive body completion after cancellation; decoded error records, not elapsed speed, are the oracle, with 5s scheduling room.
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 4096),
    )
    .await
    .unwrap()
    .unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("checkpoint export cancelled"));
    assert!(text.contains("unavailable"));
    assert!(!text.contains("completed"));
}

#[tokio::test]
async fn progress_backpressure_never_blocks_work_and_panics_are_terminal() {
    let (sent, finished) = tokio::sync::oneshot::channel();
    let response = response(CancellationToken::default(), move |progress| {
        for _ in 0..10_000 {
            progress.advance(64 * 1024).unwrap();
        }
        sent.send(()).unwrap();
        panic!("deliberate export fixture panic")
    });
    // test-time: allow(hang-guard) -- Positive completion of finite 10000-item backpressure work; keep the existing 5s host guard, never an elapsed-performance assertion.
    tokio::time::timeout(Duration::from_secs(5), finished)
        .await
        .unwrap()
        .unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .unwrap();
    let text = std::str::from_utf8(&bytes).unwrap();
    assert!(text.contains("checkpoint export task failed"));
    assert!(text.contains("failed"));
}

#[tokio::test]
async fn public_definition_preserves_structured_errors_and_rejects_truncation() {
    // test-time: allow(hang-guard) -- Outer 5s safety budget for finite local HTTP exchanges; exact structured error/truncation records decide success.
    tokio::time::timeout(Duration::from_secs(5), async {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let router = axum::Router::new().route(
            "/v1/simulations/{id}/checkpoints/{name}/definition",
            axum::routing::get(|axum::extract::Path((_, name)): axum::extract::Path<(String, String)>| async move {
                if name == "failed" {
                    response(CancellationToken::default(), |_| Err(HttpError::new(
                        axum::http::StatusCode::NOT_FOUND, ErrorCode::NotFound, "missing checkpoint binary",
                    )))
                } else {
                    // A valid progress event is not a successful definition.
                    ([ (axum::http::header::CONTENT_TYPE, "text/event-stream") ],
                     "data: {\"type\":\"progress\",\"bytesProcessed\":0}\n\n").into_response()
                }
            }),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).with_graceful_shutdown(async { let _ = stopped.await; }).await.unwrap();
        });
        let client = seedbox_client::Client::connect_to(&socket).unwrap();
        let id = "23456789".parse().unwrap();
        assert!(matches!(client.checkpoint_definition(&id, "failed").await,
            Err(seedbox_client::Error::Rejected { code: ErrorCode::NotFound, message }) if message == "missing checkpoint binary"));
        assert!(matches!(client.checkpoint_definition(&id, "truncated").await,
            Err(seedbox_client::Error::Stream(message)) if message.contains("without its terminal record")));
        stop.send(()).unwrap();
        server.await.unwrap();
    }).await.unwrap();
}

#[tokio::test]
async fn public_export_stream_peer_drop_cancels_work() {
    {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::default();
        let (observed, finished) = tokio::sync::oneshot::channel();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Some(observed)));
        let parent = shutdown.clone();
        let router = axum::Router::new().route(
            "/v1/simulations/{id}/checkpoints/{name}/definition",
            axum::routing::get(move || {
                let parent = parent.clone();
                let observed = observed.clone();
                async move {
                    response(parent, move |progress| {
                        // Await cancellation, not an elapsed-time target. Guard only
                        // prevents a broken cancellation path wedging the test runtime.
                        // test-time: allow(hang-guard) -- Anchor the existing 5s positive peer-disconnect observation guard; injected Tokio time never changes this infrastructure budget.
                        let guard = std::time::Instant::now() + Duration::from_secs(5);
                        while progress.advance(0).is_ok() {
                            // test-time: allow(hang-guard) -- Only fail a wedged cancellation callback after 5s; progress.advance returning cancellation is the positive completion witness.
                            assert!(
                                std::time::Instant::now() < guard,
                                "peer disconnect did not cancel export"
                            );
                            std::thread::yield_now();
                        }
                        observed.lock().unwrap().take().unwrap().send(()).unwrap();
                        Err(HttpError::internal("cancelled"))
                    })
                }
            }),
        );
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let client = seedbox_client::Client::connect_to(&socket).unwrap();
        let id = "23456789".parse().unwrap();
        let mut updates = client.checkpoint_export(&id, "saved").await.unwrap();
        assert!(matches!(
            updates.next().await.unwrap().unwrap(),
            CheckpointExport::Progress { bytes_processed: 0 }
        ));
        // Duration/reset behavior is tested against the shared controlled clock
        // in seedbox-client; this real connection test owns peer-drop cleanup.
        drop(updates);
        // test-time: allow(hang-guard) -- Positive exporter cancellation acknowledgement after peer drop; 5s allows loaded-runtime dispatch, never timed absence.
        tokio::time::timeout(Duration::from_secs(5), finished)
            .await
            .unwrap()
            .unwrap();
        stop.send(()).unwrap();
        // test-time: allow(hang-guard) -- Positive local server join after its explicit stop message; retain the 5s infrastructure cleanup budget.
        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
    }
}
