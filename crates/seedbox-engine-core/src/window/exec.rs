//! Executors: run each round's assignments, serially or on persistent
//! workers, then commit. Host timing is observed here and never decides
//! anything.

use super::plan::{Assignment, Error, Outcome, Report, RoundPlan, Router, Step, World};
use std::hash::Hash;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A machine the scheduler can run for one round.
pub trait Machine<P> {
    /// Per-worker execution state created on the worker's own thread, such
    /// as a thread-local timebase.
    type Context;

    /// Execute the assignment: deliver its events at their delivery points,
    /// buffer outputs at their send times, and stop where it says.
    fn run(&mut self, cx: &mut Self::Context, a: &Assignment<P>) -> Result<Report<P>, String>;
}

/// Host observations of a run. Never part of its identity.
#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub rounds: u64,
    pub machine_runs: u64,
    pub window_ns_sum: u128,
    pub window_ns_min: u64,
    pub window_ns_max: u64,
    pub wall: Duration,
    /// Planning and commit on the coordinator.
    pub coordinator: Duration,
    /// Machine execution, summed over workers.
    pub machine: Duration,
    /// From dispatch until the last worker finished, summed over rounds.
    pub round_wall: Duration,
    /// Runs on a different pool thread than the machine's previous run,
    /// counting each machine's first run.
    pub migrations: u64,
}

impl Stats {
    fn round<P>(&mut self, plan: &RoundPlan<P>) {
        let w = plan.end - plan.start;
        self.window_ns_min = if self.rounds == 0 {
            w
        } else {
            self.window_ns_min.min(w)
        };
        self.window_ns_max = self.window_ns_max.max(w);
        self.window_ns_sum += w as u128;
        self.rounds += 1;
        self.machine_runs += plan.assignments.len() as u64;
    }
}

/// The reference executor: every machine on the calling thread, in order.
/// `boundary` runs on the calling thread after every commit (host control).
pub fn run_serial<P, M, R>(
    world: &mut World<P>,
    machines: &mut [M],
    cx: &mut M::Context,
    router: &mut R,
    mut boundary: impl FnMut() -> Result<(), String>,
) -> Result<(Outcome, Stats), Error>
where
    P: Clone + Hash,
    M: Machine<P>,
    R: Router<P>,
{
    check_count(world, machines.len())?;
    let started = Instant::now();
    let mut stats = Stats::default();
    loop {
        let t = Instant::now();
        let plan = match world.plan() {
            Step::Done(o) => {
                stats.coordinator += t.elapsed();
                stats.wall = started.elapsed();
                return Ok((o, stats));
            }
            Step::Round(p) => p,
        };
        stats.coordinator += t.elapsed();
        let t = Instant::now();
        let mut reports = Vec::with_capacity(plan.assignments.len());
        for a in &plan.assignments {
            let r = machines[a.machine]
                .run(cx, a)
                .map_err(|message| Error::Machine {
                    machine: a.machine,
                    round: plan.round,
                    message,
                })?;
            reports.push(r);
        }
        stats.machine += t.elapsed();
        stats.round_wall += t.elapsed();
        let t = Instant::now();
        world.commit(&plan, reports, router)?;
        boundary().map_err(Error::Boundary)?;
        stats.round(&plan);
        stats.coordinator += t.elapsed();
    }
}

/// A worker's context, touched only by the pool thread that created it.
struct Local<C>(Mutex<Option<C>>);

// SAFETY: slot `i` is created, used and dropped only on pool thread `i`
// (`broadcast` for creation and drop; `current_thread_index` during rounds),
// so a context that is not `Send` (a thread's perf counter or PV watchdog)
// never runs on another thread.
unsafe impl<C> Sync for Local<C> {}

