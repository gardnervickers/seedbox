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

    /// NEXT-100 case 96: a background status read times out with its reply
    /// withheld, and a fault, a checkpoint and a rate change are then requested.
    /// Later commands are refused while one is pending rather than queued. No
    /// mutation reaches the worker until the same status request resolves; then
    /// each command crosses the wire once, in request order, with one receipt.
    #[test]
    fn mutations_wait_for_an_uncertain_status_and_never_overtake() {
        use seedbox_engine_protocol::host_control::{CheckpointState, CheckpointStatus, Command};
        // What the fake worker received, in arrival order. A marker is a
        // datagram sent by the test itself: everything queued before it was
        // sent before the test's send.
        #[derive(Debug)]
        enum Arrival {
            Request {
                wire: u64,
                kind: &'static str,
                released: bool,
            },
            Marker,
        }
        let guard = seedbox_test_time::HangGuard::new(
            "the fake worker and the control driver make progress",
            Duration::from_secs(60),
        );
        let directory = tempfile::tempdir().unwrap();
        let manager = Arc::new(Manager::new(crate::catalogue::Catalogue::in_memory()));
        let configuration = seedbox_application::config::parse(
            "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
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
        socket.set_nonblocking(true).unwrap();
        fs::write(
            run_dir.join("output/host-control"),
            endpoint.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        manager.attach_worker(&id, &run_dir).unwrap();

        // The fake worker withholds every reply until released, then answers
        // each command, including retransmissions of the withheld status.
        let stop = Arc::new(AtomicBool::new(false));
        let released = Arc::new(AtomicBool::new(false));
        let (arrived, arrivals) = std::sync::mpsc::channel();
        let peer = {
            let (stop, released, guard) = (stop.clone(), released.clone(), guard.clone());
            std::thread::spawn(move || {
                let mut buffer = [0; MAX_DATAGRAM + 1];
                let position = BoundaryPosition {
                    boundary_index: 4,
                    completed_rounds: 3,
                    simulated_time_ns: 500,
                };
                let mut rate = PlaybackRate::Unlimited;
                let mut proposal = None;
                loop {
                    let received = guard
                        .wait_for(|| {
                            if stop.load(Ordering::Acquire) {
                                return Some(None);
                            }
                            match socket.recv_from(&mut buffer) {
                                Ok(received) => Some(Some(received)),
                                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                    None
                                }
                                Err(error) => panic!("fake worker: {error}"),
                            }
                        })
                        .unwrap();
                    let Some((count, from)) = received else {
                        break;
                    };
                    if &buffer[..count] == b"marker" {
                        arrived.send(Arrival::Marker).unwrap();
                        continue;
                    }
                    let request: Request = serde_json::from_slice(&buffer[..count]).unwrap();
                    let open = released.load(Ordering::Acquire);
                    let kind = match &request.command {
                        Command::Status => "status",
                        Command::SetRate { .. } => "rate",
                        Command::Step => "step",
                        Command::Checkpoint { .. } => "checkpoint",
                        Command::PrepareFault { .. } => "prepare",
                        Command::CommitFault { .. } => "commit",
                        other => panic!("unexpected command {other:?}"),
                    };
                    // Report before replying: a receipt implies its arrival.
                    arrived
                        .send(Arrival::Request {
                            wire: request.id,
                            kind,
                            released: open,
                        })
                        .unwrap();
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
                        before_next_fault: false,
                        checkpoint: None,
                        error: None,
                        detail: None,
                        intervention: None,
                    };
                    match request.command {
                        Command::SetRate { rate: requested } => {
                            rate = requested;
                            reply.rate = requested;
                        }
                        Command::Checkpoint { request_id } => {
                            reply.checkpoint = Some(CheckpointStatus {
                                request_id,
                                position,
                                state: CheckpointState::Captured,
                            });
                        }
                        Command::PrepareFault {
                            request_id,
                            action,
                            timeout_ns,
                        } => {
                            let record = seedbox_api::Intervention {
                                request_id,
                                position,
                                plan: seedbox_engine_protocol::control::Plan {
                                    id: 1,
                                    at_ns: position.simulated_time_ns,
                                    deadline_ns: position.simulated_time_ns + timeout_ns,
                                    action,
                                },
                            };
                            proposal = Some(record.clone());
                            reply.intervention = Some(record);
                        }
                        Command::CommitFault { .. } => reply.intervention = proposal.clone(),
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
        let shutdown = CancellationToken::default();
        let driver = {
            let (manager, shutdown) = (manager.clone(), shutdown.clone());
            std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(run(manager, shutdown))
            })
        };
        let mut seen = Vec::new();
        let next = |seen: &mut Vec<Arrival>| {
            seen.push(guard.recv(&arrivals).unwrap());
            seen.last()
                .map(|arrival| matches!(arrival, Arrival::Marker))
        };
        let marker = || {
            UnixDatagram::unbound()
                .unwrap()
                .send_to(b"marker", &endpoint)
                .unwrap();
        };
        let worker = manager.worker(&id).unwrap().unwrap();
        // The driver's background status read is withheld. Acquiring the
        // connection gate after its datagram arrived means that read returned,
        // and without a reply it can only have timed out.
        next(&mut seen);
        let mut gate = worker.lock().unwrap();
        let fault = ControlRequest {
            request_id: "cut".into(),
            command: ControlCommand::InjectFault {
                action: seedbox_api::FaultAction::Heal {
                    handle: "cut".into(),
                },
            },
        };
        let checkpoint = ControlRequest {
            request_id: "saved".into(),
            command: ControlCommand::Checkpoint,
        };
        let rate = ControlRequest {
            request_id: "slow".into(),
            command: ControlCommand::SetRate {
                rate: PlaybackRate::Paused,
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
        // One drive whose work was read before the fault was accepted may still
        // inspect. Two rounds of "a datagram after a marker, then the gate"
        // pass it, so the second round's datagram comes from a drive applying
        // the fault while the status is still unresolved.
        for _ in 0..2 {
            marker();
            drop(gate);
            while next(&mut seen) != Some(true) {}
            next(&mut seen);
            gate = worker.lock().unwrap();
        }
        let first = match &seen[0] {
            Arrival::Request {
                wire,
                kind: "status",
                released: false,
            } => *wire,
            other => panic!("first arrival {other:?}"),
        };
        for arrival in &seen {
            let resolving = match arrival {
                Arrival::Marker => true,
                Arrival::Request {
                    wire,
                    kind,
                    released,
                } => *wire == first && *kind == "status" && !released,
            };
            assert!(
                resolving,
                "a mutation overtook the uncertain status: {seen:?}"
            );
        }
        released.store(true, Ordering::Release);
        drop(gate);
        let acknowledged = |request_id: &str| {
            guard
                .wait_for(|| {
                    let receipt = manager
                        .read(|catalogue| Ok(catalogue.control_receipt(&id, request_id)?.cloned()))
                        .unwrap()
                        .unwrap();
                    match &receipt.state {
                        ControlState::Accepted => None,
                        ControlState::Acknowledged { error: None, .. } => Some(receipt),
                        other => panic!("{request_id}: {other:?}"),
                    }
                })
                .unwrap()
        };
        acknowledged("cut");
        for later in [&checkpoint, &rate] {
            manager.accept_control(&id, later.clone()).unwrap();
            acknowledged(&later.request_id);
        }
        // A retry of an acknowledged key returns its receipt and enqueues
        // nothing: a later probe is admitted at once and is the next mutation.
        let retried = manager.accept_control(&id, fault.clone()).unwrap();
        assert!(matches!(retried.state, ControlState::Acknowledged { .. }));
        manager
            .accept_control(
                &id,
                ControlRequest {
                    request_id: "probe".into(),
                    command: ControlCommand::Step,
                },
            )
            .unwrap();
        acknowledged("probe");
        seen.extend(arrivals.try_iter());
        // The withheld status was resolved by the same wire request: the
        // fault's prepare is the very next request the connection issued.
        let mut mutations = Vec::new();
        for arrival in &seen {
            if let Arrival::Request {
                wire,
                kind,
                released,
            } = arrival
            {
                if *kind != "status" && !mutations.iter().any(|(seen, _)| seen == wire) {
                    assert!(released, "{kind} reached the worker before release");
                    mutations.push((*wire, *kind));
                }
            }
        }
        assert_eq!(
            mutations.iter().map(|(_, kind)| *kind).collect::<Vec<_>>(),
            ["prepare", "commit", "checkpoint", "rate", "step"],
            "each mutation crosses the wire once, in request order: {seen:?}"
        );
        assert_eq!(mutations[0].0, first + 1);
        for request_id in ["cut", "saved", "slow"] {
            let receipt = manager
                .read(|catalogue| Ok(catalogue.control_receipt(&id, request_id)?.cloned()))
                .unwrap()
                .unwrap();
            assert_eq!(receipt.revision, 2, "{request_id}: one acknowledgement");
        }
        shutdown.cancel();
        driver.join().unwrap().unwrap();
        stop.store(true, Ordering::Release);
        peer.join().unwrap();
    }
}
