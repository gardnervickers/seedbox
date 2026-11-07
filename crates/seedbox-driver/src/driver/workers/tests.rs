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

/// Queue a job straight onto the pool, for the pool's own mechanics.
fn submit<C: 'static>(workers: &Workers<C>, job: impl FnOnce(&mut C) + Send + 'static) {
    workers
        .pool
        .as_ref()
        .expect("a pool beside the calling thread")
        .spawn(move || execute(Box::new(job)));
}

fn guard(waiting_for: &str) -> seedbox_test_time::HangGuard {
    seedbox_test_time::HangGuard::new(
        format!("worker {waiting_for}; retain the existing loaded-host deadlock budget, never timed absence"),
        std::time::Duration::from_secs(30),
    )
}

#[test]
fn non_send_contexts_persist_and_drop_on_their_own_threads() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let record = dropped.clone();
    let mut workers = Workers::new(3, move || {
        Ok::<_, io::Error>(LocalContext {
            owner: thread::current().id(),
            calls: Rc::new(Cell::new(0)),
            dropped: record.clone(),
        })
    })
    .unwrap();
    for _ in 0..4 {
        let jobs = (0..4)
            .map(|machine| {
                (machine, |context: &mut LocalContext| {
                    assert_eq!(context.owner, thread::current().id());
                    context.calls.set(context.calls.get() + 1);
                })
            })
            .collect();
        workers.run(jobs);
    }
    drop(workers); // All queued work and context destruction must finish.
    let calls = dropped.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls.iter().sum::<usize>(), 16);
}

#[test]
fn one_job_batches_and_one_worker_stay_on_the_calling_thread() {
    let here = thread::current().id();
    let job = |machine| (machine, move |_: &mut ()| (machine, thread::current().id()));
    let mut pooled = Workers::new(2, || Ok::<_, io::Error>(())).unwrap();
    assert_eq!(pooled.run(vec![job(0)]), [(0, here)]);
    let mut single = Workers::new(1, || Ok::<_, io::Error>(())).unwrap();
    assert!(single.pool.is_none());
    assert_eq!(
        single.run((0..4).map(job).collect()),
        (0..4).map(|machine| (machine, here)).collect::<Vec<_>>()
    );
}

#[test]
fn pool_threads_run_the_rest_of_a_batch_beside_the_calling_thread() {
    let mut workers = Workers::new(2, || Ok::<_, io::Error>(())).unwrap();
    let (started, pooled) = mpsc::channel();
    // The first job holds the calling thread until another thread has taken
    // the second, so only the pool can run it.
    let first: Box<dyn FnOnce(&mut ()) -> thread::ThreadId + Send> = Box::new(move |_| {
        guard("pool entry on pooled").recv(&pooled).unwrap();
        thread::current().id()
    });
    let second: Box<dyn FnOnce(&mut ()) -> thread::ThreadId + Send> = Box::new(move |_| {
        started.send(()).unwrap();
        thread::current().id()
    });
    let threads = workers.run(vec![(0, first), (1, second)]);
    assert_eq!(threads[0], thread::current().id());
    assert_ne!(threads[1], threads[0]);
}

#[test]
fn the_calling_thread_runs_every_job_no_pool_thread_has_claimed() {
    let mut workers = Workers::new(2, || Ok::<_, io::Error>(())).unwrap();
    let (release, wait) = mpsc::channel::<()>();
    let (ready, entered) = mpsc::channel();
    submit(&workers, move |_| {
        ready.send(()).unwrap();
        guard("hold on wait").recv(&wait).unwrap();
    });
    guard("hold entry on entered").recv(&entered).unwrap();
    // The only pool thread is busy, so the batch finishes without it.
    let order = Arc::new(Mutex::new(Vec::new()));
    let jobs = (0..3)
        .map(|machine| {
            let order = order.clone();
            (machine, move |_: &mut ()| {
                order.lock().unwrap().push(machine);
                thread::current().id()
            })
        })
        .collect();
    let (finished, done) = mpsc::channel();
    let batch = thread::spawn(move || {
        let threads = workers.run(jobs);
        finished
            .send(threads.iter().all(|&id| id == thread::current().id()))
            .unwrap();
        workers
    });
    let result = guard("batch completion on done").recv(&done);
    release.send(()).unwrap();
    drop(batch.join().unwrap());
    assert_eq!(result, Ok(true));
    assert_eq!(*order.lock().unwrap(), [0, 2, 1]);
}