/// Run on a work-stealing pool of `workers` threads (rayon). Each thread
/// creates its context with `make_cx` before the first round and drops it
/// after the last. In each round the pool's threads take assignments as they
/// become free, so a machine may run on a different thread every round.
/// Results cannot depend on that: a context must hold no per-machine state
/// that changes what a machine does. A round always runs to completion, so
/// the lowest failing machine is the one reported whatever the timing; a
/// failed round is not committed.
pub fn run_parallel<P, M, R, F>(
    world: &mut World<P>,
    machines: &mut [M],
    workers: usize,
    make_cx: F,
    router: &mut R,
    mut boundary: impl FnMut() -> Result<(), String>,
) -> Result<(Outcome, Stats), Error>
where
    P: Clone + Hash + Send + Sync,
    M: Machine<P> + Send,
    R: Router<P>,
    F: Fn(usize) -> Result<M::Context, String> + Sync,
{
    check_count(world, machines.len())?;
    if workers == 0 {
        return Err(Error::Params("at least one worker is required".into()));
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .thread_name(|i| format!("seedbox-machine-{i}"))
        .build()
        .map_err(|e| Error::Worker {
            worker: 0,
            message: e.to_string(),
        })?;
    let contexts: Vec<Local<M::Context>> = (0..workers).map(|_| Local(Mutex::new(None))).collect();
    let admitted =
        pool.broadcast(
            |b| match catch_unwind(AssertUnwindSafe(|| make_cx(b.index()))) {
                Ok(Ok(cx)) => {
                    *contexts[b.index()]
                        .0
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(cx);
                    Ok(())
                }
                Ok(Err(message)) => Err(Error::Worker {
                    worker: b.index(),
                    message,
                }),
                Err(p) => Err(Error::Worker {
                    worker: b.index(),
                    message: panicked(p),
                }),
            },
        );
    let result = match admitted.into_iter().find(Result::is_err) {
        Some(Err(e)) => Err(e),
        _ => rounds(world, machines, &pool, &contexts, router, &mut boundary),
    };
    // Each context is dropped on the thread that created it.
    pool.broadcast(|b| {
        drop(
            contexts[b.index()]
                .0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take(),
        )
    });
    result
}

fn rounds<P, M, R>(
    world: &mut World<P>,
    machines: &mut [M],
    pool: &rayon::ThreadPool,
    contexts: &[Local<M::Context>],
    router: &mut R,
    boundary: &mut impl FnMut() -> Result<(), String>,
) -> Result<(Outcome, Stats), Error>
where
    P: Clone + Hash + Send + Sync,
    M: Machine<P> + Send,
    R: Router<P>,
{
    use rayon::prelude::*;
    // Uncontended: each assignment is taken by exactly one thread.
    let slots: Vec<Mutex<&mut M>> = machines.iter_mut().map(Mutex::new).collect();
    let last: Vec<AtomicUsize> = slots.iter().map(|_| AtomicUsize::new(usize::MAX)).collect();
    let migrations = AtomicU64::new(0);
    let busy_ns = AtomicU64::new(0);
    let started = Instant::now();
    let mut stats = Stats::default();
    loop {
        let t = Instant::now();
        let plan = match world.plan() {
            Step::Done(o) => {
                stats.coordinator += t.elapsed();
                stats.wall = started.elapsed();
                stats.machine = Duration::from_nanos(busy_ns.load(Ordering::Relaxed));
                stats.migrations = migrations.load(Ordering::Relaxed);
                return Ok((o, stats));
            }
            Step::Round(p) => p,
        };
        stats.coordinator += t.elapsed();
        let t = Instant::now();
        // The parallel collect is the round barrier.
        let results: Vec<Result<Report<P>, String>> = pool.install(|| {
            plan.assignments
                .par_iter()
                .with_max_len(1)
                .map(|a| {
                    let w = rayon::current_thread_index().expect("a pool thread");
                    if last[a.machine].swap(w, Ordering::Relaxed) != w {
                        migrations.fetch_add(1, Ordering::Relaxed);
                    }
                    let started = Instant::now();
                    let mut machine = slots[a.machine].lock().unwrap_or_else(|p| p.into_inner());
                    let mut cx = contexts[w].0.lock().unwrap_or_else(|p| p.into_inner());
                    let cx = cx.as_mut().expect("an admitted context");
                    let m: &mut M = &mut machine;
                    let result = match catch_unwind(AssertUnwindSafe(|| m.run(cx, a))) {
                        Ok(r) => r,
                        Err(p) => Err(panicked(p)),
                    };
                    busy_ns.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                    result
                })
                .collect()
        });
        stats.round_wall += t.elapsed();
        let mut reports = Vec::with_capacity(results.len());
        for (r, a) in results.into_iter().zip(&plan.assignments) {
            // Assignments are in machine order: the first failure is the
            // lowest machine's.
            reports.push(r.map_err(|message| Error::Machine {
                machine: a.machine,
                round: plan.round,
                message,
            })?);
        }
        let t = Instant::now();
        world.commit(&plan, reports, router)?;
        boundary().map_err(Error::Boundary)?;
        stats.round(&plan);
        stats.coordinator += t.elapsed();
    }
}

fn panicked(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "panic".into())
}

fn check_count<P: Clone + Hash>(world: &World<P>, machines: usize) -> Result<(), Error> {
    if world.machines() != machines {
        return Err(Error::Params(format!(
            "world has {} machines, executor {machines}",
            world.machines()
        )));
    }
    Ok(())
}
