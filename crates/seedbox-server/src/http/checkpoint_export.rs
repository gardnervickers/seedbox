//! A bounded, request-owned export: dropping the body cancels its blocking work.
use super::HttpError;
use axum::response::{
    sse::{Event, Sse},
    IntoResponse, Response,
};
use seedbox_api::{CheckpointExport, ErrorCode, Simulation};
use seedbox_application::execution::CancellationToken;
use std::{convert::Infallible, io};
use tokio::sync::mpsc;

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub(super) struct Progress {
    cancellation: CancellationToken,
    sender: mpsc::Sender<CheckpointExport>,
    bytes: u64,
}
impl Progress {
    pub(super) fn advance(&mut self, bytes: u64) -> io::Result<()> {
        if self.cancellation.is_cancelled() || self.sender.is_closed() {
            return Err(io::Error::other("checkpoint export cancelled"));
        }
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| io::Error::other("checkpoint export byte count overflow"))?;
        if bytes != 0 {
            // Progress may coalesce under backpressure; terminal delivery may not.
            let _ = self.sender.try_send(CheckpointExport::Progress {
                bytes_processed: self.bytes,
            });
        }
        Ok(())
    }
}

pub(super) fn response(
    shutdown: CancellationToken,
    work: impl FnOnce(&mut Progress) -> Result<Simulation, HttpError> + Send + 'static,
) -> Response {
    let cancellation = shutdown.owned();
    let guard = CancelOnDrop(cancellation.clone());
    let (sender, receiver) = mpsc::channel(2);
    sender
        .try_send(CheckpointExport::Progress { bytes_processed: 0 })
        .expect("new export channel");
    // The body is ready before scheduling any potentially size-dependent work.
    // Keep the JoinHandle so panic becomes a terminal error, never silent EOF.
    let worker_sender = sender.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let mut progress = Progress {
            cancellation,
            sender: worker_sender,
            bytes: 0,
        };
        let result = work(&mut progress);
        if progress.cancellation.is_cancelled() {
            return Err(HttpError::new(
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                ErrorCode::Unavailable,
                "checkpoint export cancelled",
            ));
        }
        result
    });
    tokio::spawn(async move {
        let update = match worker.await {
            Ok(Ok(simulation)) => CheckpointExport::Completed {
                simulation: Box::new(simulation),
            },
            Ok(Err(error)) => CheckpointExport::Failed { error: error.error },
            Err(_) => CheckpointExport::Failed {
                error: HttpError::internal("checkpoint export task failed").error,
            },
        };
        let _ = sender.send(update).await;
    });
    let stream =
        futures_util::stream::unfold((receiver, guard), |(mut receiver, guard)| async move {
            let update = receiver.recv().await?;
            let event = Event::default()
                .json_data(update)
                .expect("export update is JSON serializable");
            Some((Ok::<_, Infallible>(event), (receiver, guard)))
        });
    Sse::new(stream).into_response()
}

#[cfg(test)]
mod tests;
