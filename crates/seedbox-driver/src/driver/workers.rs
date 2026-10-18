//! Persistent work-stealing execution threads. Contexts are constructed, used,
//! and dropped on their owning thread; they need not implement Send.

use super::Error;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::{
    any::Any,
    cell::{Cell, RefCell},
    collections::VecDeque,
    io,
    marker::PhantomData,
    panic::{catch_unwind, resume_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};

type Job<C> = Box<dyn FnOnce(&mut C) + Send>;
type Deferred = Box<dyn FnOnce() + Send>;

thread_local! {
    // Only the dedicated pool's own threads receive driver jobs. Type erasure
    // permits different context types without moving a context between threads.
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

pub(super) struct Workers<C> {
    pool: Option<ThreadPool>,
    threads: Vec<JoinHandle<()>>,
    context: PhantomData<fn() -> C>,
    // Private unit-test witness after the submitted closure has published its
    // result. Not present in runtime builds or part of Driver snapshots.
    #[cfg(test)]
    pub(super) result_observer: Option<Arc<dyn Fn(usize) + Send + Sync>>,
}

impl<C: 'static> Workers<C> {
    pub(super) fn new<E: Send + 'static>(
        count: usize,
        context: impl Fn() -> Result<C, E> + Send + Sync + 'static,
    ) -> Result<Self, Error<E>> {
        let context = Arc::new(context);
        let mut threads = Vec::new();
        let mut startup_error = None;
        let pool = ThreadPoolBuilder::new()
            .num_threads(count)
            .spawn_handler(|rayon_thread| {
                let worker = rayon_thread.index();
                let factory = context.clone();
                let (ready, started) = mpsc::sync_channel(1);
                let thread = thread::Builder::new()
                    .name(format!("seedbox-machine-{worker}"))
                    .spawn(move || {
                        let context = match catch_unwind(AssertUnwindSafe(|| factory())) {
                            Ok(Ok(context)) => {
                                CONTEXT.with(|slot| *slot.borrow_mut() = Some(Box::new(context)));
                                Ok(())
                            }
                            Ok(Err(source)) => Err(Error::Context { worker, source }),
                            Err(_) => Err(Error::Worker {
                                worker,
                                source: io::Error::other("worker context factory panicked"),
                            }),
                        };
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
                context: PhantomData,
                #[cfg(test)]
                result_observer: None,
            }),
            Err(source) => {
                let worker = threads.len();
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

    pub(super) fn submit<E>(
        &self,
        machine: usize,
        job: impl FnOnce(&mut C) + Send + 'static,
    ) -> Result<(), Error<E>> {
        let _ = machine; // Identity is observed only by the private test hook.
        #[cfg(test)]
        let job = {
            let observer = self.result_observer.clone();
            move |context: &mut C| {
                job(context);
                if let Some(observer) = observer {
                    observer(machine);
                }
            }
        };
        self.pool
            .as_ref()
            .expect("live worker pool")
            .spawn(|| execute(Box::new(job)));
        Ok(())
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
