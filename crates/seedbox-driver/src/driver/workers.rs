//! Machine execution threads. The calling thread is worker 0: it runs the
//! first job of every batch itself, then any job no pool thread has claimed,
//! so a one-job batch never leaves it and one worker needs no pool. The other
//! workers form a persistent work-stealing pool. Contexts are constructed,
//! used and dropped on their owning thread, so they need not implement Send;
//! worker 0's context moves only with its owner, which is therefore Send only
//! when the context is.

use super::Error;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::VecDeque,
    io,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc, Mutex, PoisonError},
    thread::{self, JoinHandle},
};

type Job<C> = Box<dyn FnOnce(&mut C) + Send>;
type Deferred = Box<dyn FnOnce() + Send>;

thread_local! {
    // Only pool threads keep a context here; the calling thread passes worker
    // 0's context directly. Type erasure permits different context types
    // without moving a pool context between threads.
    static CONTEXT: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
    static DEFERRED: RefCell<VecDeque<Deferred>> = const { RefCell::new(VecDeque::new()) };
    static DRAINING: Cell<bool> = const { Cell::new(false) };
}

struct LocalContext;

impl Drop for LocalContext {
    fn drop(&mut self) {
        CONTEXT.with(|slot| drop(slot.borrow_mut().take()));
    }
}

fn execute<C: 'static>(job: Job<C>) {
    let mut job = Some(job);
    let result = catch_unwind(AssertUnwindSafe(|| {
        CONTEXT.with(|slot| {
            let Ok(mut slot) = slot.try_borrow_mut() else {
                return false;
            };
            let context = slot
                .as_mut()
                .expect("initialized worker context")
                .downcast_mut::<C>()
                .expect("job belongs to this worker pool");
            job.take().expect("unconsumed job")(context);
            true
        })
    }));
    if matches!(result, Ok(false)) {
        // Machine code can enter another Rayon scope, allowing this worker to
        // reenter its own queue. Never lend the context twice. These jobs remain
        // owned here until the outer call releases its borrow, even on unwind.
        let job = job.expect("deferred job");
        DEFERRED.with(|queue| queue.borrow_mut().push_back(Box::new(|| execute(job))));
        return;
    }
    let mut panic = result.err();
    if DRAINING.with(|draining| !draining.replace(true)) {
        // Only the outer dispatcher drains: each deferred callback can itself
        // yield, but the backlog never becomes a recursive drain stack.
        while let Some(job) = DEFERRED.with(|queue| queue.borrow_mut().pop_front()) {
            let result = catch_unwind(AssertUnwindSafe(job));
            if panic.is_none() {
                panic = result.err();
            }
        }
        DRAINING.with(|draining| draining.set(false));
    }
    if let Some(panic) = panic {
        resume_unwind(panic);
    }
}

/// Build one worker's context, reporting a failure or panic as that worker's.
fn start<C, E>(worker: usize, factory: impl FnOnce() -> Result<C, E>) -> Result<C, Error<E>> {
    match catch_unwind(AssertUnwindSafe(factory)) {
        Ok(Ok(context)) => Ok(context),
        Ok(Err(source)) => Err(Error::Context { worker, source }),
        Err(_) => Err(Error::Worker {
            worker,
            source: io::Error::other("worker context factory panicked"),
        }),
    }
}

/// A batch job and its machine, until one thread claims it.
type Slot<F> = Mutex<Option<(usize, F)>>;

/// Take a job unless another thread already has.
fn claim<F>(slot: &Slot<F>) -> Option<(usize, F)> {
    slot.lock().unwrap_or_else(PoisonError::into_inner).take()
}

/// Jobs submitted one at a time whose results are taken as they finish
/// ([`Workers::submit`], [`Workers::next`]).
pub(super) struct Stream<F, T> {
    /// Submitted jobs, newest last; pool threads empty the ones they claim.
    slots: Vec<Arc<Slot<F>>>,
    send: mpsc::Sender<T>,
    receive: mpsc::Receiver<T>,
    outstanding: usize,
}

