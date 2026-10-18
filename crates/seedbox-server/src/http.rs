//! Versioned local HTTP resource interface. The execution owner drains accepted
//! IDs from the supplied queue; it must recover queued resources on startup.

mod checkpoint_export;
mod export_accept;

use crate::{catalogue::Error as CatalogueError, manager::Manager};
use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        DefaultBodyLimit, Path, Query, State,
    },
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use seedbox_api::{
    ApiError, ApiVersion, ErrorCode, ServerInfo, Simulation, SimulationId, SimulationList,
    SubmitRequest,
};
use serde::Deserialize;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Clone)]
pub struct ApiState {
    execution: Option<Arc<crate::execution::ExecutionService>>,
    shutdown: seedbox_application::execution::CancellationToken,
    manager: Arc<Manager>,
    submissions: mpsc::Sender<SimulationId>,
}

impl ApiState {
    pub fn with_execution(mut self, execution: Arc<crate::execution::ExecutionService>) -> Self {
        self.execution = Some(execution);
        self
    }

    /// Parent cancellation for request-owned work, including checkpoint export.
    pub fn with_shutdown(
        mut self,
        shutdown: seedbox_application::execution::CancellationToken,
    ) -> Self {
        self.shutdown = shutdown;
        self
    }

    pub fn new(manager: Arc<Manager>, submissions: mpsc::Sender<SimulationId>) -> Self {
        Self {
            execution: None,
            shutdown: Default::default(),
            manager,
            submissions,
        }
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1/simulations/{id}/bundle", get(export_bundle))
        .route(
            "/v1/bundles",
            post(import_bundle).layer(DefaultBodyLimit::disable()),
        )
        .route("/v1", get(info))
        .route("/v1/capacity", get(capacity))
        .route("/v1/simulations", get(list).post(submit))
        .route("/v1/seed-ranges", post(submit_seeds))
        .route("/v1/simulations/{id}/seeds", get(seed_range))
        .route("/v1/simulations/{id}/seeds/stop", post(stop_seeds))
        .route(
            "/v1/simulations/{id}",
            get(describe).delete(delete_simulation),
        )
        .route("/v1/simulations/{id}/stop", post(stop))
        .route("/v1/simulations/{id}/compare/{other}", get(compare))
        .route("/v1/simulations/{id}/diagnostics", get(diagnostics))
        .route(
            "/v1/simulations/{id}/diagnostics/file",
            get(diagnostic_file),
        )
        .route("/v1/simulations/{id}/watch", get(watch))
        .route("/v1/simulations/{id}/playback", get(playback))
        .route("/v1/simulations/{id}/timeline", get(timeline))
        .route("/v1/simulations/{id}/derive", post(derive))
        .route("/v1/simulations/{id}/checkpoints", get(checkpoints))
        .route(
            "/v1/simulations/{id}/checkpoints/{request_id}/definition",
            get(checkpoint_definition),
        )
        .route(
            "/v1/simulations/{id}/checkpoints/{request_id}",
            get(checkpoint).delete(delete_checkpoint),
        )
        .route("/v1/simulations/{id}/controls", post(request_control))
        .route(
            "/v1/simulations/{id}/controls/{request_id}",
            get(control_receipt),
        )
        .route("/v1/simulations/{id}/events", get(events))
        .route("/v1/simulations/{id}/world", get(world))
        .route("/v1/simulations/{id}/fault-types", get(fault_types))
        .route("/v1/simulations/{id}/fault-preview", post(fault_preview))
        .route("/v1/simulations/{id}/frames", get(frames))
        .route("/v1/simulations/{id}/logs/{machine}", get(logs))
        .route("/v1/simulations/{id}/logs", get(all_logs))
        .fallback(|| async {
            HttpError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "unknown API endpoint",
            )
        })
        .layer(DefaultBodyLimit::max(16 * 1024 * 1024))
        .with_state(state)
}

fn bundle_options(
    query: Result<Query<seedbox_api::BundleTransferQuery>, QueryRejection>,
) -> Result<seedbox_artifacts::archive::Options, HttpError> {
    let Query(query) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    if query.max_bytes == 0 || query.max_bytes > seedbox_api::BUNDLE_MAX_BYTES {
        return Err(HttpError::invalid(
            "maxBytes must be between 1 and 68719476736",
        ));
    }
    Ok(seedbox_artifacts::archive::Options {
        max_bytes: query.max_bytes,
        ..Default::default()
    })
}

