//! Server-owned retry driver. HTTP connection lifetime never owns a mutation.
use crate::{catalogue::Error, manager::Manager};
use seedbox_api::{ControlRequest, ControlState, SimulationId};
use seedbox_application::{execution::CancellationToken, host_control::ClientError};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, TryLockError},
    time::Duration,
};
use tokio::task::JoinSet;

pub(crate) async fn run(manager: Arc<Manager>, shutdown: CancellationToken) -> Result<(), Error> {
    let mut tasks = JoinSet::new();
    let mut active = BTreeSet::new();
    let mut inspections = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result = async { loop {
        if shutdown.is_cancelled() && tasks.is_empty() {
            return Ok(());
        }
        tokio::select! {
            result = tasks.join_next(), if !tasks.is_empty() => {
                let (id, result) = result.ok_or(Error::StorageFailed)?.map_err(|_| Error::StorageFailed)?;
                active.remove(&id);
                result?;
            }
            _ = tick.tick() => {
                if shutdown.is_cancelled() { continue; }
                let owner = manager.clone();
                let pending = tokio::task::spawn_blocking(move || owner.control_work())
                    .await.map_err(|_| Error::StorageFailed)??;
                inspections.retain(|id, _| pending.iter().any(|(simulation, _)| simulation == id));
                for (id, request) in pending {
                    if request.is_none() && inspections.get(&id).is_some_and(|at: &std::time::Instant| at.elapsed() < Duration::from_millis(250)) { continue; }
                    if !active.insert(id.clone()) { continue; }
                    inspections.insert(id.clone(), std::time::Instant::now());
                    let owner = manager.clone();
                    tasks.spawn_blocking(move || {
                        let result = drive(&owner, &id, request);
                        (id, result)
                    });
                }
            }
        }
    }    }.await;
    if result.is_err() {
        shutdown.cancel();
    }
    // Blocking transport attempts are bounded, but cannot be aborted by Tokio.
    // Own their full lifetime even when another attempt failed.
    while tasks.join_next().await.is_some() {}
    result
}

fn drive(
    manager: &Manager,
    id: &SimulationId,
    request: Option<ControlRequest>,
) -> Result<(), Error> {
    let Some(worker) = manager.worker(id)? else {
        return Ok(());
    };
    let mut worker = match worker.try_lock() {
        Ok(worker) => worker,
        Err(TryLockError::WouldBlock) => return Ok(()),
        Err(TryLockError::Poisoned(_)) => return Err(Error::StorageFailed),
    };
    if worker.is_retired() {
        return Ok(());
    }
    match drive_worker(manager, id, request, &mut worker) {
        Err(Error::WorkerControl(message)) => {
            // Keep the worker gate until failure is durable. Completion and
            // deletion cannot retire this connection and overtake the error.
            manager.record_worker_control_failure(id, message)?;
            worker.retire();
            Ok(())
        }
        result => result,
    }
}

