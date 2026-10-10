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
    let Some(request) = request else {
        if let Ok(status) = worker.inspect() {
            manager.observe_playback(id, status)?;
        }
        return Ok(());
    };
    let result = worker.apply(&request);
    let receipt = match result {
        Ok(receipt) => receipt,
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
