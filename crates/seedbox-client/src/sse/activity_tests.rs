use super::*;
use std::sync::mpsc;
use std::{task::Poll, time::Duration};

type Source = mpsc::SyncSender<Result<Vec<u8>, Error>>;
type Decoded = Pin<Box<dyn Stream<Item = Result<serde_json::Value, Error>> + Send>>;
fn subject() -> (activity::Controlled, Source, Decoded) {
    let clock = activity::Controlled::default();
    // A manually polled test source has no runtime cooperative-budget effects.
    // Real HTTP delivery/waking stays covered by public-client service tests.
    let (sender, receiver) = mpsc::sync_channel(4);
    let chunks = futures_util::stream::poll_fn(move |_| match receiver.try_recv() {
        Ok(chunk) => Poll::Ready(Some(chunk)),
        Err(mpsc::TryRecvError::Empty) => Poll::Pending,
        Err(mpsc::TryRecvError::Disconnected) => Poll::Ready(None),
    });
    let decoded = decode_chunks(
        chunks,
        |value: &serde_json::Value| value["done"] == true,
        Some(activity::LIMIT),
        clock.clone(),
    );
    (clock, sender, decoded)
}
fn send(sender: &Source, bytes: &[u8]) {
    sender.try_send(Ok(bytes.to_vec())).unwrap();
}
fn pending(decoded: &mut Decoded) {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(decoded.as_mut().poll_next(&mut context).is_pending());
}
fn expired(decoded: &mut Decoded) {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(
        matches!(decoded.as_mut().poll_next(&mut context), Poll::Ready(Some(Err(Error::Stream(message)))) if message == activity::STALLED)
    );
    assert!(matches!(
        decoded.as_mut().poll_next(&mut context),
        Poll::Ready(None)
    ));
}

#[test]
fn controlled_clock_advancement_wakes_the_owned_timer() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct Wakes(AtomicUsize);
    impl std::task::Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = std::task::Waker::from(wakes.clone());
    let mut context = std::task::Context::from_waker(&waker);
    let clock = activity::Controlled::default();
    let mut timer = clock.sleep_until(activity::LIMIT);
    assert!(timer.as_mut().poll(&mut context).is_pending());
    clock.advance(Duration::from_secs(29));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    assert!(timer.as_mut().poll(&mut context).is_pending());
    clock.advance(Duration::from_secs(1));
    assert_eq!(wakes.0.load(Ordering::SeqCst), 2);
    assert!(timer.as_mut().poll(&mut context).is_ready());
}

#[tokio::test]
async fn silence_fails_at_exact_controlled_deadline_with_a_clear_error() {
    let (clock, _sender, mut decoded) = subject();
    pending(&mut decoded);
    clock.advance(activity::LIMIT - Duration::from_nanos(1));
    pending(&mut decoded);
    clock.advance(Duration::from_nanos(1));
    expired(&mut decoded);
}

#[tokio::test]
async fn complete_events_reset_the_idle_deadline_without_a_total_limit() {
    let (clock, sender, mut decoded) = subject();
    for _ in 0..200 {
        pending(&mut decoded);
        clock.advance(Duration::from_secs(20));
        send(&sender, b"data: {\"done\":false}\n\n");
        assert!(
            matches!(futures_util::poll!(decoded.next()), Poll::Ready(Some(Ok(value))) if value["done"] == false)
        );
    }
    assert_eq!(clock.clock.now(), Duration::from_secs(4000));
    send(&sender, b"data: {\"done\":true}\n\n");
    assert!(
        matches!(futures_util::poll!(decoded.next()), Poll::Ready(Some(Ok(value))) if value["done"] == true)
    );
    assert!(matches!(
        futures_util::poll!(decoded.next()),
        Poll::Ready(None)
    ));
}

#[tokio::test]
async fn completed_keepalives_reset_but_partial_bytes_do_not() {
    let (clock, sender, mut decoded) = subject();
    pending(&mut decoded);
    clock.advance(Duration::from_secs(20));
    send(&sender, b": kee");
    pending(&mut decoded);
    clock.advance(Duration::from_secs(9));
    send(&sender, b"palive\r\n\r\n");
    pending(&mut decoded);
    clock.advance(Duration::from_secs(20));
    send(&sender, b"data: {\"done\":false}\r\n\r\n");
    assert!(matches!(
        futures_util::poll!(decoded.next()),
        Poll::Ready(Some(Ok(_)))
    ));
    pending(&mut decoded);
    clock.advance(Duration::from_secs(20));
    send(&sender, b"data: {\"done\":");
    pending(&mut decoded);
    clock.advance(Duration::from_secs(10));
    expired(&mut decoded);
}

#[tokio::test]
async fn caller_backpressure_does_not_expire_an_already_received_terminal_event() {
    let (clock, sender, mut decoded) = subject();
    send(
        &sender,
        b"data: {\"done\":false}\n\ndata: {\"done\":true}\n\n",
    );
    assert!(
        matches!(futures_util::poll!(decoded.next()), Poll::Ready(Some(Ok(value))) if value["done"] == false)
    );
    clock.advance(Duration::from_secs(3600));
    assert!(
        matches!(futures_util::poll!(decoded.next()), Poll::Ready(Some(Ok(value))) if value["done"] == true)
    );
}

#[tokio::test]
async fn endless_valid_keepalives_can_outlive_the_idle_limit_and_then_complete() {
    let (clock, sender, mut decoded) = subject();
    for _ in 0..200 {
        pending(&mut decoded);
        clock.advance(Duration::from_secs(20));
        send(&sender, b": live\n\n");
        pending(&mut decoded);
    }
    send(&sender, b"data: {\"done\":true}\n\n");
    assert!(
        matches!(futures_util::poll!(decoded.next()), Poll::Ready(Some(Ok(value))) if value["done"] == true)
    );
}
