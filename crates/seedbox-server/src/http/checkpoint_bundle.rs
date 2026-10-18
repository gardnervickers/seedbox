//! Portable checkpoint files, independent of the source server catalogue.
use super::*;

struct Cancel(seedbox_application::execution::CancellationToken);
impl Drop for Cancel {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) async fn export(
    State(state): State<ApiState>,
    id: Result<Path<(SimulationId, String)>, PathRejection>,
    query: Result<Query<seedbox_api::BundleTransferQuery>, QueryRejection>,
) -> Result<Response, HttpError> {
    use std::io::{Seek, SeekFrom};
    use tokio::io::AsyncReadExt;
    let Path((id, checkpoint)) = id.map_err(|e| HttpError::invalid(e.body_text()))?;
    let mut options = bundle_options(query)?;
    options.cancellation = state.shutdown.owned();
    let execution = state.execution.ok_or_else(|| {
        HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Unavailable,
            "execution service unavailable",
        )
    })?;
    // Dropping a connection while spooling cooperatively cancels the blocking
    // writer. Once prepared, the response owns its anonymous spool file.
    let _cancel = Cancel(options.cancellation.clone());
    let (file, size) = tokio::task::spawn_blocking(move || -> Result<_, HttpError> {
        let mut file = tempfile::tempfile().map_err(|e| HttpError::internal(e.to_string()))?;
        execution.export_checkpoint_bundle(&id, &checkpoint, &mut file, &options)?;
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
                seedbox_api::CHECKPOINT_CONTENT_TYPE.to_owned(),
            ),
            (axum::http::header::CONTENT_LENGTH, size.to_string()),
        ],
        axum::body::Body::from_stream(stream),
    )
        .into_response())
}

pub(super) async fn import(
    State(state): State<ApiState>,
    query: Result<Query<seedbox_api::BundleTransferQuery>, QueryRejection>,
    headers: axum::http::HeaderMap,
    body: axum::body::Body,
) -> Result<(StatusCode, Json<seedbox_spec::definition::Definition>), HttpError> {
    use futures_util::StreamExt;
    use std::io::Seek;
    use tokio::io::AsyncWriteExt;
    let mut options = bundle_options(query)?;
    options.cancellation = state.shutdown.owned();
    let _cancel = Cancel(options.cancellation.clone());
    if headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(seedbox_api::CHECKPOINT_CONTENT_TYPE)
    {
        return Err(HttpError::invalid(
            "expected application/vnd.seedbox.checkpoint+tar body",
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
    let resource =
        tokio::task::spawn_blocking(move || execution.import_checkpoint_bundle(file, &options))
            .await
            .map_err(|e| HttpError::internal(e.to_string()))??;
    Ok((StatusCode::OK, Json(resource)))
}
