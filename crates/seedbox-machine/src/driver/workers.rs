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
        _machine: usize,
        job: impl FnOnce(&mut C) + Send + 'static,
    ) -> Result<(), Error<E>> {
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
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc, sync::Mutex};

    struct LocalContext {
        owner: thread::ThreadId,
        calls: Rc<Cell<usize>>,
        dropped: Arc<Mutex<Vec<usize>>>,
    }

    impl Drop for LocalContext {
        fn drop(&mut self) {
            assert_eq!(self.owner, thread::current().id());
            self.dropped.lock().unwrap().push(self.calls.get());
        }
    }

    #[test]
    fn non_send_contexts_persist_and_drop_on_their_own_threads() {
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let record = dropped.clone();
        let workers = Workers::new(2, move || {
            Ok::<_, io::Error>(LocalContext {
                owner: thread::current().id(),
                calls: Rc::new(Cell::new(0)),
                dropped: record.clone(),
            })
        })
        .unwrap();
        for machine in 0..8 {
            workers
                .submit::<io::Error>(machine, |context| {
                    assert_eq!(context.owner, thread::current().id());
                    context.calls.set(context.calls.get() + 1);
                })
                .unwrap();
        }
        drop(workers); // All queued work and context destruction must finish.
        let calls = dropped.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls.iter().sum::<usize>(), 8);
    }

    #[test]
    fn an_idle_worker_steals_jobs_from_an_occupied_workers_local_queue() {
        let workers = Workers::new(2, || Ok::<_, io::Error>(())).unwrap();
        let (release, wait) = mpsc::channel();
        let (ready, entered) = mpsc::channel();
        let (finished, stolen) = mpsc::channel();
        workers
            .submit::<io::Error>(0, move |_| {
                let owner = thread::current().id();
                for _ in 0..8 {
                    let finished = finished.clone();
                    // Spawn from inside the pool to put jobs on this worker's
                    // local deque. Its peer must steal them while it is blocked.
                    rayon::spawn(move || {
                        execute(Box::new(move |_: &mut ()| {
                            finished.send(thread::current().id() != owner).unwrap();
                        }));
                    });
                }
                ready.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            })
            .unwrap();
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let results = (0..8)
            .map(|_| stolen.recv_timeout(std::time::Duration::from_secs(5)))
            .collect::<Vec<_>>();
        let _ = release.send(());
        drop(workers);
        assert!(results.into_iter().all(|result| result == Ok(true)));
    }

    #[test]
    fn reentrant_rayon_work_defers_context_use_until_the_outer_call_returns() {
        let workers = Workers::new(1, || Ok::<_, io::Error>(Rc::new(Cell::new(0)))).unwrap();
        let (ready, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (finished, done) = mpsc::channel();
        workers
            .submit::<io::Error>(0, move |context| {
                context.set(1);
                ready.send(()).unwrap();
                wait.recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                assert_eq!(rayon::yield_now(), Some(rayon::Yield::Executed));
                // The yielded driver job ran its dispatcher, but cannot borrow
                // this context until this callback returns.
                assert_eq!(context.get(), 1);
                context.set(2);
            })
            .unwrap();
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        workers
            .submit::<io::Error>(1, move |context| {
                assert_eq!(context.get(), 2);
                finished.send(()).unwrap();
            })
            .unwrap();
        release.send(()).unwrap();
        let result = done.recv_timeout(std::time::Duration::from_secs(5));
        drop(workers);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn nested_pools_use_their_own_thread_local_contexts() {
        let workers = Workers::new(1, || Ok::<_, io::Error>(Rc::new(Cell::new(7)))).unwrap();
        let (finished, done) = mpsc::channel();
        workers
            .submit::<io::Error>(0, move |outer| {
                let nested = Workers::new(2, || {
                    Ok::<_, io::Error>(Rc::new(RefCell::new(String::from("inner"))))
                })
                .unwrap();
                for id in 0..4 {
                    nested
                        .submit::<io::Error>(id, |inner| assert_eq!(&*inner.borrow(), "inner"))
                        .unwrap();
                }
                drop(nested);
                assert_eq!(outer.get(), 7);
                finished.send(()).unwrap();
            })
            .unwrap();
        let result = done.recv_timeout(std::time::Duration::from_secs(5));
        drop(workers);
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn unwinding_drains_a_large_reentrant_backlog_without_recursing() {
        const COUNT: usize = 4096;
        let workers = Workers::new(1, || Ok::<_, io::Error>(Rc::new(Cell::new(0usize)))).unwrap();
        let (finished, done) = mpsc::channel();
        workers.pool.as_ref().unwrap().spawn(move || {
            let panic = catch_unwind(AssertUnwindSafe(|| {
                execute(Box::new(|context: &mut Rc<Cell<usize>>| {
                    for _ in 0..COUNT {
                        rayon::spawn(|| {
                            execute(Box::new(|context: &mut Rc<Cell<usize>>| {
                                context.set(context.get() + 1);
                            }));
                        });
                    }
                    for _ in 0..COUNT {
                        assert_eq!(rayon::yield_now(), Some(rayon::Yield::Executed));
                    }
                    assert_eq!(context.get(), 0);
                    panic!("fixture panic after reentrant dispatch");
                }));
            }));
            assert!(panic.is_err());
            execute(Box::new(move |context: &mut Rc<Cell<usize>>| {
                finished.send(context.get()).unwrap();
            }));
        });
        let result = done.recv_timeout(std::time::Duration::from_secs(5));
        drop(workers);
        assert_eq!(result, Ok(COUNT));
    }

    #[test]
    fn partial_startup_failure_joins_and_drops_started_contexts() {
        for panic in [false, true] {
            let dropped = Arc::new(Mutex::new(Vec::new()));
            let record = dropped.clone();
            let started = Mutex::new(0);
            let result = Workers::new(3, move || {
                let mut count = started.lock().unwrap();
                *count += 1;
                if *count == 2 {
                    if panic {
                        panic!("fixture context panic");
                    }
                    return Err(io::Error::other("fixture context failure"));
                }
                Ok(LocalContext {
                    owner: thread::current().id(),
                    calls: Rc::new(Cell::new(0)),
                    dropped: record.clone(),
                })
            });
            if panic {
                assert!(matches!(result, Err(Error::Worker { worker: 1, .. })));
            } else {
                assert!(matches!(result, Err(Error::Context { worker: 1, .. })));
            }
            assert_eq!(*dropped.lock().unwrap(), [0]);
        }
    }
}