async fn export_bundle(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    query: Result<Query<seedbox_api::BundleTransferQuery>, QueryRejection>,
) -> Result<Response, HttpError> {
    use std::io::{Seek, SeekFrom};
    use tokio::io::AsyncReadExt;
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let options = bundle_options(query)?;
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    // Dropping a connection while spooling cooperatively cancels the blocking
    // writer. Once prepared, the response owns its anonymous spool file.
    struct Cancel(seedbox_application::execution::CancellationToken);
    impl Drop for Cancel {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }
    let _cancel = Cancel(options.cancellation.clone());
    let (file, size) = tokio::task::spawn_blocking(move || -> Result<_, HttpError> {
        let mut file = tempfile::tempfile().map_err(|e| HttpError::internal(e.to_string()))?;
        execution.export_bundle(&id, &mut file, &options)?;
        let size = file
            .metadata()
            .map_err(|e| HttpError::internal(e.to_string()))?
            .len();
        file.seek(SeekFrom::Start(0))
            .map_err(|e| HttpError::internal(e.to_string()))?;
        Ok((tokio::fs::File::from_std(file), size))
    })
    .await
    .map_err(|e| HttpError::internal(e.to_string()))??;
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes).await?;
        if count == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        bytes.truncate(count);
        Ok(Some((bytes, file)))
    });
    Ok((
        [
            (
                axum::http::header::CONTENT_TYPE,
                seedbox_api::BUNDLE_CONTENT_TYPE.to_owned(),
            ),
            (axum::http::header::CONTENT_LENGTH, size.to_string()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

async fn import_bundle(
    State(state): State<ApiState>,
    query: Result<Query<seedbox_api::BundleTransferQuery>, QueryRejection>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Result<(StatusCode, Json<Simulation>), HttpError> {
    use futures_util::StreamExt;
    use std::io::Seek;
    use tokio::io::AsyncWriteExt;
    let options = bundle_options(query)?;
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(seedbox_api::BUNDLE_CONTENT_TYPE)
    {
        return Err(HttpError::invalid(
            "expected application/vnd.seedbox.simulation+tar body",
        ));
    }
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    let mut file = tokio::fs::File::from_std(
        tempfile::tempfile().map_err(|e| HttpError::internal(e.to_string()))?,
    );
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.map_err(|e| HttpError::invalid(e.to_string()))?;
        total = total
            .checked_add(bytes.len() as u64)
            .filter(|total| *total <= options.max_bytes)
            .ok_or_else(|| HttpError::invalid("bundle exceeds wire byte limit"))?;
        file.write_all(&bytes)
            .await
            .map_err(|e| HttpError::internal(e.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|e| HttpError::internal(e.to_string()))?;
    let mut file = file.into_std().await;
    file.rewind()
        .map_err(|e| HttpError::internal(e.to_string()))?;
    let resource = tokio::task::spawn_blocking(move || execution.import_bundle(file, &options))
        .await
        .map_err(|e| HttpError::internal(e.to_string()))??;
    Ok((StatusCode::CREATED, Json(resource)))
}

async fn diagnostics(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::DiagnosticFiles>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    Ok(Json(
        tokio::task::spawn_blocking(move || execution.diagnostics(&id))
            .await
            .map_err(|e| HttpError::internal(e.to_string()))??,
    ))
}

async fn diagnostic_file(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    query: Result<Query<seedbox_api::DiagnosticFileQuery>, QueryRejection>,
) -> Result<Response, HttpError> {
    use std::io::Seek;
    use tokio::io::AsyncReadExt;
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Query(query) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    if !(1..=seedbox_api::BUNDLE_MAX_BYTES).contains(&query.max_bytes) {
        return Err(HttpError::invalid(
            "maxBytes must be between 1 and 68719476736",
        ));
    }
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    let (file, descriptor) = tokio::task::spawn_blocking(move || -> Result<_, HttpError> {
        let mut file = tempfile::tempfile().map_err(|e| HttpError::internal(e.to_string()))?;
        let descriptor = execution.diagnostic_file(&id, &query.name, query.max_bytes, &mut file)?;
        file.rewind()
            .map_err(|e| HttpError::internal(e.to_string()))?;
        Ok((tokio::fs::File::from_std(file), descriptor))
    })
    .await
    .map_err(|e| HttpError::internal(e.to_string()))??;
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut bytes = vec![0; 64 * 1024];
        let count = file.read(&mut bytes).await?;
        if count == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        bytes.truncate(count);
        Ok(Some((bytes, file)))
    });
    Ok((
        [
            ("content-type", "application/octet-stream".to_owned()),
            ("content-length", descriptor.size_bytes.to_string()),
            ("x-seedbox-sha256", descriptor.sha256),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        api_version: ApiVersion::V1,
        process_id: std::process::id(),
    })
}

async fn capacity(
    State(state): State<ApiState>,
) -> Result<Json<seedbox_api::ExecutionCapacity>, HttpError> {
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    let capacity = tokio::task::spawn_blocking(move || execution.capacity())
        .await
        .map_err(|e| HttpError::internal(e.to_string()))??;
    Ok(Json(capacity))
}

async fn submit(
    State(state): State<ApiState>,
    request: Result<Json<SubmitRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Simulation>), HttpError> {
    let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
    // Reserve work capacity before persisting acceptance. The blocking task owns
    // both operations, so disconnecting the HTTP client cannot strand a newly
    // accepted resource between the durable write and queue publication.
    let permit = state.submissions.try_reserve_owned().map_err(|_| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution queue unavailable; retry with the same request ID",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let (simulation, created) = state.manager.submit(request)?;
        if created {
            permit.send(simulation.metadata.id.clone());
        }
        Ok((
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(simulation),
        ))
    })
    .await
    .map_err(|_| HttpError::internal("submission task failed"))?
}

/// Selection is interpreted by the server, never by a client copying private
/// engine configuration. Queue reservation has the same ownership as submit.
async fn derive(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    request: Result<Json<seedbox_api::DeriveRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<Simulation>), HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution evidence is unavailable",
        )
    })?;
    let permit = state.submissions.try_reserve_owned().map_err(|_| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution queue unavailable; retry with the same request ID",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let request = execution.derive_request(&id, request)?;
        let (simulation, created) = state.manager.submit(request)?;
        if created {
            permit.send(simulation.metadata.id.clone());
        }
        Ok((
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(simulation),
        ))
    })
    .await
    .map_err(|_| HttpError::internal("derivation task failed"))?
}

