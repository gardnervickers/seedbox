//! Server-owned preparation deadline. Cancellation remains cooperative: the
//! owner waits for preparation cleanup rather than abandoning a blocking task.
use seedbox_application::execution::CancellationToken;
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) const DEFAULT_LIMIT: Duration = Duration::from_secs(15 * 60);

pub(crate) struct Budget {
    pub cancellation: CancellationToken,
    deadline: Instant,
    expired: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl Budget {
    pub fn start(
        parent: &CancellationToken,
        expired: Arc<AtomicBool>,
        limit: Duration,
    ) -> io::Result<Self> {
        let cancellation = parent.owned();
        let deadline = Instant::now()
            .checked_add(limit)
            .ok_or_else(|| io::Error::other("preparation deadline overflow"))?;
        let (stop, wake) = mpsc::channel();
        let timer_cancel = cancellation.clone();
        let timer_expired = expired.clone();
        let task = thread::Builder::new()
            .name("seedbox-preparation-deadline".into())
            .spawn(move || {
                if matches!(
                    wake.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ) {
                    timer_expired.store(true, Ordering::Release);
                    timer_cancel.cancel();
                }
            })?;
        Ok(Self {
            cancellation,
            deadline,
            expired,
            stop: Some(stop),
            task: Some(task),
        })
    }

    fn stop(&mut self) {
        if self.task.is_none() {
            return;
        }
        // Also enforce the deadline when the timer thread has not been scheduled.
        if Instant::now() >= self.deadline {
            self.expired.store(true, Ordering::Release);
            self.cancellation.cancel();
        }
        self.stop.take();
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }

    pub fn finish(mut self) -> bool {
        self.stop();
        self.expired.load(Ordering::Acquire)
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_cancels_owned_preparation_without_cancelling_the_simulation() {
        let parent = CancellationToken::default();
        let expired = Arc::new(AtomicBool::new(false));
        let budget = Budget::start(&parent, expired, Duration::from_millis(10)).unwrap();
        let limit = Instant::now() + Duration::from_secs(2);
        while !budget.cancellation.is_cancelled() && Instant::now() < limit {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(budget.cancellation.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(budget.finish());
    }

    #[test]
    fn finished_preparation_joins_its_timer_and_preserves_parent_cancellation() {
        let parent = CancellationToken::default();
        let expired = Arc::new(AtomicBool::new(false));
        let budget = Budget::start(&parent, expired, Duration::from_secs(2)).unwrap();
        let child = budget.cancellation.clone();
        assert!(!budget.finish());
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }
}
