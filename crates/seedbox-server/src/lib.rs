//! Server-owned simulation lifecycle and observation.
//!
//! Libraries receive explicit runtime inputs. Environment discovery and daemon
//! process startup belong to the executable, not this crate.

#[cfg(unix)]
mod capacity;
#[cfg(unix)]
pub mod catalogue;
#[cfg(unix)]
mod checkpoint_cleanup;
#[cfg(unix)]
mod controls;
#[cfg(unix)]
pub mod execution;
#[cfg(unix)]
mod fault_catalogue;
#[cfg(unix)]
mod fault_preview;
#[cfg(unix)]
pub mod http;
#[cfg(unix)]
pub mod manager;
#[cfg(unix)]
mod observations;
#[cfg(unix)]
mod preparation_budget;
pub mod stream;
#[cfg(unix)]
mod timing;
#[cfg(unix)]
mod worker;
#[cfg(unix)]
mod world;

/// Run HTTP and owned execution together. The executable supplies a socket in
/// its private state directory and handles signals through `shutdown`.
#[cfg(unix)]
pub async fn serve(
    listener: tokio::net::UnixListener,
    manager: std::sync::Arc<manager::Manager>,
    execution: std::sync::Arc<execution::ExecutionService>,
    capacity: std::num::NonZeroUsize,
    shutdown: seedbox_application::execution::CancellationToken,
) -> Result<(), execution::Error> {
    let (sender, receiver) = tokio::sync::mpsc::channel(128);
    let queued = manager.read(|catalogue| {
        Ok(catalogue
            .list()
            .filter(|s| s.status.phase == seedbox_api::Phase::Queued)
            .map(|s| s.metadata.id.clone())
            .collect::<Vec<_>>())
    })?;
    let replay_sender = sender.clone();
    let recovery = tokio::spawn(async move {
        for id in queued {
            if replay_sender.send(id).await.is_err() {
                break;
            }
        }
    });
    let seeds = run_seeds(manager.clone(), sender.clone(), shutdown.clone());
    let jobs = run_jobs(execution.clone(), sender.clone(), shutdown.clone());
    let controls = controls::run(manager.clone(), shutdown.clone());
    let router =
        http::router(http::ApiState::new(manager, sender).with_execution(execution.clone()));
    let http_shutdown = shutdown.clone();
    let http_finished = shutdown.clone();
    let http = async move {
        let result = axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                while !http_shutdown.is_cancelled() {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await;
        http_finished.cancel();
        result
    };
    let (http, execution, controls, seeds, jobs) = tokio::join!(
        http,
        execution.run(receiver, capacity, shutdown),
        controls,
        seeds,
        jobs
    );
    recovery.await?;
    jobs?;
    execution?;
    controls?;
    seeds?;
    http?;
    Ok(())
}

/// Drive campaigns and shrinks: fold finished members, queue new ones.
#[cfg(unix)]
async fn run_jobs(
    execution: std::sync::Arc<execution::ExecutionService>,
    sender: tokio::sync::mpsc::Sender<seedbox_api::SimulationId>,
    shutdown: seedbox_application::execution::CancellationToken,
) -> Result<(), execution::Error> {
    while !shutdown.is_cancelled() {
        let service = execution.clone();
        match tokio::task::spawn_blocking(move || service.advance_jobs()).await {
            Ok(Ok(queued)) => {
                for id in queued {
                    if sender.send(id).await.is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(Err(execution::Error::Catalogue(catalogue::Error::Unavailable))) => return Ok(()),
            Ok(Err(error)) => {
                shutdown.cancel();
                return Err(error);
            }
            Err(error) => {
                shutdown.cancel();
                return Err(error.into());
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(())
}

#[cfg(unix)]
async fn run_seeds(
    manager: std::sync::Arc<manager::Manager>,
    sender: tokio::sync::mpsc::Sender<seedbox_api::SimulationId>,
    shutdown: seedbox_application::execution::CancellationToken,
) -> Result<(), execution::Error> {
    while !shutdown.is_cancelled() {
        if let Ok(permit) = sender.clone().try_reserve_owned() {
            let manager = manager.clone();
            let result = tokio::task::spawn_blocking(move || {
                if let Some(id) = manager.advance_seeds()? {
                    permit.send(id);
                }
                Ok::<_, catalogue::Error>(())
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    shutdown.cancel();
                    return Err(error.into());
                }
                Err(error) => {
                    shutdown.cancel();
                    return Err(error.into());
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    Ok(())
}

mod explain;
mod timeline;