async fn submit_seeds(
    State(state): State<ApiState>,
    request: Result<Json<seedbox_api::SeedRangeRequest>, JsonRejection>,
) -> Result<Json<seedbox_api::SeedRangeStatus>, HttpError> {
    let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
    let permit = state.submissions.try_reserve_owned().map_err(|_| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution queue unavailable; retry with the same request ID",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let (source, created) = state.manager.submit_seeds(request)?;
        if created {
            permit.send(source.metadata.id.clone());
        }
        state
            .manager
            .read(|catalogue| catalogue.seed_range(&source.metadata.id))
            .map(Json)
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("seed range submission task failed"))?
}

async fn seed_range(
    State(state): State<ApiState>,
    path: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::SeedRangeStatus>, HttpError> {
    let Path(id) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state
            .manager
            .read(|catalogue| catalogue.seed_range(&id))
            .map(Json)
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("seed range query task failed"))?
}

async fn stop_seeds(
    State(state): State<ApiState>,
    path: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::SeedRangeStatus>, HttpError> {
    let Path(id) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state
            .manager
            .stop_seeds(&id)
            .map(Json)
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("seed range stop task failed"))?
}

async fn compare(
    State(state): State<ApiState>,
    path: Result<Path<(SimulationId, SimulationId)>, PathRejection>,
) -> Result<Json<seedbox_api::SimulationComparison>, HttpError> {
    let Path((baseline, simulation)) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution evidence is unavailable",
        )
    })?;
    tokio::task::spawn_blocking(move || {
        execution
            .compare(&baseline, &simulation)
            .map(Json)
            .map_err(|e| HttpError::invalid(e.to_string()))
    })
    .await
    .map_err(|_| HttpError::internal("comparison task failed"))?
}

async fn timeline(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::FaultTimeline>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state.manager.describe(&id)?;
        let execution = state.execution.ok_or_else(|| {
            HttpError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "execution evidence is unavailable",
            )
        })?;
        execution.timeline(&id).map(Json).map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("timeline task failed"))?
}

async fn describe(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<Simulation>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        if let Some(execution) = state.execution {
            return execution.describe(&id).map(Json).map_err(HttpError::from);
        }
        state
            .manager
            .describe(&id)
            .map(Json)
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("describe task failed"))?
}

