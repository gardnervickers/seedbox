//! Public service checkpoint round-trip using a HOST-ONLY counter executor.
//! The fixture speaks the real control protocol and persists/restores its own
//! bytes. It is not a native snapshot, guest, subprocess, or determinism test;
//! no witness is synthesized. Native checkpoint codecs remain backend-owned.
#![cfg(unix)]

mod support;

use futures_util::StreamExt;
use seedbox_api::{ControlCommand, ControlRequest, ControlState, ExecutionTermination, Phase};
use seedbox_application::{
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparedInputs, StopReason,
    },
    store::{ContentDigest, ContentStore},
};
use seedbox_client::Client;
use seedbox_engine_protocol::host_control::{
    BoundaryPosition, CheckpointState, CheckpointStatus, Command, PlaybackRate, Reply, Request,
    MAX_DATAGRAM,
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
};
use std::{
    fs,
    num::NonZeroUsize,
    os::unix::net::UnixDatagram,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;

#[derive(Default)]
struct State {
    ready: Notify,
    calls: AtomicUsize,
    captured: Mutex<Option<PathBuf>>,
    restored: Mutex<Option<u64>>,
}
struct Provider(Arc<State>);
impl BackendProvider for Provider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: seedbox_spec::backend::CpuBackend,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved host fixture must never prepare images")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        Ok(Box::new(Counter {
            profile: inputs.profile.clone(),
            state: self.0.clone(),
        }))
    }
}
struct Counter {
    profile: ExecutionProfile,
    state: Arc<State>,
}
impl ExecutionBackend for Counter {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn execute(
        &self,
        request: ExecutionRequest<'_>,
    ) -> seedbox_application::Result<ExecutionReport> {
        self.state.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(checkpoint) = request.resume_checkpoint {
            let manifest: serde_json::Value = serde_json::from_reader(fs::File::open(checkpoint)?)?;
            assert_eq!(manifest["fixture_format"], "host-counter-v1");
            let machine = &manifest["state"]["world"]["machines"][0];
            let path = PathBuf::from(machine["path"].as_str().unwrap());
            let original = self.state.captured.lock().unwrap().clone().unwrap();
            assert_ne!(path, original);
            assert!(
                !original.exists(),
                "restore must not depend on the removed capture"
            );
            let bytes = fs::read(&path)?;
            assert_eq!(
                ContentDigest::of(&bytes).hex(),
                machine["sha256"].as_str().unwrap()
            );
            let counter =
                u64::from_le_bytes(bytes.try_into().expect("counter checkpoint is eight bytes"));
            assert_eq!(counter, 7);
            let continued = counter + 1;
            *self.state.restored.lock().unwrap() = Some(continued);
            return Ok(report(StopReason::Quiescent, continued));
        }
        // This fixture owns a real stopped counter state at position seven.
        // Its storage envelope follows the server's artifact ownership format;
        // the binary payload is explicitly NOT any native machine snapshot.
        let endpoint = tempfile::tempdir()?;
        let socket_path = endpoint.path().join("control");
        let socket = UnixDatagram::bind(&socket_path)?;
        socket.set_nonblocking(true)?;
        fs::write(
            request.output.join("host-control"),
            socket_path.as_os_str().as_encoded_bytes(),
        )?;
        self.state.ready.notify_one();
        while !request.cancellation.is_cancelled() {
            let mut buffer = [0; MAX_DATAGRAM + 1];
            let received = seedbox_test_time::HangGuard::new(
                "wait for the fixture cancellation or socket signal",
                Duration::from_secs(60),
            )
            .wait_for(|| {
                if request.cancellation.is_cancelled() {
                    return Some(Ok(None));
                }
                match socket.recv_from(&mut buffer) {
                    Ok(value) => Some(Ok(Some(value))),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
                    Err(error) => Some(Err(error)),
                }
            })
            .unwrap()?;
            let Some((length, peer)) = received else {
                break;
            };
            let control: Request = serde_json::from_slice(&buffer[..length])?;
            let position = BoundaryPosition {
                boundary_index: 7,
                completed_rounds: 7,
                simulated_time_ns: 7,
            };
            let checkpoint = match control.command {
                Command::Status => None,
                Command::Checkpoint { request_id } => {
                    assert_eq!(request_id, "saved");
                    let output = request
                        .output
                        .join(seedbox_application::execution::MACHINE_OUTPUT_DIR);
                    let state = output.join(".checkpoint-machines-saved/0.snapshot");
                    fs::create_dir_all(state.parent().unwrap())?;
                    let bytes = 7u64.to_le_bytes();
                    fs::write(&state, bytes)?;
                    let manifest = serde_json::json!({
                        "fixture_format": "host-counter-v1",
                        "state": {"world": {
                            "input_program": {"inputs": [], "next": 0},
                            "machines": [{"path": state, "sha256": ContentDigest::of(&bytes).hex()}]
                        }}
                    });
                    fs::write(
                        output.join("checkpoint-saved.json"),
                        serde_json::to_vec(&manifest)?,
                    )?;
                    *self.state.captured.lock().unwrap() = Some(state);
                    Some(CheckpointStatus {
                        request_id,
                        position,
                        state: CheckpointState::Captured,
                    })
                }
                command => panic!("unexpected fixture command: {command:?}"),
            };
            let reply = Reply {
                id: control.id,
                position,
                terminal: false,
                application_ready: true,
                rate: PlaybackRate::Paused,
                achieved_rate: None,
                stepping: false,
                checkpoint,
                error: None,
                intervention: None,
                detail: None,
            };
            socket.send_to(&serde_json::to_vec(&reply)?, peer.as_pathname().unwrap())?;
        }
        Ok(report(StopReason::Cancelled, 7))
    }
}
fn report(reason: StopReason, simulated_time_ns: u64) -> ExecutionReport {
    ExecutionReport {
        reason,
        simulated_time_ns: Some(simulated_time_ns),
        workload_exit: None,
        witness: None,
        full_state: None,
        faults: Vec::new(),
        host_wait_ns: 0,
        detail: None,
    }
}
struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

