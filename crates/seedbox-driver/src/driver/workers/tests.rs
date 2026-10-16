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
            seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on wait; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&wait)
                .unwrap();
        })
        .unwrap();
    seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on entered; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&entered)
        .unwrap();
    let results = (0..8)
        .map(|_| seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on stolen; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&stolen))
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
            seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on wait; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&wait)
                .unwrap();
            assert_eq!(rayon::yield_now(), Some(rayon::Yield::Executed));
            // The yielded driver job ran its dispatcher, but cannot borrow
            // this context until this callback returns.
            assert_eq!(context.get(), 1);
            context.set(2);
        })
        .unwrap();
    seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on entered; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&entered)
        .unwrap();
    workers
        .submit::<io::Error>(1, move |context| {
            assert_eq!(context.get(), 2);
            finished.send(()).unwrap();
        })
        .unwrap();
    release.send(()).unwrap();
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&done);
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
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&done);
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
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(5)).recv(&done);
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
