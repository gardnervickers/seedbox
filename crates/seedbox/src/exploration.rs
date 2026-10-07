//! Finite sequential seed exploration using one complete prepared closure.
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
    time::Instant,
};

#[derive(Clone, Debug)]
pub struct ExploreOptions {
    pub seed: Option<u64>,
    pub count: u64,
    pub cancellation: CancellationToken,
}
impl Default for ExploreOptions {
    fn default() -> Self {
        Self {
            seed: None,
            count: 1,
            cancellation: CancellationToken::default(),
        }
    }
}
/// Resolve inclusive bounds before any preparation, state creation or lease.
pub fn bounds(simulation: &Simulation, options: &ExploreOptions) -> Result<(u64, u64)> {
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
/// Read a batch checkpoint. Ordinary run manifests remain independently inspectable.
pub fn inspect(directory: &Path) -> Result<Exploration> {
    Ok(serde_json::from_reader(fs::File::open(
        directory.join("exploration.json"),
    )?)?)
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
            version: 1,
            directory: directory.clone(),
            first_seed: first,
            last_seed: last,
            count: options.count,
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
            executor.preflight(None)?;
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
        for index in 0..options.count {
            if options.cancellation.is_cancelled() {
                aggregate = Outcome::Incomplete;
                break;
            }
            if executor.profile() != profile || executor.performance_parameters() != performance {
                batch.error = Some(
                    "execution backend profile or performance changed during exploration".into(),
                );
                aggregate = Outcome::Error;
                break;
            }
            // Validated once, and never increment past the inclusive upper bound.
            let seed = first
                .checked_add(index)
                .ok_or_else(|| Error::Integrity("validated seed range overflow".into()))?;
            let subtree = directory.join("attempts").join(seed.to_string());
            batch.attempts.push(Attempt {
                seed,
                subtree: subtree.clone(),
                directory: None,
                outcome: None,
                error: None,
                elapsed_seconds: None,
            });
            batch.counts = OutcomeCounts::from_attempts(&batch.attempts);
            batch.undispatched_count -= 1;
            batch.total_seconds = began.elapsed().as_secs_f64();
            batch.checkpoint()?;
            let attempt_at = Instant::now();
            let run = Orchestrator::new(self.store.clone(), subtree)
                .with_log_policy(self.log_policy.clone())
                .run_prepared(
                    simulation,
                    RunOptions {
                        seed: Some(seed),
                        cancellation: options.cancellation.clone(),
                    },
                    inputs.clone(),
                    executor,
                );
            let attempt = batch
                .attempts
                .last_mut()
                .ok_or_else(|| Error::Integrity("exploration attempt disappeared".into()))?;
            attempt.elapsed_seconds = Some(attempt_at.elapsed().as_secs_f64());
            let stop = match run {
                Ok(run) => {
                    attempt.directory = Some(run.directory);
                    attempt.outcome = Some(run.record.outcome.clone());
                    attempt.error = run.record.error;
                    if run.record.performance_parameters != performance {
                        batch.error =
                            Some("execution backend performance changed at dispatch".into());
                        aggregate = Outcome::Error;
                        true
                    } else {
                        aggregate = combine(aggregate, &run.record.outcome);
                        match run.record.outcome {
                            Outcome::Pass | Outcome::Fail => false,
                            Outcome::Timeout => !run.record.report.as_ref().is_some_and(|r| {
                                r.reason == StopReason::SimulatedTimeLimit && r.witness.is_some()
                            }),
                            Outcome::Error | Outcome::Incomplete => true,
                        }
                    }
                }
                Err(error) => {
                    attempt.error = Some(error.to_string());
                    batch.error = Some(error.to_string());
                    aggregate = Outcome::Error;
                    true
                }
            };
            batch.counts = OutcomeCounts::from_attempts(&batch.attempts);
            batch.total_seconds = began.elapsed().as_secs_f64();
            batch.checkpoint()?;
            if stop {
                break;
            }
        }
        batch.finished = true;
        batch.outcome = aggregate;
        batch.total_seconds = began.elapsed().as_secs_f64();
        batch.checkpoint()?;
        Ok(batch)
    }
}
fn combine(previous: Outcome, next: &Outcome) -> Outcome {
    fn priority(o: &Outcome) -> u8 {
        match o {
            Outcome::Pass => 0,
            Outcome::Fail => 1,
            Outcome::Timeout => 2,
            Outcome::Incomplete => 3,
            Outcome::Error => 4,
        }
    }
    if priority(next) > priority(&previous) {
        next.clone()
    } else {
        previous
    }
}