fn drive_worker(
    manager: &Manager,
    id: &SimulationId,
    request: Option<ControlRequest>,
    worker: &mut crate::worker::Worker,
) -> Result<(), Error> {
    let Some(request) = request else {
        match worker.inspect() {
            Ok(status) => manager.observe_playback(id, status)?,
            Err(error @ (ClientError::Protocol(_) | ClientError::Exhausted)) => {
                return Err(Error::WorkerControl(error.to_string()));
            }
            Err(_) => {}
        }
        return Ok(());
    };
    let result = worker.apply(&request);
    let receipt = match result {
        Ok(receipt) => receipt,
        Err(crate::worker::Error::Retired) => return Ok(()),
        Err(crate::worker::Error::Pending) => return Ok(()),
        Err(crate::worker::Error::Transport(ClientError::Unsupported)) => ControlState::Unsupported,
        // Retain the same connection and wire request after an uncertain result.
        // A later tick retries it; completion/recovery marks it interrupted.
        Err(crate::worker::Error::Transport(
            error @ (ClientError::Protocol(_) | ClientError::Exhausted),
        )) => {
            return Err(Error::WorkerControl(error.to_string()));
        }
        Err(crate::worker::Error::Transport(_)) => return Ok(()),
        Err(crate::worker::Error::Fatal(message)) => return Err(Error::WorkerControl(message)),
    };
    if let ControlState::Acknowledged { status, .. } = &receipt {
        manager.observe_playback(id, status.clone())?;
    }
    manager.acknowledge_control(id, &request.request_id, receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{
        ControlCommand, EventKind, ExecutionTermination, InputSource, Phase, SimulationSpec,
        SubmitRequest, TestVerdict,
    };
    use seedbox_engine_protocol::host_control::{
        BoundaryPosition, PlaybackRate, Reply, Request, MAX_DATAGRAM,
    };
    use std::{
        fs,
        os::unix::net::UnixDatagram,
        sync::atomic::{AtomicBool, Ordering},
    };

    #[tokio::test]
    async fn malformed_worker_reply_cancels_only_its_simulation_and_preserves_oracle() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(
            crate::catalogue::Catalogue::open(&directory.path().join("catalogue")).unwrap(),
        ));
        let shutdown = CancellationToken::default();
        let stop_peers = Arc::new(AtomicBool::new(false));
        let (request_seen, seen) = std::sync::mpsc::channel();
        let (release_reply, reply_gate) = std::sync::mpsc::channel();
        let mut reply_gate = Some(reply_gate);
        let mut peers = Vec::new();
        let mut simulations = Vec::new();
        for (name, malformed) in [("broken", true), ("healthy", false)] {
            let configuration = seedbox_application::config::parse(
                "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
                std::path::Path::new("/project/compose.yaml"),
                &Default::default(),
            )
            .unwrap();
            let resource = manager
                .submit(SubmitRequest {
                    metadata: Default::default(),
                    options: Default::default(),
                    request_id: name.into(),
                    spec: SimulationSpec {
                        program: Vec::new(),
                        start_paused: false,
                        seed: Some(1),
                        inputs: InputSource::Project,
                        configuration,
                    },
                })
                .unwrap()
                .0;
            let id = resource.metadata.id;
            let (_, cancellation) = manager.begin(&id).unwrap().unwrap();
            manager
                .update(&id, |resource| resource.status.phase = Phase::Running)
                .unwrap();
            let run = directory.path().join(name);
            fs::create_dir_all(run.join("output")).unwrap();
            let endpoint = directory.path().join(format!("{name}.sock"));
            let socket = UnixDatagram::bind(&endpoint).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(50)))
                .unwrap();
            fs::write(
                run.join("output/host-control"),
                endpoint.as_os_str().as_encoded_bytes(),
            )
            .unwrap();
            manager.attach_worker(&id, &run).unwrap();
            let stop = stop_peers.clone();
            let request_seen = request_seen.clone();
            let mut gate = if malformed { reply_gate.take() } else { None };
            peers.push(std::thread::spawn(move || {
                let mut buffer = [0; MAX_DATAGRAM + 1];
                while !stop.load(Ordering::Acquire) {
                    let (count, peer) = match socket.recv_from(&mut buffer) {
                        Ok(received) => received,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(error) => panic!("fake worker: {error}"),
                    };
                    let request: Request = serde_json::from_slice(&buffer[..count]).unwrap();
                    let bytes = if malformed {
                        if let Some(gate) = gate.take() {
                            request_seen.send(()).unwrap();
                            gate.recv_timeout(Duration::from_secs(2)).unwrap();
                        }
                        b"malformed reply".to_vec()
                    } else {
                        serde_json::to_vec(&Reply {
                            id: request.id,
                            position: BoundaryPosition::default(),
                            terminal: false,
                            application_ready: false,
                            rate: PlaybackRate::Unlimited,
                            achieved_rate: None,
                            stepping: false,
                            checkpoint: None,
                            error: None,
                            detail: None,
                            intervention: None,
                        })
                        .unwrap()
                    };
                    socket.send_to(&bytes, peer.as_pathname().unwrap()).unwrap();
                }
            }));
            manager
                .accept_control(
                    &id,
                    ControlRequest {
                        request_id: "step".into(),
                        command: ControlCommand::Step,
                    },
                )
                .unwrap();
            simulations.push((id, cancellation));
        }
        let (broken, broken_cancel) = &simulations[0];
        let (healthy, healthy_cancel) = &simulations[1];
        manager.publish_engine_event(
            broken,
            10,
            EventKind::MachineExited {
                machine: "oracle".into(),
                incarnation: 0,
                code: 0,
            },
        );
        let driver = tokio::spawn(run(manager.clone(), shutdown.clone()));
        tokio::task::spawn_blocking(move || seen.recv_timeout(Duration::from_secs(2)).unwrap())
            .await
            .unwrap();
        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let owner = manager.clone();
        let completing = broken.clone();
        let cleanup = std::thread::spawn(move || {
            owner.retire_worker(&completing).unwrap();
            retired_tx.send(()).unwrap();
            finish_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            owner
                .update(&completing, |resource| {
                    resource.status.phase = Phase::Completed;
                    resource.status.termination = Some(ExecutionTermination::Stopped);
                })
                .unwrap();
        });
        // Completion is trying to retire the very connection awaiting this reply.
        assert!(retired_rx.recv_timeout(Duration::from_millis(30)).is_err());
        release_reply.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let failed = manager.describe(broken).unwrap();
                let acknowledged = manager
                    .read(|catalogue| {
                        Ok(matches!(
                            catalogue
                                .control_receipt(healthy, "step")?
                                .map(|receipt| &receipt.state),
                            Some(ControlState::Acknowledged { .. })
                        ))
                    })
                    .unwrap();
                if matches!(
                    failed.status.termination,
                    Some(ExecutionTermination::Error { .. })
                ) && acknowledged
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(broken_cancel.is_cancelled());
        assert!(!healthy_cancel.is_cancelled());
        assert!(!shutdown.is_cancelled());
        let failed = manager.describe(broken).unwrap();
        assert_eq!(failed.status.phase, Phase::Running);
        assert!(!failed.status.stop_requested);
        assert_eq!(failed.status.verdict(), TestVerdict::Passed);
        assert!(!manager
            .control_work()
            .unwrap()
            .iter()
            .any(|(id, _)| id == broken));
        // Execution retains ownership through cleanup and completion. Its ordinary
        // cancellation reason must not overwrite the observed operational error.
        retired_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        // Retirement cannot finish until the error was durably recorded under
        // the same worker gate, even if execution cleanup raced the reply.
        assert_eq!(
            manager.describe(broken).unwrap().status.termination,
            failed.status.termination
        );
        finish_tx.send(()).unwrap();
        cleanup.join().unwrap();
        let completed = manager.describe(broken).unwrap();
        assert_eq!(completed.status.termination, failed.status.termination);
        assert_eq!(completed.status.verdict(), TestVerdict::Passed);
        assert_eq!(
            manager.describe(healthy).unwrap().status.phase,
            Phase::Running
        );
        shutdown.cancel();
        driver.await.unwrap().unwrap();
        stop_peers.store(true, Ordering::Release);
        for peer in peers {
            peer.join().unwrap();
        }
    }

    /// NEXT-100 case 96: a status read times out (its reply is withheld), and a
    /// fault, a checkpoint and a rate change are then requested. No mutation may
    /// reach the worker before the uncertain status resolves; a second command
    /// is refused while one is pending instead of being queued behind it; each
    /// command gets one receipt and reaches the worker in request order.
    #[tokio::test]
    async fn controls_wait_for_an_uncertain_status_and_never_overtake() {
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(
            crate::catalogue::Catalogue::open(&directory.path().join("catalogue")).unwrap(),
        ));
        let shutdown = CancellationToken::default();
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            std::path::Path::new("/project/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let id = manager
            .submit(SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: "ordered".into(),
                spec: SimulationSpec {
                    program: Vec::new(),
                    start_paused: false,
                    seed: Some(1),
                    inputs: InputSource::Project,
                    configuration,
                },
            })
            .unwrap()
            .0
            .metadata
            .id;
        manager.begin(&id).unwrap().unwrap();
        manager
            .update(&id, |resource| resource.status.phase = Phase::Running)
            .unwrap();
        let run_dir = directory.path().join("run");
        fs::create_dir_all(run_dir.join("output")).unwrap();
        let endpoint = directory.path().join("worker.sock");
        let socket = UnixDatagram::bind(&endpoint).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        fs::write(
            run_dir.join("output/host-control"),
            endpoint.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        manager.attach_worker(&id, &run_dir).unwrap();

        // The fake engine withholds every reply until released, then answers
        // each command; it records the first arrival of each wire request.
        let stop = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        // (wire request ID, command kind, sent after release) in arrival order.
        type Seen = Arc<std::sync::Mutex<Vec<(u64, String, bool)>>>;
        let seen: Seen = Default::default();
        let peer = {
            let (stop, released, seen) = (stop.clone(), released.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut buffer = [0; MAX_DATAGRAM + 1];
                let position = BoundaryPosition {
                    boundary_index: 4,
                    completed_rounds: 3,
                    simulated_time_ns: 500,
                };
                let mut rate = PlaybackRate::Unlimited;
                let mut proposal = None;
                while !stop.load(Ordering::Acquire) {
                    let (count, from) = match socket.recv_from(&mut buffer) {
                        Ok(received) => received,
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            continue
                        }
                        Err(error) => panic!("fake worker: {error}"),
                    };
                    let request: Request = serde_json::from_slice(&buffer[..count]).unwrap();
                    let open = released.load(Ordering::Acquire);
                    let kind = match &request.command {
                        seedbox_engine_protocol::host_control::Command::Status => "status",
                        seedbox_engine_protocol::host_control::Command::SetRate { .. } => "rate",
                        seedbox_engine_protocol::host_control::Command::Checkpoint { .. } => {
                            "checkpoint"
                        }
                        seedbox_engine_protocol::host_control::Command::PrepareFault { .. } => {
                            "prepare"
                        }
                        seedbox_engine_protocol::host_control::Command::CommitFault { .. } => {
                            "commit"
                        }
                        other => panic!("unexpected command {other:?}"),
                    };
                    {
                        let mut seen = seen.lock().unwrap();
                        if !seen.iter().any(|(wire, _, _)| *wire == request.id) {
                            seen.push((request.id, kind.into(), open));
                        }
                    }
                    if !open {
                        continue;
                    }
                    let mut reply = Reply {
                        id: request.id,
                        position,
                        terminal: false,
                        application_ready: true,
                        rate,
                        achieved_rate: None,
                        stepping: false,
                        checkpoint: None,
                        error: None,
                        detail: None,
                        intervention: None,
                    };
                    match request.command {
                        seedbox_engine_protocol::host_control::Command::SetRate {
                            rate: requested,
                        } => {
                            rate = requested;
                            reply.rate = requested;
                        }
                        seedbox_engine_protocol::host_control::Command::Checkpoint {
                            request_id,
                        } => {
                            reply.checkpoint =
                                Some(seedbox_engine_protocol::host_control::CheckpointStatus {
                                    request_id,
                                    position,
                                    state: seedbox_engine_protocol::host_control::CheckpointState::Captured,
                                });
                        }
                        seedbox_engine_protocol::host_control::Command::PrepareFault {
                            request_id,
                            action,
                            timeout_ns,
                        } => {
                            let record = seedbox_api::Intervention {
                                request_id,
                                position,
                                plan: seedbox_control::Plan {
                                    id: 1,
                                    at_ns: position.simulated_time_ns,
                                    deadline_ns: position.simulated_time_ns + timeout_ns,
                                    action,
                                },
                            };
                            proposal = Some(record.clone());
                            reply.intervention = Some(record);
                        }
                        seedbox_engine_protocol::host_control::Command::CommitFault { .. } => {
                            reply.intervention = proposal.clone();
                        }
                        _ => {}
                    }
                    socket
                        .send_to(
                            &serde_json::to_vec(&reply).unwrap(),
                            from.as_pathname().unwrap(),
                        )
                        .unwrap();
                }
            })
        };
        let driver = tokio::spawn(run(manager.clone(), shutdown.clone()));
        let kinds = |seen: &Seen| -> Vec<String> {
            seen.lock()
                .unwrap()
                .iter()
                .map(|(_, kind, _)| kind.clone())
                .collect()
        };
        // The driver's background status read times out with its reply withheld.
        tokio::time::timeout(Duration::from_secs(3), async {
            while kinds(&seen).is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let fault = ControlRequest {
            request_id: "cut".into(),
            command: ControlCommand::InjectFault {
                action: serde_json::from_value(serde_json::json!({"action": "partition",
                    "handle": "cut", "left": ["a"], "right": ["b"]}))
                .unwrap(),
            },
        };
        let checkpoint = ControlRequest {
            request_id: "saved".into(),
            command: ControlCommand::Checkpoint,
        };
        let rate = ControlRequest {
            request_id: "slow".into(),
            command: ControlCommand::SetRate {
                rate: seedbox_api::PlaybackRate::Paused,
            },
        };
        manager.accept_control(&id, fault.clone()).unwrap();
        // Later commands are refused, not queued, while the fault is pending.
        for later in [&checkpoint, &rate] {
            assert!(matches!(
                manager.accept_control(&id, later.clone()),
                Err(Error::ControlBusy)
            ));
        }
        // Several apply attempts later, still only the status has been sent.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(
            kinds(&seen).iter().all(|kind| kind == "status"),
            "a mutation overtook the uncertain status: {:?}",
            kinds(&seen)
        );
        released.store(true, Ordering::Release);
        let acknowledged = |request_id: &'static str| {
            let manager = manager.clone();
            let id = id.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let receipt = manager
                            .read(|catalogue| {
                                Ok(catalogue.control_receipt(&id, request_id)?.cloned())
                            })
                            .unwrap()
                            .unwrap();
                        if let ControlState::Acknowledged { error, .. } = &receipt.state {
                            assert_eq!(*error, None, "{request_id}: {receipt:?}");
                            return receipt;
                        }
                        assert_eq!(receipt.state, ControlState::Accepted);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap()
            }
        };
        acknowledged("cut").await;
        for later in [&checkpoint, &rate] {
            manager.accept_control(&id, later.clone()).unwrap();
            acknowledged(if later.request_id == "saved" {
                "saved"
            } else {
                "slow"
            })
            .await;
        }
        // A retry of an acknowledged key returns its receipt and sends nothing.
        let before = kinds(&seen)
            .into_iter()
            .filter(|kind| kind != "status")
            .count();
        let receipt = manager.accept_control(&id, fault.clone()).unwrap();
        assert!(matches!(receipt.state, ControlState::Acknowledged { .. }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let mutations: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, kind, _)| kind != "status")
            .cloned()
            .collect();
        assert_eq!(mutations.len(), before);
        // Each mutation crossed the wire once, after release, in request order.
        assert_eq!(
            mutations
                .iter()
                .map(|(_, kind, _)| kind.as_str())
                .collect::<Vec<_>>(),
            ["prepare", "commit", "checkpoint", "rate"]
        );
        assert!(mutations.iter().all(|(_, _, open)| *open));
        // The status sent before the fault is resolved by the same wire request.
        let first = seen.lock().unwrap()[0].clone();
        assert_eq!(first.1, "status");
        assert!(!first.2);
        for request_id in ["cut", "saved", "slow"] {
            let receipt = manager
                .read(|catalogue| Ok(catalogue.control_receipt(&id, request_id)?.cloned()))
                .unwrap()
                .unwrap();
            assert_eq!(receipt.revision, 2, "{request_id}: one acknowledgement");
        }
        shutdown.cancel();
        driver.await.unwrap().unwrap();
        stop.store(true, Ordering::Release);
        peer.join().unwrap();
    }
}
