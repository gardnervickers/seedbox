//! Versioned local HTTP resource interface. The execution owner drains accepted
//! IDs from the supplied queue; it must recover queued resources on startup.

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
    manager: Arc<Manager>,
    submissions: mpsc::Sender<SimulationId>,
}

impl ApiState {
    pub fn with_execution(mut self, execution: Arc<crate::execution::ExecutionService>) -> Self {
        self.execution = Some(execution);
        self
    }

    pub fn new(manager: Arc<Manager>, submissions: mpsc::Sender<SimulationId>) -> Self {
        Self {
            execution: None,
            manager,
            submissions,
        }
    }
}

pub fn router(state: ApiState) -> Router {
    Router::new()
        .route("/v1", get(info))
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
        .route("/v1/simulations/{id}/watch", get(watch))
        .route("/v1/simulations/{id}/playback", get(playback))
        .route("/v1/simulations/{id}/checkpoints", get(checkpoints))
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

async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        api_version: ApiVersion::V1,
        process_id: std::process::id(),
    })
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

async fn describe(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<Json<Simulation>, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    tokio::task::spawn_blocking(move || {
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

async fn events(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<impl IntoResponse, HttpError> {
    use crate::stream::Delivery;
    use axum::response::sse::{Event, KeepAlive, Sse};
    use seedbox_api::EventDelivery;
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.events(&id))
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

async fn logs(
    State(state): State<ApiState>,
    id: Result<Path<(SimulationId, String)>, PathRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let Path((id, machine)) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.logs(&id, &machine))
        .await
        .map_err(|_| HttpError::internal("event subscription task failed"))??;
    Ok(log_response(receiver))
}

async fn all_logs(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> Result<impl IntoResponse, HttpError> {
    let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let receiver = tokio::task::spawn_blocking(move || state.manager.all_logs(&id))
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
            error => Self::internal(error.to_string()),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.error)).into_response()
    }
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