async fn stop(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<Simulation>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || state.manager.stop(&id).map(Json).map_err(HttpError::from))
        .await
        .map_err(|_| HttpError::internal("stop task failed"))?
}

async fn request_control(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    request: Result<Json<seedbox_api::ControlRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<seedbox_api::ControlReceipt>), HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
    let control = tokio::task::spawn_blocking(move || state.manager.accept_control(&id, request))
        .await
        .map_err(|_| HttpError::internal("control acceptance task failed"))??;
    let status = if control.state == seedbox_api::ControlState::Accepted {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(control)))
}

async fn checkpoints(
    State(state): State<ApiState>,
    path: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<Vec<seedbox_api::ControlReceipt>>, HttpError> {
    let Path(id) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let items = tokio::task::spawn_blocking(move || {
        state.manager.read(|catalogue| catalogue.checkpoints(&id))
    })
    .await
    .map_err(|_| HttpError::internal("checkpoint query task failed"))??;
    Ok(Json(items))
}

async fn delete_simulation(
    State(state): State<ApiState>,
    path: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let Path(id) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state
        .execution
        .ok_or_else(|| HttpError::internal("execution evidence is unavailable"))?;
    tokio::task::spawn_blocking(move || {
        execution.delete(&id).map_err(HttpError::from)?;
        Ok(Json(serde_json::json!({"simulation": id, "deleted": true})))
    })
    .await
    .map_err(|_| HttpError::internal("simulation deletion task failed"))?
}

async fn delete_checkpoint(
    State(state): State<ApiState>,
    path: Result<Path<(SimulationId, String)>, PathRejection>,
) -> Result<Json<serde_json::Value>, HttpError> {
    let Path((id, name)) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state
        .execution
        .ok_or_else(|| HttpError::internal("execution evidence is unavailable"))?;
    tokio::task::spawn_blocking(move || {
        execution
            .delete_checkpoint(&id, &name)
            .map_err(HttpError::from)?;
        Ok(Json(
            serde_json::json!({"simulation": id, "checkpoint": name, "deleted": true}),
        ))
    })
    .await
    .map_err(|_| HttpError::internal("checkpoint deletion task failed"))?
}

async fn checkpoint(
    state: State<ApiState>,
    path: Result<Path<(SimulationId, String)>, PathRejection>,
) -> Result<Json<seedbox_api::ControlReceipt>, HttpError> {
    let Path((id, name)) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let items = checkpoints(state, Ok(Path(id))).await?;
    items
        .0
        .into_iter()
        .find(|item| item.request.request_id == name)
        .map(Json)
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::NOT_FOUND,
                ErrorCode::NotFound,
                "checkpoint not found",
            )
        })
}

async fn control_receipt(
    State(state): State<ApiState>,
    path: Result<Path<(SimulationId, String)>, PathRejection>,
) -> Result<Json<seedbox_api::ControlReceipt>, HttpError> {
    let Path((id, request_id)) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let control = tokio::task::spawn_blocking(move || {
        state
            .manager
            .read(|catalogue| Ok(catalogue.control_receipt(&id, &request_id)?.cloned()))
    })
    .await
    .map_err(|_| HttpError::internal("control receipt task failed"))??;
    control.map(Json).ok_or_else(|| {
        HttpError::new(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "control request not found",
        )
    })
}

async fn playback(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::PlaybackObservation>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        let resource = state.manager.describe(&id)?;
        resource.status.playback.map(Json).ok_or_else(|| {
            HttpError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                if resource.status.phase == seedbox_api::Phase::Running {
                    "worker has not published a playback observation yet"
                } else {
                    "simulation has no active worker control endpoint"
                },
            )
        })
    })
    .await
    .map_err(|_| HttpError::internal("playback inspection task failed"))?
}

