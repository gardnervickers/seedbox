//! Server-owned retry driver. HTTP connection lifetime never owns a mutation.
use crate::{catalogue::Error, manager::Manager};
use seedbox_api::{ControlRequest, ControlState, SimulationId};
use seedbox_application::execution::CancellationToken;
use seedbox_driver_execution::host_control::ClientError;
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
            #[cfg(test)]
            worker.await_failure_publication();
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
        let manager = Arc::new(Manager::new(crate::catalogue::Catalogue::in_memory()));
        let shutdown = CancellationToken::default();
        let stop_peers = Arc::new(AtomicBool::new(false));
        let (request_seen, seen) = std::sync::mpsc::channel();
        let (release_reply, reply_gate) = std::sync::mpsc::channel();
        let mut reply_gate = Some(reply_gate);
        let mut peers = Vec::new();
        let mut simulations = Vec::new();
        for (name, malformed) in [("broken", true), ("healthy", false)] {
            let configuration = seedbox_application::config::parse(
                "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
                std::path::Path::new("/project/seedbox.yaml"),
                &Default::default(),
            )
            .unwrap();
            let resource = manager
                .submit(SubmitRequest {
                    fault_profile: None,
                    metadata: Default::default(),
                    options: Default::default(),
                    request_id: name.into(),
                    spec: SimulationSpec {
                        program: Vec::new(),
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
            socket.set_nonblocking(true).unwrap();
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
                    let received = seedbox_test_time::HangGuard::new(
                        "wait for the fixture cancellation or socket signal",
                        Duration::from_secs(60),
                    )
                    .wait_for(|| {
                        if stop.load(Ordering::Acquire) {
                            return Some(None);
                        }
                        match socket.recv_from(&mut buffer) {
                            Ok(received) => Some(Some(received)),
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => None,
                            Err(error) => panic!("fake worker: {error}"),
                        }
                    })
                    .unwrap();
                    let Some((count, peer)) = received else {
                        break;
                    };
                    let request: Request = serde_json::from_slice(&buffer[..count]).unwrap();
                    let bytes = if malformed {
                        if let Some(gate) = gate.take() {
                            request_seen.send(()).unwrap();
                            gate.recv_timeout(Duration::from_secs(60)).unwrap();
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
                            before_next_fault: false,
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
        let (failure_entered, failure_observed) = std::sync::mpsc::channel();
        let (publish_failure, failure_gate) = std::sync::mpsc::channel();
        manager
            .worker(broken)
            .unwrap()
            .unwrap()
            .lock()
            .unwrap()
            .gate_failure_publication(failure_entered, failure_gate);
        let driver = tokio::spawn(run(manager.clone(), shutdown.clone()));
        tokio::task::spawn_blocking(move || seen.recv_timeout(Duration::from_secs(60)).unwrap())
            .await
            .unwrap();
        release_reply.send(()).unwrap();
        // Wait for actual malformed-wire decoding, not merely request dispatch.
        // This timeout is only a hang guard. Once observed, no RPC deadline can
        // release the gate before the test explicitly permits publication.
        tokio::task::spawn_blocking(move || {
            failure_observed
                .recv_timeout(Duration::from_secs(60))
                .unwrap()
        })
        .await
        .unwrap();
        assert!(manager
            .describe(broken)
            .unwrap()
            .status
            .termination
            .is_none());
        let (retired_tx, retired_rx) = std::sync::mpsc::channel();
        let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let owner = manager.clone();
        let completing = broken.clone();
        let cleanup = std::thread::spawn(move || {
            crate::worker::observe_retirement_blocked(blocked_tx);
            owner.retire_worker(&completing).unwrap();
            // Capture the error at retirement, rather than inferring its order
            // from a later observation after the error eventually appears.
            retired_tx
                .send(owner.describe(&completing).unwrap().status.termination)
                .unwrap();
            finish_rx.recv_timeout(Duration::from_secs(60)).unwrap();
            owner
                .update(&completing, |resource| {
                    resource.status.phase = Phase::Completed;
                    resource.status.termination = Some(ExecutionTermination::Stopped);
                })
                .unwrap();
        });
        // Hang guard only: this signal follows a failed acquisition of the
        // actual gate held at failure publication, under explicit test control.
        blocked_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(
            retired_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        );
        publish_failure.send(()).unwrap();

        tokio::time::timeout(Duration::from_secs(60), async {
            let mut failures = manager.watch(broken).unwrap();
            while !matches!(
                failures.borrow_and_update().status.termination,
                Some(ExecutionTermination::Error { .. })
            ) {
                failures.changed().await.unwrap();
            }
            let mut events = manager.events(healthy, true).unwrap();
            loop {
                let acknowledged = manager
                    .read(|catalogue| {
                        Ok(matches!(
                            catalogue
                                .control_receipt(healthy, "step")?
                                .map(|r| &r.state),
                            Some(ControlState::Acknowledged { .. })
                        ))
                    })
                    .unwrap();
                if acknowledged {
                    break;
                }
                assert!(matches!(
                    events.recv().await,
                    crate::stream::Delivery::Record(_)
                ));
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
        let retired_termination = retired_rx.recv_timeout(Duration::from_secs(60)).unwrap();
        assert_eq!(retired_termination, failed.status.termination);
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
