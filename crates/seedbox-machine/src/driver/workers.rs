//! Persistent execution threads. Contexts are constructed, used, and dropped
//! on their owning thread; they need not implement Send.

use super::Error;
use std::{
    io,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{mpsc, Arc},
    thread::{self, JoinHandle},
};

type Job<C> = Box<dyn FnOnce(&mut C) + Send>;

pub(super) struct Workers<C> {
    senders: Vec<mpsc::Sender<Job<C>>>,
    threads: Vec<JoinHandle<()>>,
}

impl<C: 'static> Workers<C> {
    pub(super) fn new<E: Send + 'static>(
        count: usize,
        context: impl Fn() -> Result<C, E> + Send + Sync + 'static,
    ) -> Result<Self, Error<E>> {
        let context = Arc::new(context);
        let mut pool = Self {
            senders: Vec::new(),
            threads: Vec::new(),
        };
        for worker in 0..count {
            let factory = context.clone();
            let (send, receive) = mpsc::channel::<Job<C>>();
            let (ready, started) = mpsc::sync_channel(1);
            let thread = thread::Builder::new()
                .name(format!("seedbox-machine-{worker}"))
                .spawn(move || {
                    let mut context = match catch_unwind(AssertUnwindSafe(|| factory())) {
                        Ok(Ok(context)) => context,
                        Ok(Err(source)) => {
                            let _ = ready.send(Err(Error::Context { worker, source }));
                            return;
                        }
                        Err(_) => {
                            let _ = ready.send(Err(Error::Worker {
                                worker,
                                source: io::Error::other("worker context factory panicked"),
                            }));
                            return;
                        }
                    };
                    if ready.send(Ok(())).is_err() {
                        return;
                    }
                    for job in receive {
                        job(&mut context);
                    }
                })
                .map_err(|source| Error::Worker { worker, source })?;
            pool.senders.push(send);
            pool.threads.push(thread);
            started.recv().map_err(|_| Error::Worker {
                worker,
                source: io::Error::other("worker stopped during context creation"),
            })??;
        }
        Ok(pool)
    }

    pub(super) fn submit<E>(
        &self,
        machine: usize,
        job: impl FnOnce(&mut C) + Send + 'static,
    ) -> Result<(), Error<E>> {
        let worker = machine % self.senders.len();
        self.senders[worker]
            .send(Box::new(job))
            .map_err(|_| Error::Worker {
                worker,
                source: io::Error::other("worker stopped before accepting execution"),
            })
    }
}

impl<C> Drop for Workers<C> {
    fn drop(&mut self) {
        // Close every queue before joining, including on partial startup failure.
        self.senders.clear();
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
        assert_eq!(*dropped.lock().unwrap(), [4, 4]);
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