async fn completed(client: &Client, id: &seedbox_api::SimulationId) -> seedbox_api::Simulation {
    let mut watch = client.watch(id).await.unwrap();
    while let Some(resource) = watch.next().await {
        let resource = resource.unwrap();
        if resource.status.phase == Phase::Completed {
            return resource;
        }
    }
    panic!("watch closed before completion")
}

#[tokio::test]
async fn public_checkpoint_export_outlives_capture_and_materializes_into_continuation() {
    tokio::time::timeout(Duration::from_secs(60), checkpoint_roundtrip())
        .await
        .expect("host checkpoint protocol round-trip hung");
}
async fn checkpoint_roundtrip() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    store.put(b"catalogue-only-binary").unwrap();
    let manager = Arc::new(Manager::new(Catalogue::in_memory()));
    let state = Arc::new(State::default());
    let service = Arc::new(
        ExecutionService::new(
            manager.clone(),
            directory.path().join("execution"),
            store,
            Arc::new(Provider(state.clone())),
        )
        .unwrap(),
    );
    let socket = directory.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let shutdown = CancellationToken::default();
    let _cancel_on_drop = CancelOnDrop(shutdown.clone());
    let server = tokio::spawn(seedbox_server::serve(
        listener,
        manager,
        service,
        NonZeroUsize::new(1).unwrap(),
        shutdown.clone(),
    ));
    let client = Client::connect_to(&socket).unwrap();
    let source = client
        .submit(&support::resolved_request("checkpoint-host"))
        .await
        .unwrap();
    let id = &source.metadata.id;
    state.ready.notified().await;
    let request = ControlRequest {
        request_id: "saved".into(),
        command: ControlCommand::Checkpoint,
    };
    let accepted = client.request_control(id, &request).await.unwrap();
    assert_eq!(accepted.state, ControlState::Accepted);
    let mut changes = client.events_with(id, true).await.unwrap();
    let captured = loop {
        let receipt = client.control_receipt(id, "saved").await.unwrap();
        if receipt.state != ControlState::Accepted {
            break receipt;
        }
        assert!(matches!(
            changes.next().await.unwrap().unwrap(),
            seedbox_api::EventDelivery::Event { .. }
        ));
    };
    assert!(matches!(&captured.state, ControlState::Acknowledged {
        checkpoint: Some(checkpoint), error: None, ..
    } if checkpoint.state == CheckpointState::Captured && checkpoint.position.completed_rounds == 7));
    assert_eq!(
        client.request_control(id, &request).await.unwrap(),
        captured
    );
    assert_eq!(client.checkpoint(id, "saved").await.unwrap(), captured);
    client.stop(id).await.unwrap();
    assert_eq!(
        completed(&client, id).await.status.termination,
        Some(ExecutionTermination::Stopped)
    );
    let exported = client.checkpoint_definition(id, "saved").await.unwrap();
    let seedbox_api::InputSource::Resolved { definition } = &exported.spec.inputs else {
        panic!("expected resolved export")
    };
    let seedbox_spec::definition::Start::Checkpoint { manifest, assets } = &definition.start else {
        panic!("expected checkpoint export")
    };
    assert_eq!(assets.len(), 1);
    assert_eq!(manifest["fixture_format"], "host-counter-v1");
    assert_eq!(
        manifest["state"]["world"]["machines"][0]["path"],
        "asset:state-1"
    );
    let original = state.captured.lock().unwrap().clone().unwrap();
    assert!(original.is_file());
    client.delete_checkpoint(id, "saved").await.unwrap();
    assert!(
        !original.exists(),
        "public deletion must reclaim owned checkpoint bytes"
    );
    assert!(client.checkpoints(id).await.unwrap().is_empty());
    client.delete(id).await.unwrap();
    let restored = client
        .submit_definition(&serde_json::to_value(exported).unwrap(), false)
        .await
        .unwrap();
    let restored = completed(&client, &restored.metadata.id).await;
    assert_eq!(
        restored.status.termination,
        Some(ExecutionTermination::Quiescent)
    );
    assert_eq!(restored.status.simulated_time_ns, 8);
    assert_eq!(*state.restored.lock().unwrap(), Some(8));
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        restored.status.verdict(),
        seedbox_api::TestVerdict::Incomplete
    );
    client.delete(&restored.metadata.id).await.unwrap();
    shutdown.cancel();
    server.await.unwrap().unwrap();
}