#[test]
fn an_idle_worker_steals_jobs_from_an_occupied_workers_local_queue() {
    let workers = Workers::new(3, || Ok::<_, io::Error>(())).unwrap();
    let (release, wait) = mpsc::channel();
    let (ready, entered) = mpsc::channel();
    let (finished, stolen) = mpsc::channel();
    submit(&workers, move |_| {
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
        seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on wait; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&wait)
            .unwrap();
    });
    seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on entered; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&entered)
        .unwrap();
    let results = (0..8)
        .map(|_| seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on stolen; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&stolen))
        .collect::<Vec<_>>();
    let _ = release.send(());
    drop(workers);
    assert!(results.into_iter().all(|result| result == Ok(true)));
}

#[test]
fn reentrant_rayon_work_defers_context_use_until_the_outer_call_returns() {
    let workers = Workers::new(2, || Ok::<_, io::Error>(Rc::new(Cell::new(0)))).unwrap();
    let (ready, entered) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let (finished, done) = mpsc::channel();
    submit(&workers, move |context| {
        context.set(1);
        ready.send(()).unwrap();
        seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on wait; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&wait)
            .unwrap();
        assert_eq!(rayon::yield_now(), Some(rayon::Yield::Executed));
        // The yielded driver job ran its dispatcher, but cannot borrow
        // this context until this callback returns.
        assert_eq!(context.get(), 1);
        context.set(2);
    });
    seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on entered; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&entered)
        .unwrap();
    submit(&workers, move |context| {
        assert_eq!(context.get(), 2);
        finished.send(()).unwrap();
    });
    release.send(()).unwrap();
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&done);
    drop(workers);
    assert_eq!(result, Ok(()));
}

#[test]
fn nested_pools_use_their_own_thread_local_contexts() {
    let workers = Workers::new(2, || Ok::<_, io::Error>(Rc::new(Cell::new(7)))).unwrap();
    let (finished, done) = mpsc::channel();
    submit(&workers, move |outer| {
        let mut nested = Workers::new(2, || {
            Ok::<_, io::Error>(Rc::new(RefCell::new(String::from("inner"))))
        })
        .unwrap();
        let jobs = (0..4)
            .map(|id| {
                (id, |inner: &mut Rc<RefCell<String>>| {
                    assert_eq!(&*inner.borrow(), "inner")
                })
            })
            .collect();
        nested.run(jobs);
        drop(nested);
        assert_eq!(outer.get(), 7);
        finished.send(()).unwrap();
    });
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&done);
    drop(workers);
    assert_eq!(result, Ok(()));
}

#[test]
fn unwinding_drains_a_large_reentrant_backlog_without_recursing() {
    const COUNT: usize = 4096;
    let workers = Workers::new(2, || Ok::<_, io::Error>(Rc::new(Cell::new(0usize)))).unwrap();
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
    let result = seedbox_test_time::HangGuard::new("Rayon worker entry/proceed/completion on done; retain the existing loaded-host deadlock budget, never timed absence", std::time::Duration::from_secs(30)).recv(&done);
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

#[test]
fn streamed_jobs_return_once_each_and_follow_ups_can_join() {
    for count in [1, 3] {
        let _guard = guard("stream");
        let caller = thread::current().id();
        let mut workers = Workers::new(count, || Ok::<_, io::Error>(())).unwrap();
        type Job = Box<dyn FnOnce(&mut ()) -> (usize, thread::ThreadId) + Send>;
        let job = |machine: usize| -> Job { Box::new(move |_| (machine, thread::current().id())) };
        let mut stream = Stream::default();
        for machine in 0..4 {
            workers.submit(&mut stream, machine, job(machine));
        }
        let mut seen = Vec::new();
        while let Some((machine, thread)) = workers.next(&mut stream) {
            if count == 1 {
                assert_eq!(thread, caller, "one worker runs every job itself");
            }
            // A finished job's follow-up joins the same stream.
            if machine < 4 {
                workers.submit(&mut stream, machine + 4, job(machine + 4));
            }
            seen.push(machine);
        }
        seen.sort();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
    }
}