async fn watch(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<impl IntoResponse, HttpError> {
    use axum::response::sse::{Event, KeepAlive, Sse};
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let manager = state.manager;
    let owner = manager.clone();
    let simulation = id.clone();
    let receiver = tokio::task::spawn_blocking(move || owner.watch(&simulation))
        .await
        .map_err(|_| HttpError::internal("watch task failed"))??;
    let stream =
        futures_util::stream::unfold(Some((receiver, true, manager, id)), |state| async move {
            let (mut receiver, first, manager, id) = state?;
            if !first {
                tokio::select! {
                    changed = receiver.changed() => {
                        if changed.is_err() { return None; }
                    }
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {}
                }
            }
            receiver.borrow_and_update();
            let owner = manager.clone();
            let simulation = id.clone();
            // Query the current resource under the manager lock, so a timing
            // refresh cannot combine an old phase with a newer stage clock.
            let resource =
                match tokio::task::spawn_blocking(move || owner.describe(&simulation)).await {
                    Ok(Ok(resource)) => resource,
                    result => {
                        return Some((
                            Err(std::io::Error::other(format!(
                                "status refresh failed: {result:?}"
                            ))),
                            None,
                        ))
                    }
                };
            let terminal = resource.status.phase == seedbox_api::Phase::Completed;
            let event = Event::default()
                .event("simulation")
                .id(resource.metadata.revision.to_string())
                .json_data(&resource)
                .map_err(std::io::Error::other);
            Some((
                event,
                if terminal {
                    None
                } else {
                    Some((receiver, false, manager, id))
                },
            ))
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// `follow=false` replays retained history and closes; the default follows.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FollowQuery {
    #[serde(default = "following")]
    follow: bool,
}

fn following() -> bool {
    true
}

async fn events(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    query: Result<Query<FollowQuery>, QueryRejection>,
) -> Result<impl IntoResponse, HttpError> {
    use crate::stream::Delivery;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use seedbox_api::EventDelivery;
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Query(FollowQuery { follow }) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.events(&id, follow))
        .await
        .map_err(|_| HttpError::internal("event subscription task failed"))??;
    let stream = futures_util::stream::unfold(Some(receiver), |state| async move {
        let mut receiver = state?;
        let delivery = match receiver.recv().await {
            Delivery::Record(record) => EventDelivery::Event {
                sequence: record.sequence,
                observation: record.payload.clone(),
            },
            Delivery::Gap { first, last } => EventDelivery::Gap { first, last },
            Delivery::Closed => EventDelivery::Closed,
        };
        let closed = matches!(delivery, EventDelivery::Closed);
        Some((
            Event::default().event("observation").json_data(delivery),
            if closed { None } else { Some(receiver) },
        ))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn fault_preview(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    request: Result<Json<seedbox_api::FaultPreviewRequest>, JsonRejection>,
) -> Result<Json<seedbox_api::FaultPreview>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state
            .manager
            .preview_fault(&id, &request.action)
            .map(Json)
            .map_err(Into::into)
    })
    .await
    .map_err(|_| HttpError::internal("fault preview task failed"))?
}

async fn fault_types(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::FaultCatalogue>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state.manager.fault_types(&id).map(Json).map_err(Into::into)
    })
    .await
    .map_err(|_| HttpError::internal("fault discovery task failed"))?
}

async fn world(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<seedbox_api::WorldState>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || state.manager.world(&id).map(Json).map_err(Into::into))
        .await
        .map_err(|_| HttpError::internal("world inspection task failed"))?
}

