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

#[derive(Clone)]
enum Deadline {
    Host(Instant),
    // Tests decide expiration explicitly; scheduler delay cannot fire it.
    #[cfg(test)]
    Controlled(Arc<AtomicBool>),
}

impl Deadline {
    fn elapsed(&self) -> bool {
        match self {
            Self::Host(deadline) => Instant::now() >= *deadline,
            #[cfg(test)]
            Self::Controlled(elapsed) => elapsed.load(Ordering::Acquire),
        }
    }

    fn wait(&self, wake: mpsc::Receiver<()>) -> bool {
        match self {
            Self::Host(deadline) => matches!(
                wake.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            #[cfg(test)]
            Self::Controlled(_) => {
                // The finish/disconnect signal is the only wake. The supplied
                // clock state, not real elapsed time, decides expiration.
                let _ = wake.recv();
                self.elapsed()
            }
        }
    }
}

pub(crate) struct Budget {
    pub cancellation: CancellationToken,
    deadline: Deadline,
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
        let deadline = Instant::now()
            .checked_add(limit)
            .ok_or_else(|| io::Error::other("preparation deadline overflow"))?;
        Self::start_with_deadline(parent, expired, Deadline::Host(deadline))
    }

    fn start_with_deadline(
        parent: &CancellationToken,
        expired: Arc<AtomicBool>,
        deadline: Deadline,
    ) -> io::Result<Self> {
        let cancellation = parent.owned();
        let timer_deadline = deadline.clone();
        let (stop, wake) = mpsc::channel();
        let timer_cancel = cancellation.clone();
        let timer_expired = expired.clone();
        let task = thread::Builder::new()
            .name("seedbox-preparation-deadline".into())
            .spawn(move || {
                if timer_deadline.wait(wake) {
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
        if self.deadline.elapsed() {
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
    fn controlled_expiration_is_enforced_when_finishing_preparation() {
        let parent = CancellationToken::default();
        let expired = Arc::new(AtomicBool::new(false));
        let elapsed = Arc::new(AtomicBool::new(false));
        let budget = Budget::start_with_deadline(
            &parent,
            expired.clone(),
            Deadline::Controlled(elapsed.clone()),
        )
        .unwrap();
        let child = budget.cancellation.clone();
        elapsed.store(true, Ordering::Release);
        assert!(budget.finish());
        assert!(expired.load(Ordering::Acquire));
        assert!(child.is_cancelled());
        assert!(!parent.is_cancelled());
    }

    #[test]
    fn finished_preparation_joins_its_timer_and_preserves_parent_cancellation() {
        let parent = CancellationToken::default();
        let expired = Arc::new(AtomicBool::new(false));
        let elapsed = Arc::new(AtomicBool::new(false));
        let budget =
            Budget::start_with_deadline(&parent, expired, Deadline::Controlled(elapsed)).unwrap();
        let child = budget.cancellation.clone();
        assert!(!budget.finish());
        assert!(!child.is_cancelled());
        parent.cancel();
        assert!(child.is_cancelled());
    }
}
