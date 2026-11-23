//! Host execution inside a round. Only finished runs leave this queue.
//! Each lane accepts outputs into its own staging area. The Driver merges
//! those at the barrier in simulation order.
use super::batch::{BatchView, Flight, RunResult, Started};
use super::workers::Stream;
use super::*;
use seedbox_machine::host::{
    doorbell::{Coordinator, Doorbell, Wake},
    idle::{Action, Outcome},
};

type Job<M> = Box<dyn FnOnce(&mut <M as Machine>::Context) -> RunResult<M> + Send>;

pub(super) struct Runs<M: Machine> {
    flying: Vec<Flight<M>>,
    finished: VecDeque<RunResult<M>>,
    lent: Option<Stream<Job<M>, RunResult<M>>>,
    /// How the round waits for its runs in flight, decided once, before its
    /// first scan of them.
    wait: Option<Wait>,
}
/// A process has one doorbell and the doorbell one coordinator at a time.
enum Wait {
    /// This round holds the doorbell and parks on it.
    Doorbell(Coordinator<'static>),
    /// Another Driver's round holds the doorbell, or the process has none.
    /// The round sleeps in its oldest run, on that worker's own pipe.
    OldestRun,
}
impl<M: Machine> Runs<M> {
    pub(super) fn new() -> Self {
        Self {
            flying: Vec::new(),
            finished: VecDeque::new(),
            lent: None,
            wait: None,
        }
    }
}

impl<M> Driver<M>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + 'static,
{
    pub(super) fn submit_run(&mut self, view: BatchView, runs: &mut Runs<M>) {
        if self.splits(view.0) {
            match self.start_flight(view) {
                Started::Finished(result) => runs.finished.push_back(result),
                Started::Flying(flight) => runs.flying.push(flight),
            }
        } else {
            let (id, job) = self.run_job(view);
            self.workers.submit(
                runs.lent.get_or_insert_with(Stream::default),
                id,
                Box::new(job),
            );
        }
    }

    pub(super) fn next_run(&mut self, runs: &mut Runs<M>) -> RunResult<M> {
        if let Some(result) = runs.finished.pop_front() {
            return result;
        }
        if runs.flying.is_empty() {
            return self
                .workers
                .next(runs.lent.as_mut().expect("outstanding jobs"))
                .expect("job reports");
        }
        // Claimed before any scan, and never during one: a doorbell claimed
        // after a scan would not have sampled the rings that scan raced.
        let mut bell = match runs.wait.get_or_insert_with(|| {
            let bell = self.workers.doorbell().and_then(Doorbell::claim);
            bell.map_or(Wait::OldestRun, Wait::Doorbell)
        }) {
            Wait::Doorbell(bell) => Some(bell),
            Wait::OldestRun => None,
        };
        let mut wait = Instant::now();
        let mut attempt = 0;
        loop {
            // Mark before scanning. A reply racing any ring read will
            // either be observed here or invalidate the mark before park.
            if let Some(bell) = bell.as_mut() {
                bell.mark();
            }
            let mut i = 0;
            while i < runs.flying.len() {
                let result = match runs.flying[i].ready() {
                    Ok(false) => {
                        i += 1;
                        continue;
                    }
                    Ok(true) => runs.flying.remove(i).finish(self.workers.caller()),
                    Err(error) => runs.flying.remove(i).result(Err(error)),
                };
                runs.finished.push_back(result);
            }
            if let Some(result) = runs.finished.pop_front() {
                if attempt > 0 {
                    self.workers.idle().reset(Outcome::Progress);
                }
                return result;
            }
            if let Some(result) = runs.lent.as_mut().and_then(|s| self.workers.try_next(s)) {
                return result;
            }
            match self.workers.idle().idle(attempt, wait.elapsed()) {
                // Wait on the publication word sampled before the scan.
                action @ (Action::Spin | Action::Monitor(_)) => match bell.as_ref() {
                    Some(bell) => bell.spin(action),
                    None => std::hint::spin_loop(),
                },
                Action::Sleep => {
                    self.workers.idle().reset(if attempt == 0 {
                        Outcome::SleptAtOnce
                    } else {
                        Outcome::SleptAfterTrying
                    });
                    // A lent job reports through its channel, not the doorbell.
                    let lent = runs.lent.as_ref().is_some_and(|s| s.outstanding() > 0);
                    if let Some(bell) = bell.as_mut().filter(|_| !lent) {
                        match bell.park(|ticket| {
                            runs.flying
                                .iter_mut()
                                .map(|f| f.watch_reply(ticket))
                                .collect()
                        }) {
                            Ok(Wake::Published) => {
                                attempt = 0;
                                wait = Instant::now();
                                continue;
                            }
                            Ok(Wake::Exited(i)) => {
                                return runs.flying.remove(i).finish(self.workers.caller())
                            }
                            Ok(Wake::Unwatched) | Err(_) => {}
                        }
                    }
                    // Without the doorbell, sleep in the oldest run. Its
                    // finish waits on that worker's ring and pipe alone, so
                    // it needs no doorbell and ends if the worker exits. The
                    // other runs keep their replies in their rings, where the
                    // next scan finds them.
                    return runs.flying.remove(0).finish(self.workers.caller());
                }
            }
            attempt += 1;
        }
    }
}
