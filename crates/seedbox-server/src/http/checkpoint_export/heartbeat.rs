//! Heartbeats carry no export state and keep old SSE clients compatible.
use super::{CancelOnDrop, CheckpointExport, Event, Infallible};
use futures_util::{Stream, StreamExt};
use std::{pin::Pin, time::Duration};
use tokio::sync::mpsc;

pub(super) const INTERVAL: Duration = Duration::from_secs(10);
type Ticks = Pin<Box<dyn Stream<Item = ()> + Send>>;

pub(super) fn ticks() -> Ticks {
    let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + INTERVAL, INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    Box::pin(futures_util::stream::unfold(
        interval,
        |mut interval| async move {
            interval.tick().await;
            Some(((), interval))
        },
    ))
}

pub(super) fn stream(
    receiver: mpsc::Receiver<CheckpointExport>,
    guard: CancelOnDrop,
    ticks: Ticks,
) -> impl Stream<Item = Result<Event, Infallible>> + Send {
    futures_util::stream::unfold(
        (receiver, guard, ticks),
        |(mut receiver, guard, mut ticks)| async move {
            let event = tokio::select! {
                // Progress/terminal delivery and channel EOF beat a simultaneous tick.
                biased;
                update = receiver.recv() => {
                    Event::default().json_data(update?).expect("export update is JSON serializable")
                }
                Some(()) = ticks.next() => Event::default().comment("checkpoint export keepalive"),
            };
            Some((Ok(event), (receiver, guard, ticks)))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::{IntoResponse, Sse};
    use seedbox_application::execution::CancellationToken;
    use seedbox_test_time::ControlledClock;
    use std::task::Poll;

    #[tokio::test]
    async fn bounded_ticks_progress_and_eof_use_controlled_time() {
        let clock = ControlledClock::default();
        let observer = clock.clone();
        let mut next_tick = INTERVAL;
        let ticks = futures_util::stream::poll_fn(move |_| {
            if observer.now() < next_tick {
                return Poll::Pending;
            }
            next_tick = observer.now() + INTERVAL;
            Poll::Ready(Some(()))
        });
        let (sender, receiver) = mpsc::channel(2);
        let token = CancellationToken::default();
        let events = stream(receiver, CancelOnDrop(token.clone()), Box::pin(ticks));
        let mut body = Box::pin(
            Sse::new(events)
                .into_response()
                .into_body()
                .into_data_stream(),
        );
        assert!(futures_util::poll!(body.next()).is_pending());
        clock.advance_by(INTERVAL - Duration::from_secs(1)).unwrap();
        assert!(futures_util::poll!(body.next()).is_pending());
        clock.advance_by(Duration::from_secs(1)).unwrap();
        let Poll::Ready(Some(Ok(heartbeat))) = futures_util::poll!(body.next()) else {
            panic!("heartbeat due at interval")
        };
        assert!(std::str::from_utf8(&heartbeat)
            .unwrap()
            .contains("checkpoint export keepalive"));
        assert!(heartbeat.starts_with(b":"));
        clock
            .advance_by(INTERVAL - Duration::from_nanos(1))
            .unwrap();
        assert!(futures_util::poll!(body.next()).is_pending());
        clock.advance_by(Duration::from_nanos(1)).unwrap();
        assert!(matches!(
            futures_util::poll!(body.next()),
            Poll::Ready(Some(Ok(_)))
        ));
        // When both are ready, real progress wins over a heartbeat tick.
        clock.advance_by(INTERVAL).unwrap();
        sender
            .try_send(CheckpointExport::Progress {
                bytes_processed: 65536,
            })
            .unwrap();
        let Poll::Ready(Some(Ok(progress))) = futures_util::poll!(body.next()) else {
            panic!("queued progress must be delivered")
        };
        assert!(std::str::from_utf8(&progress)
            .unwrap()
            .contains("bytesProcessed"));
        drop(sender);
        assert!(matches!(
            futures_util::poll!(body.next()),
            Poll::Ready(None)
        ));
        drop(body);
        assert!(token.is_cancelled());
    }
}
