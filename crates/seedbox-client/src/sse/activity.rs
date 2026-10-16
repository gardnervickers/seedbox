//! Export-only inactivity policy. Each complete event/comment renews the budget;
//! there is deliberately no total-duration deadline.
use std::{future::Future, pin::Pin, time::Duration};

pub(super) const LIMIT: Duration = Duration::from_secs(30);
pub(super) const STALLED: &str =
    "checkpoint export stalled: no progress or keepalive received for 30 seconds";

pub(super) trait Clock: Clone + Send + 'static {
    fn now(&self) -> Duration;
    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

#[derive(Clone)]
pub(super) struct TokioClock(tokio::time::Instant);
impl Default for TokioClock {
    fn default() -> Self {
        Self(tokio::time::Instant::now())
    }
}
impl Clock for TokioClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(tokio::time::sleep_until(self.0 + deadline))
    }
}

#[cfg(test)]
#[derive(Clone, Default)]
pub(super) struct Controlled {
    pub(super) clock: seedbox_test_time::ControlledClock,
    waker: std::sync::Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}
#[cfg(test)]
impl Controlled {
    pub(super) fn advance(&self, duration: Duration) {
        let mut waker = self.waker.lock().unwrap();
        self.clock.advance_by(duration).unwrap();
        let notify = waker.take();
        drop(waker);
        if let Some(notify) = notify {
            notify.wake();
        }
    }
}
#[cfg(test)]
impl Clock for Controlled {
    fn now(&self) -> Duration {
        self.clock.now()
    }
    fn sleep_until(&self, deadline: Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let this = self.clone();
        Box::pin(std::future::poll_fn(move |context| {
            let mut waker = this.waker.lock().unwrap();
            if this.clock.now() >= deadline {
                std::task::Poll::Ready(())
            } else {
                *waker = Some(context.waker().clone());
                std::task::Poll::Pending
            }
        }))
    }
}