async fn frames(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<impl IntoResponse, HttpError> {
    use crate::stream::Delivery;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use seedbox_api::FrameDelivery;
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.frames(&id))
        .await
        .map_err(|_| HttpError::internal("frame subscription task failed"))??;
    let stream = futures_util::stream::unfold(Some(receiver), |state| async move {
        let mut receiver = state?;
        let delivery = match receiver.recv().await {
            Delivery::Record(record) => FrameDelivery::Frames {
                sequence: record.sequence,
                simulated_time_ns: record.payload.simulated_time_ns,
                frames: record.payload.frames.clone(),
            },
            Delivery::Gap { first, last } => FrameDelivery::Gap { first, last },
            Delivery::Closed => FrameDelivery::Closed,
        };
        let closed = matches!(delivery, FrameDelivery::Closed);
        Some((
            Event::default().event("observation").json_data(delivery),
            if closed { None } else { Some(receiver) },
        ))
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn logs(
    State(state): State<ApiState>,
    id: Result<Path<(SimulationId, String)>, PathRejection>,
    query: Result<Query<FollowQuery>, QueryRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let Path((id, machine)) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Query(FollowQuery { follow }) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.logs(&id, &machine, follow))
        .await
        .map_err(|_| HttpError::internal("event subscription task failed"))??;
    Ok(log_response(receiver))
}

async fn all_logs(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    query: Result<Query<FollowQuery>, QueryRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let Query(FollowQuery { follow }) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.all_logs(&id, follow))
        .await
        .map_err(|_| HttpError::internal("log subscription task failed"))??;
    Ok(log_response(receiver))
}

fn log_response(receiver: crate::stream::Subscription<seedbox_api::LogEntry>) -> impl IntoResponse {
    use crate::stream::Delivery;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use seedbox_api::LogDelivery;
    let stream = futures_util::stream::unfold(Some(receiver), |state| async move {
        let mut receiver = state?;
        let delivery = match receiver.recv().await {
            Delivery::Record(record) => LogDelivery::Entry {
                sequence: record.sequence,
                entry: record.payload.clone(),
            },
            Delivery::Gap { first, last } => LogDelivery::Gap { first, last },
            Delivery::Closed => LogDelivery::Closed,
        };
        let closed = matches!(delivery, LogDelivery::Closed);
        Some((
            Event::default().event("observation").json_data(delivery),
            if closed { None } else { Some(receiver) },
        ))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<SimulationId>,
}

async fn list(
    State(state): State<ApiState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<Json<SimulationList>, HttpError> {
    let Query(query) = query.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
        state
            .manager
            .list(query.after.as_ref())
            .map(Json)
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("list task failed"))?
}

struct HttpError {
    status: StatusCode,
    error: ApiError,
}

impl HttpError {
    fn new(status: StatusCode, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status,
            error: ApiError {
                code,
                message: message.into(),
            },
        }
    }
    fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, message)
    }
    fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorCode::Internal,
            message,
        )
    }
}

impl From<CatalogueError> for HttpError {
    fn from(error: CatalogueError) -> Self {
        let (status, code) = match &error {
            CatalogueError::Unavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable)
            }
            CatalogueError::NotFound(_) => (StatusCode::NOT_FOUND, ErrorCode::NotFound),
            CatalogueError::RequestConflict
            | CatalogueError::RevisionConflict
            | CatalogueError::ControlBusy
            | CatalogueError::ControlUnavailable => (StatusCode::CONFLICT, ErrorCode::Conflict),
            CatalogueError::Invalid(_) => (StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest),
            _ => (StatusCode::INTERNAL_SERVER_ERROR, ErrorCode::Internal),
        };
        Self::new(status, code, error.to_string())
    }
}

impl From<crate::execution::Error> for HttpError {
    fn from(error: crate::execution::Error) -> Self {
        match error {
            crate::execution::Error::Catalogue(error) => error.into(),
            crate::execution::Error::Bundle(message) => Self::invalid(message),
            error => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
}

async fn checkpoint_definition(
    State(state): State<ApiState>,
    path: Result<Path<(SimulationId, String)>, PathRejection>,
    headers: axum::http::HeaderMap,
) -> Result<Response, HttpError> {
    let Path((id, name)) = path.map_err(|e| HttpError::invalid(e.body_text()))?;
    let execution = state
        .execution
        .ok_or_else(|| HttpError::invalid("execution evidence is unavailable"))?;
    let representation = export_accept::negotiate(&headers).ok_or_else(|| {
        HttpError::new(
            StatusCode::NOT_ACCEPTABLE,
            ErrorCode::InvalidRequest,
            "checkpoint export requires application/json or text/event-stream",
        )
    })?;
    if representation == export_accept::Representation::Stream {
        return Ok(checkpoint_export::response(
            state.shutdown,
            move |progress| {
                execution
                    .checkpoint_definition_observed(&id, &name, &mut |n| progress.advance(n))
                    .map_err(HttpError::from)
            },
        ));
    }
    tokio::task::spawn_blocking(move || {
        execution
            .checkpoint_definition(&id, &name)
            .map(|simulation| Json(simulation).into_response())
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("checkpoint export task failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_errors_keep_resource_and_storage_failures_distinct() {
        let missing: SimulationId = "23456789".parse().unwrap();
        let error = HttpError::from(crate::execution::Error::Catalogue(
            CatalogueError::NotFound(missing),
        ));
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.error.code, ErrorCode::NotFound);
        let error = HttpError::from(crate::execution::Error::Storage(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cleanup denied",
        )));
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.error.code, ErrorCode::Internal);
        let error = HttpError::from(crate::execution::Error::Catalogue(
            CatalogueError::ControlUnavailable,
        ));
        assert_eq!(error.status, StatusCode::CONFLICT);
    }
}