impl<F, T> Default for Stream<F, T> {
    fn default() -> Self {
        let (send, receive) = mpsc::channel();
        Self {
            slots: Vec::new(),
            send,
            receive,
            outstanding: 0,
        }
    }
}

pub(super) struct Workers<C> {
    /// Workers 1 and up; none when the calling thread is the only worker.
    pool: Option<ThreadPool>,
    threads: Vec<JoinHandle<()>>,
    /// Worker 0's context, used on the calling thread.
    caller: C,
    // Private unit-test witness after a job has published its result. Not
    // present in runtime builds or part of Driver snapshots.
    #[cfg(test)]
    pub(super) result_observer: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl<C: 'static> Workers<C> {
    /// Start `count` workers in order: the calling thread's context first, as
    /// worker 0, then a pool of `count - 1` threads.
    pub(super) fn new<E: Send + 'static>(
        count: usize,
        context: impl Fn() -> Result<C, E> + Send + Sync + 'static,
    ) -> Result<Self, Error<E>> {
        let caller = start(0, &context)?;
        let mut threads = Vec::new();
        if count <= 1 {
            return Ok(Self {
                pool: None,
                threads,
                caller,
                #[cfg(test)]
                result_observer: None,
            });
        }
        let context = Arc::new(context);
        let mut startup_error = None;
        let pool = ThreadPoolBuilder::new()
            .num_threads(count - 1)
            .spawn_handler(|rayon_thread| {
                let worker = rayon_thread.index() + 1;
                let factory = context.clone();
                let (ready, started) = mpsc::sync_channel(1);
                let thread = thread::Builder::new()
                    .name(format!("seedbox-machine-{worker}"))
                    .spawn(move || {
                        let context = start(worker, || factory()).map(|context| {
                            CONTEXT.with(|slot| *slot.borrow_mut() = Some(Box::new(context)));
                        });
                        let _context = LocalContext;
                        let _ = ready.send(context);
                        // Even failed initialization must enter the Rayon loop
                        // so its registry can terminate every started worker.
                        rayon_thread.run();
                    })?;
                threads.push(thread);
                // Preserve ordered initialization and first-failure selection.
                // No execution jobs are admitted until every worker is ready.
                match started.recv() {
                    Ok(Ok(())) => Ok(()),
                    result => {
                        startup_error = Some(match result {
                            Ok(Err(error)) => error,
                            Err(_) => Error::Worker {
                                worker,
                                source: io::Error::other("worker stopped during context creation"),
                            },
                            Ok(Ok(())) => unreachable!(),
                        });
                        Err(io::Error::other("worker context initialization failed"))
                    }
                }
            })
            .build();
        match pool {
            Ok(pool) => Ok(Self {
                pool: Some(pool),
                threads,
                caller,
                #[cfg(test)]
                result_observer: None,
            }),
            Err(source) => {
                let worker = threads.len() + 1;
                // Rayon signals termination on failed build, but does not join.
                for thread in threads {
                    let _ = thread.join();
                }
                Err(startup_error.unwrap_or_else(|| Error::Worker {
                    worker,
                    source: io::Error::other(source),
                }))
            }
        }
    }

    /// Run every job once and return the results in job order, after all of
    /// them have finished. The calling thread runs the first job, then, from
    /// the back, each job no pool thread has claimed yet; pool threads claim
    /// the others from the front. `machine` names a job only for the private
    /// test hook.
    pub(super) fn run<F, T>(&mut self, jobs: Vec<(usize, F)>) -> Vec<T>
    where
        F: FnOnce(&mut C) -> T + Send + 'static,
        T: Send + 'static,
    {
        let mut jobs = jobs.into_iter();
        let Some(first) = jobs.next() else {
            return Vec::new();
        };
        let Some(pool) = self.pool.as_ref().filter(|_| jobs.len() > 0) else {
            return std::iter::once(first)
                .chain(jobs)
                .map(|job| self.run_here(job))
                .collect();
        };
        let slots: Arc<[Slot<F>]> = jobs.map(|job| Mutex::new(Some(job))).collect();
        let (send, receive) = mpsc::channel();
        for index in 0..slots.len() {
            let (slots, send, published) = (slots.clone(), send.clone(), self.published());
            pool.spawn(move || {
                if let Some((machine, job)) = claim(&slots[index]) {
                    execute(Box::new(move |context: &mut C| {
                        let _ = send.send((index, job(context)));
                        published(machine);
                    }));
                }
            });
        }
        drop(send);
        let mut results = Vec::with_capacity(slots.len() + 1);
        results.push(Some(self.run_here(first)));
        results.extend(slots.iter().map(|_| None));
        let mut pooled = slots.len();
        for index in (0..slots.len()).rev() {
            if let Some(job) = claim(&slots[index]) {
                results[index + 1] = Some(self.run_here(job));
                pooled -= 1;
            }
        }
        // Wait only for the jobs pool threads claimed: a task whose job this
        // thread took finds its slot empty and does nothing.
        for (index, result) in receive.iter().take(pooled) {
            results[index + 1] = Some(result);
        }
        results
            .into_iter()
            .map(|result| result.expect("every job returns its result"))
            .collect()
    }

    /// Submit one job to a stream of jobs whose results are taken as they
    /// finish. A pool thread may claim it; otherwise the calling thread runs
    /// it while it waits in [`Self::next`].
    pub(super) fn submit<F, T>(&self, stream: &mut Stream<F, T>, machine: usize, job: F)
    where
        F: FnOnce(&mut C) -> T + Send + 'static,
        T: Send + 'static,
    {
        let slot: Arc<Slot<F>> = Arc::new(Mutex::new(Some((machine, job))));
        if let Some(pool) = &self.pool {
            let (claimed, send, published) = (slot.clone(), stream.send.clone(), self.published());
            pool.spawn(move || {
                if let Some((machine, job)) = claim(&claimed) {
                    execute(Box::new(move |context: &mut C| {
                        let _ = send.send(job(context));
                        published(machine);
                    }));
                }
            });
        }
        stream.slots.push(slot);
        stream.outstanding += 1;
    }

    /// The next finished result of the stream, in completion order; `None`
    /// once every submitted job has returned. While no result is ready, the
    /// calling thread runs the newest job no pool thread has claimed.
    pub(super) fn next<F, T>(&mut self, stream: &mut Stream<F, T>) -> Option<T>
    where
        F: FnOnce(&mut C) -> T,
    {
        if stream.outstanding == 0 {
            return None;
        }
        stream.outstanding -= 1;
        if let Ok(result) = stream.receive.try_recv() {
            return Some(result);
        }
        while let Some(slot) = stream.slots.pop() {
            if let Some(job) = claim(&slot) {
                return Some(self.run_here(job));
            }
        }
        // Every outstanding job is on a pool thread, which always sends.
        Some(stream.receive.recv().expect("pool job reports"))
    }

    fn run_here<F, T>(&mut self, (machine, job): (usize, F)) -> T
    where
        F: FnOnce(&mut C) -> T,
    {
        let result = job(&mut self.caller);
        self.published()(machine);
        result
    }

    /// Calls the private test hook, if any, once a job has published its result.
    fn published(&self) -> impl Fn(usize) + Send + 'static {
        #[cfg(test)]
        let observer = self.result_observer.clone();
        move |machine| {
            #[cfg(test)]
            if let Some(observer) = &observer {
                observer(machine);
            }
            let _ = machine;
        }
    }
}

impl<C> Drop for Workers<C> {
    fn drop(&mut self) {
        // Dropping Rayon signals shutdown; joining also waits for accepted jobs
        // and thread-local context destruction, including partial startup.
        drop(self.pool.take());
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests;
