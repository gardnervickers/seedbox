//! Finite bounded seed exploration using one complete prepared closure.
use crate::{
    config::Simulation,
    execution::{
        CancellationToken, ExecutionBackend, PreparationBackend, PreparedInputs, StopReason,
    },
    orchestration::{Orchestrator, Outcome, RunOptions},
    preparation::PreparationContext,
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct ExploreOptions {
    pub seed: Option<u64>,
    pub count: u64,
    /// Maximum active attempts; concurrency is excluded from run identity.
    pub workers: u32,
    pub cancellation: CancellationToken,
}
impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            seed: None,
            count: 1,
            workers: 1,
            cancellation: CancellationToken::default(),
        }
    }
}
/// Resolve inclusive bounds before any preparation, state creation or lease.
pub fn bounds(simulation: &Simulation, options: &ExploreOptions) -> Result<(u64, u64)> {
    if !(1..=32).contains(&options.workers) {
        return Err(Error::Setup("--workers: use 1..32".into()));
    }
    let first = options
        .seed
        .or(simulation.seed)
        .ok_or_else(|| Error::Setup("--seed: no Compose seed; provide --seed FIRST".into()))?;
    let last=options.count.checked_sub(1).and_then(|n| first.checked_add(n)).ok_or_else(||Error::Setup("--seed/--count: range is empty or overflows u64; use a positive --count and reduce the range".into()))?;
    Ok((first, last))
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub index: u64,
    pub finished: bool,
    pub seed: u64,
    /// This subtree owns the ordinary manifest even if the batch checkpoint lags.
    pub subtree: PathBuf,
    pub directory: Option<PathBuf>,
    pub outcome: Option<Outcome>,
    pub error: Option<String>,
    pub elapsed_seconds: Option<f64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeCounts {
    pub pass: u64,
    pub fail: u64,
    pub timeout: u64,
    pub error: u64,
    pub incomplete: u64,
    pub unfinished: u64,
}
impl OutcomeCounts {
    fn from_attempts(attempts: &[Attempt]) -> Self {
        let mut counts = Self::default();
        for attempt in attempts {
            match &attempt.outcome {
                Some(Outcome::Pass) => counts.pass += 1,
                Some(Outcome::Fail) => counts.fail += 1,
                Some(Outcome::Timeout) => counts.timeout += 1,
                Some(Outcome::Error) => counts.error += 1,
                Some(Outcome::Incomplete) => counts.incomplete += 1,
                None => counts.unfinished += 1,
            }
        }
        counts
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessResources {
    pub self_user_seconds: f64,
    pub self_system_seconds: f64,
    pub reaped_children_user_seconds: f64,
    pub reaped_children_system_seconds: f64,
    pub self_rss_high_water_bytes: u64,
    pub reaped_children_rss_high_water_bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exploration {
    pub version: u32,
    pub directory: PathBuf,
    pub first_seed: u64,
    pub last_seed: u64,
    pub count: u64,
    pub workers: u32,
    pub peak_running: u32,
    /// Sum of whole attempt wall durations, including admission/materialization.
    /// This is not a measurement of physical-core lease time or CPU time.
    pub active_attempt_seconds: f64,
    pub configuration: Simulation,
    pub inputs: Option<PreparedInputs>,
    pub performance_parameters: BTreeMap<String, String>,
    pub preparation_directory: PathBuf,
    pub attempts: Vec<Attempt>,
    pub counts: OutcomeCounts,
    pub undispatched_count: u64,
    /// Unfinished checkpoints always carry INCOMPLETE, including process death.
    pub finished: bool,
    pub outcome: Outcome,
    pub error: Option<String>,
    pub preparation_seconds: Option<f64>,
    pub total_seconds: f64,
    pub process_resources: Option<ProcessResources>,
}
impl Exploration {
    pub fn record_resources(&mut self, resources: Option<ProcessResources>) -> Result<()> {
        self.process_resources = resources;
        self.checkpoint()
    }
    fn checkpoint(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer_pretty(&mut file, self)?;
        file.write_all(b"\n")?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join("exploration.json"))
            .map_err(|e| e.error)?;
        Ok(())
    }
}
/// Read a v2 batch checkpoint. Older bytes and ordinary manifests are preserved;
/// use the prior reader for a prior checkpoint format, rather than migrating it.
pub fn inspect(directory: &Path) -> Result<Exploration> {
    let value: serde_json::Value =
        serde_json::from_reader(fs::File::open(directory.join("exploration.json"))?)?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(2) {
        return Err(Error::Integrity("unsupported exploration checkpoint version; expected 2; use the retained prior reader for older checkpoints".into()));
    }
    Ok(serde_json::from_value(value)?)
}

impl Orchestrator {
    pub fn explore(
        &self,
        simulation: &Simulation,
        options: ExploreOptions,
        preparer: &dyn PreparationBackend,
        executor: &dyn ExecutionBackend,
    ) -> Result<Exploration> {
        let (first, last) = bounds(simulation, &options)?;
        let began = Instant::now();
        fs::create_dir_all(&self.runs)?;
        let directory = tempfile::Builder::new()
            .prefix("explore-")
            .tempdir_in(&self.runs)?
            .keep();
        let mut batch = Exploration {
            version: 2,
            directory: directory.clone(),
            first_seed: first,
            last_seed: last,
            count: options.count,
            workers: options.workers,
            peak_running: 0,
            active_attempt_seconds: 0.0,
            configuration: simulation.clone(),
            inputs: None,
            performance_parameters: executor.performance_parameters(),
            preparation_directory: directory.join("preparation"),
            attempts: Vec::new(),
            counts: OutcomeCounts::default(),
            undispatched_count: options.count,
            finished: false,
            outcome: Outcome::Incomplete,
            error: None,
            preparation_seconds: None,
            total_seconds: 0.0,
            process_resources: None,
        };
        batch.checkpoint()?;
        let prepared_at = Instant::now();
        let prepared = (|| {
            let context = PreparationContext::new(
                &batch.preparation_directory,
                options.cancellation.clone(),
            )?;
            context.check()?;
            preparer.validate_runtime_inputs()?;
            executor.preflight_workers(options.workers)?;
            preparer.prepare_cancellable(simulation, &self.store, &context)
        })();
        batch.preparation_seconds = Some(prepared_at.elapsed().as_secs_f64());
        let inputs = match prepared {
            Ok(inputs) => inputs,
            Err(error) => {
                batch.outcome = if matches!(error, Error::PreparationCancelled) {
                    Outcome::Incomplete
                } else {
                    Outcome::Error
                };
                batch.error = Some(error.to_string());
                batch.finished = true;
                batch.total_seconds = began.elapsed().as_secs_f64();
                batch.checkpoint()?;
                return Ok(batch);
            }
        };
        let profile = executor.profile();
        let performance = batch.performance_parameters.clone();
        batch.inputs = Some(inputs.clone());
        batch.checkpoint()?;
        let mut aggregate = Outcome::Pass;
        let cancellation = options.cancellation.owned();
        // Only our attempt token is cancelled on infrastructure failure. The
        // caller's cancellation is observed directly by the owned token.
        struct CancelOnExit(CancellationToken);
        impl Drop for CancelOnExit {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let workers = u64::from(options.workers).min(options.count) as usize;
        // Attempts hard-link one verified read-only copy of each input.
        let shared_inputs = directory.join("inputs");
        std::thread::scope(|scope| -> Result<()> {
            let _cleanup = CancelOnExit(cancellation.clone());
            let (completed, results) = mpsc::channel();
            let mut dispatch = Vec::new();
            for worker in 0..workers {
                let (sender, jobs) = mpsc::channel::<(usize, u64, PathBuf)>();
                dispatch.push(sender);
                let completed = completed.clone();
                let cancellation = cancellation.clone();
                let inputs = &inputs;
                let performance = &performance;
                let shared_inputs = &shared_inputs;
                std::thread::Builder::new()
                    .spawn_scoped(scope, move || {
                        while let Ok((index, seed, subtree)) = jobs.recv() {
                            let at = Instant::now();
                            let result = if cancellation.is_cancelled() {
                                Ok(None)
                            } else {
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    Orchestrator {
                                        shared_inputs: Some(shared_inputs.clone()),
                                        ..Orchestrator::new(self.store.clone(), subtree)
                                    }
                                    .with_log_policy(self.log_policy.clone())
                                    .run_prepared(
                                        simulation,
                                        RunOptions {
                                            seed: Some(seed),
                                            cancellation: cancellation.clone(),
                                        },
                                        inputs.clone(),
                                        executor,
                                    )
                                }))
                                .unwrap_or_else(|_| {
                                    Err(Error::Execution(
                                        "exploration worker panicked; cleanup cannot be verified"
                                            .into(),
                                    ))
                                })
                                .map(Some)
                            };
                            if must_stop(&result, performance) {
                                cancellation.cancel();
                            }
                            if completed
                                .send((worker, index, at.elapsed().as_secs_f64(), result))
                                .is_err()
                            {
                                break;
                            }
                        }
                    })
                    .map_err(|error| {
                        Error::Execution(format!("starting exploration worker: {error}"))
                    })?;
            }
            drop(completed);
            let mut idle: Vec<usize> = (0..workers).rev().collect();
            let mut running = 0_u32;
            let mut next = 0_u64;
            let mut stopped = false;
            loop {
                if options.cancellation.is_cancelled() {
                    cancellation.cancel();
                    aggregate = combine(&aggregate, &Outcome::Incomplete);
                }
                if cancellation.is_cancelled() {
                    stopped = true;
                }
                if !stopped && next < options.count && !idle.is_empty() {
                    if executor.profile() != profile
                        || executor.performance_parameters() != performance
                    {
                        batch.error = Some(
                            "execution backend profile or performance changed during exploration"
                                .into(),
                        );
                        aggregate = Outcome::Error;
                        cancellation.cancel();
                        stopped = true;
                        continue;
                    }
                    if cancellation.is_cancelled() {
                        continue;
                    }
                    let worker = idle
                        .pop()
                        .ok_or_else(|| Error::Integrity("idle worker disappeared".into()))?;
                    let seed = first
                        .checked_add(next)
                        .ok_or_else(|| Error::Integrity("validated seed range overflow".into()))?;
                    let subtree = directory.join("attempts").join(seed.to_string());
                    let index = batch.attempts.len();
                    batch.attempts.push(Attempt {
                        index: next,
                        finished: false,
                        seed,
                        subtree: subtree.clone(),
                        directory: None,
                        outcome: None,
                        error: None,
                        elapsed_seconds: None,
                    });
                    batch.counts = OutcomeCounts::from_attempts(&batch.attempts);
                    batch.undispatched_count -= 1;
                    running += 1;
                    batch.peak_running = batch.peak_running.max(running);
                    batch.total_seconds = began.elapsed().as_secs_f64();
                    // The attempt's ownership/index is durable before dispatch.
                    batch.checkpoint()?;
                    dispatch[worker].send((index, seed, subtree)).map_err(|_| {
                        Error::Execution("exploration worker disconnected before dispatch".into())
                    })?;
                    next += 1;
                    continue;
                }
                if running == 0 {
                    break;
                }
                match results.recv_timeout(Duration::from_millis(10)) {
                    Ok((worker, index, elapsed, result)) => {
                        running -= 1;
                        idle.push(worker);
                        let stop = must_stop(&result, &performance);
                        let attempt = batch.attempts.get_mut(index).ok_or_else(|| {
                            Error::Integrity("exploration attempt disappeared".into())
                        })?;
                        attempt.finished = true;
                        attempt.elapsed_seconds = Some(elapsed);
                        batch.active_attempt_seconds += elapsed;
                        match result {
                            Ok(None) => {
                                attempt.outcome = Some(Outcome::Incomplete);
                                attempt.error =
                                    Some("cancelled before entering attempt orchestration".into());
                                aggregate = combine(&aggregate, &Outcome::Incomplete);
                            }
                            Ok(Some(run)) => {
                                attempt.directory = Some(run.directory);
                                attempt.outcome = Some(run.record.outcome.clone());
                                attempt.error = run.record.error;
                                if run.record.performance_parameters != performance {
                                    batch.error = Some(
                                        "execution backend performance changed at dispatch".into(),
                                    );
                                    aggregate = Outcome::Error;
                                } else {
                                    aggregate = combine(&aggregate, &run.record.outcome);
                                }
                            }
                            Err(error) => {
                                attempt.outcome = Some(Outcome::Error);
                                attempt.error = Some(error.to_string());
                                batch.error = Some(error.to_string());
                                aggregate = Outcome::Error;
                            }
                        }
                        if stop {
                            stopped = true;
                            cancellation.cancel();
                        }
                        batch.counts = OutcomeCounts::from_attempts(&batch.attempts);
                        batch.total_seconds = began.elapsed().as_secs_f64();
                        batch.checkpoint()?;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return Err(Error::Execution(
                            "exploration workers disconnected with active attempts".into(),
                        ))
                    }
                }
            }
            // Closing only our queues joins every scoped worker. Backends must
            // return only after their owned child/lease cleanup; no prompt
            // cleanup claim is possible when a backend blocks indefinitely.
            drop(dispatch);
            Ok(())
        })?;
        batch.finished = true;
        batch.outcome = aggregate;
        batch.total_seconds = began.elapsed().as_secs_f64();
        batch.checkpoint()?;
        Ok(batch)
    }
}
fn combine(previous: &Outcome, next: &Outcome) -> Outcome {
    fn priority(o: &Outcome) -> u8 {
        match o {
            Outcome::Pass => 0,
            Outcome::Fail => 1,
            Outcome::Timeout => 2,
            Outcome::Incomplete => 3,
            Outcome::Error => 4,
        }
    }
    if priority(next) > priority(previous) {
        next.clone()
    } else {
        previous.clone()
    }
}

fn must_stop(
    run: &Result<Option<crate::orchestration::Run>>,
    performance: &BTreeMap<String, String>,
) -> bool {
    let Ok(Some(run)) = run else {
        return true;
    };
    if &run.record.performance_parameters != performance {
        return true;
    }
    match run.record.outcome {
        Outcome::Pass | Outcome::Fail => false,
        Outcome::Timeout => !run
            .record
            .report
            .as_ref()
            .is_some_and(|r| r.reason == StopReason::SimulatedTimeLimit && r.witness.is_some()),
        Outcome::Error | Outcome::Incomplete => true,
    }
}
