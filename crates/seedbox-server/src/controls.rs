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
        assert_eq!(failed.status.oracle_verdict(), TestVerdict::Passed);
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
}
