//! Exploration endpoints the API defines but this server does not carry out
//! yet: campaigns, analyses, explanations and cold bundles. Each checks its
//! request as the implementation will (an unknown simulation is a 404), then
//! answers `notImplemented`.
use super::{ApiState, HttpError};
use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::StatusCode,
    Json,
};
use seedbox_api::{AnalysisRequest, CampaignRequest, ErrorCode, SimulationId};

pub(super) fn not_implemented(name: &str) -> HttpError {
    HttpError::new(
        StatusCode::NOT_IMPLEMENTED,
        ErrorCode::NotImplemented,
        seedbox_engine_protocol::not_implemented(name),
    )
}

/// The named simulation exists, or the request is a 404.
async fn known(state: ApiState, id: SimulationId) -> Result<(), HttpError> {
    tokio::task::spawn_blocking(move || {
        state
            .manager
            .read(|catalogue| catalogue.get(&id).map(drop))
            .map_err(HttpError::from)
    })
    .await
    .map_err(|_| HttpError::internal("simulation query task failed"))?
}

pub(super) async fn submit_campaign(
    request: Result<Json<CampaignRequest>, JsonRejection>,
) -> HttpError {
    let checked = request
        .map_err(|e| HttpError::invalid(e.body_text()))
        .and_then(|Json(request)| {
            request.plan.validate().map_err(HttpError::invalid)?;
            if let Some(profile) = &request.fault_profile {
                profile.validate().map_err(HttpError::invalid)?;
                crate::catalogue::fits_input_budget(profile, request.spec.program.len())
                    .map_err(HttpError::invalid)?;
            }
            Ok(())
        });
    checked.err().unwrap_or_else(|| not_implemented("campaign"))
}

pub(super) async fn campaign(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> HttpError {
    let checked = match id {
        Err(e) => Err(HttpError::invalid(e.body_text())),
        Ok(Path(id)) => known(state, id).await,
    };
    checked.err().unwrap_or_else(|| not_implemented("campaign"))
}

pub(super) async fn submit_analysis(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
    request: Result<Json<AnalysisRequest>, JsonRejection>,
) -> HttpError {
    let checked = async {
        let Path(id) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
        let Json(request) = request.map_err(|e| HttpError::invalid(e.body_text()))?;
        request.validate().map_err(HttpError::invalid)?;
        known(state, id).await
    };
    checked
        .await
        .err()
        .unwrap_or_else(|| not_implemented("analysis"))
}

pub(super) async fn analysis(
    State(state): State<ApiState>,
    path: Result<Path<(SimulationId, String)>, PathRejection>,
) -> HttpError {
    let checked = match path {
        Err(e) => Err(HttpError::invalid(e.body_text())),
        Ok(Path((id, _))) => known(state, id).await,
    };
    checked.err().unwrap_or_else(|| not_implemented("analysis"))
}

pub(super) async fn explanation(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> HttpError {
    let checked = match id {
        Err(e) => Err(HttpError::invalid(e.body_text())),
        Ok(Path(id)) => known(state, id).await,
    };
    checked
        .err()
        .unwrap_or_else(|| not_implemented("explanation"))
}

pub(super) async fn cold_bundle(
    State(state): State<ApiState>,
    id: Result<Path<SimulationId>, PathRejection>,
) -> HttpError {
    let checked = match id {
        Err(e) => Err(HttpError::invalid(e.body_text())),
        Ok(Path(id)) => known(state, id).await,
    };
    checked
        .err()
        .unwrap_or_else(|| not_implemented("cold bundle"))
}
