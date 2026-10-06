//! Bounded campaign/acceptance dispatch over the common orchestrator. The
//! executor alone owns host leases, processes and MSR restoration.
use crate::{
    execution::*,
    fixture::{Fixture, Frozen},
    orchestration::{Orchestrator, Outcome, ReplayOptions, Run, RunOptions},
    store::ContentStore,
    vmm::VmmExecutor,
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    pub scenario: String,
    pub seed: u64,
    pub witness: WitnessMode,
    pub margin: String,
    pub undo: u64,
    pub engine: Option<PathBuf>,
    pub replay: Option<PathBuf>,
}
impl Job {
    pub fn new(scenario: impl Into<String>, seed: u64) -> Self {
        Self {
            scenario: scenario.into(),
            seed,
            witness: WitnessMode::Exits,
            margin: "adaptive".into(),
            undo: 256,
            engine: None,
            replay: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct JobResult {
    pub job: Job,
    pub run: Option<Run>,
    pub attempts: Vec<PathBuf>,
    pub overshoot_retry: bool,
    pub error: Option<String>,
}
impl JobResult {
    pub fn completed(&self) -> bool {
        self.run
            .as_ref()
            .is_some_and(|r| matches!(r.record.outcome, Outcome::Pass | Outcome::Fail))
    }
}

pub trait JobRunner: Sync {
    fn run(&self, job: &Job, cpu: u32, cancellation: &CancellationToken) -> Result<Run>;
}

/// Prepare every fixture once, before dispatch. All subsequent runs/replays use
/// these retained bytes even if source files or a mutable binary are replaced.
pub struct FixtureRunner {
    pub store: ContentStore,
    pub runs: PathBuf,
    fixtures: BTreeMap<String, Frozen>,
}
impl FixtureRunner {
    pub fn prepare(
        fixtures: BTreeMap<String, Fixture>,
        engine: &std::path::Path,
        store: ContentStore,
        runs: PathBuf,
    ) -> Result<Self> {
        let engine_digest = store.put_file(engine)?;
        let mut frozen = BTreeMap::new();
        for (name, fixture) in fixtures {
            let executor = VmmExecutor::from_profile(
                ExecutionProfile {
                    backend: "seedbox-kvm".into(),
                    engine: engine_digest.clone(),
                    witness_mode: WitnessMode::Exits,
                    semantic_parameters: BTreeMap::from([
                        ("memory_mb".into(), fixture.memory_mb.to_string()),
                        ("guest_abi".into(), crate::fixture::ABI.into()),
                    ]),
                },
                0,
            )?;
            frozen.insert(name, fixture.prepare(executor.profile(), &store)?);
        }
        Ok(Self {
            store,
            runs,
            fixtures: frozen,
        })
    }
}
impl JobRunner for FixtureRunner {
    fn run(&self, job: &Job, cpu: u32, cancellation: &CancellationToken) -> Result<Run> {
        let mut frozen = self
            .fixtures
            .get(&job.scenario)
            .ok_or_else(|| Error::Execution("unknown scenario".into()))?
            .clone();
        frozen.inputs.profile.witness_mode = job.witness.clone();
        if let Some(engine) = &job.engine {
            frozen.inputs.profile.engine = self.store.put_file(engine)?;
        }
        let executor = VmmExecutor::from_profile(frozen.inputs.profile.clone(), cpu)?
            .with_margin(&job.margin)?
            .with_undo(job.undo)?;
        let orchestrator = Orchestrator::new(self.store.clone(), &self.runs);
        match &job.replay {
            Some(path) => {
                let baseline = crate::orchestration::inspect(path)?;
                if baseline.identity.seed != job.seed {
                    return Err(Error::Integrity("replay seed changed".into()));
                }
                orchestrator.replay_with_options(
                    path,
                    &executor,
                    ReplayOptions {
                        cancellation: cancellation.clone(),
                        allow_engine_change: job.engine.is_some(),
                    },
                )
            }
            None => orchestrator.run(
                &frozen.simulation,
                RunOptions {
                    seed: Some(job.seed),
                    cancellation: cancellation.clone(),
                },
                &frozen,
                &executor,
            ),
        }
    }
}

pub fn cpu_list(value: &str) -> Result<Vec<u32>> {
    let mut result = BTreeSet::new();
    for part in value.trim().split(',') {
        let (low, high) = part.split_once('-').unwrap_or((part, part));
        let bad = || Error::Execution("expected CPU list such as 2,4-7".into());
        let low: u32 = low.parse().map_err(|_| bad())?;
        let high: u32 = high.parse().map_err(|_| bad())?;
        if high < low || high >= 1024 {
            return Err(bad());
        }
        for n in low..=high {
            if !result.insert(n) {
                return Err(bad());
            }
        }
    }
    if result.is_empty() {
        return Err(Error::Execution("no CPUs selected".into()));
    }
    Ok(result.into_iter().collect())
}

pub fn validate_cpus(cpus: &[u32], siblings: impl Fn(u32) -> Result<BTreeSet<u32>>) -> Result<()> {
    let mut reserved = BTreeSet::new();
    if cpus.is_empty() {
        return Err(Error::Execution("no CPUs selected".into()));
    }
    for &cpu in cpus {
        let group = siblings(cpu)?;
        if !group.contains(&cpu) || !group.is_disjoint(&reserved) {
            return Err(Error::Execution(
                "CPU selections overlap SMT siblings".into(),
            ));
        }
        reserved.extend(group);
    }
    Ok(())
}

/// One worker per selected core. Results retain input ordering; every retry is
/// a fresh retained attempt. Only typed lease contention and PMU overshoot retry.
pub fn dispatch(
    jobs: &[Job],
    cpus: &[u32],
    runner: &dyn JobRunner,
    cancellation: &CancellationToken,
    lease_wait: Duration,
) -> Vec<JobResult> {
    if cpus.is_empty() || cpus.iter().collect::<BTreeSet<_>>().len() != cpus.len() {
        return jobs
            .iter()
            .map(|j| JobResult {
                job: j.clone(),
                run: None,
                attempts: vec![],
                overshoot_retry: false,
                error: Some("CPU selection is empty or duplicated".into()),
            })
            .collect();
    }
    let next = AtomicUsize::new(0);
    let results = Mutex::new(vec![None; jobs.len()]);
    std::thread::scope(|scope| {
        for &cpu in cpus {
            let next = &next;
            let results = &results;
            scope.spawn(move || loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(job) = jobs.get(i) else {
                    break;
                };
                let mut result = JobResult {
                    job: job.clone(),
                    run: None,
                    attempts: vec![],
                    overshoot_retry: false,
                    error: None,
                };
                let mut attempt = job.clone();
                let started = Instant::now();
                loop {
                    if cancellation.is_cancelled() {
                        result.error = Some("cancelled before dispatch".into());
                        break;
                    }
                    let execution = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        runner.run(&attempt, cpu, cancellation)
                    }))
                    .unwrap_or_else(|_| {
                        Err(Error::Execution(
                            "job runner panicked; retained attempt may be incomplete".into(),
                        ))
                    });
                    match execution {
                        Err(e) => {
                            result.error = Some(e.to_string());
                            break;
                        }
                        Ok(run) => {
                            let kind = run.record.failure_kind.clone();
                            result.attempts.push(run.directory.clone());
                            result.run = Some(run);
                            if kind == Some(crate::orchestration::FailureKind::Overshoot)
                                && !result.overshoot_retry
                            {
                                attempt.margin = "98304".into();
                                result.overshoot_retry = true;
                                continue;
                            }
                            if kind == Some(crate::orchestration::FailureKind::LeaseBusy)
                                && started.elapsed() < lease_wait
                            {
                                let end = Instant::now()
                                    + Duration::from_secs(1)
                                        .min(lease_wait.saturating_sub(started.elapsed()));
                                while Instant::now() < end && !cancellation.is_cancelled() {
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                                continue;
                            }
                            break;
                        }
                    }
                }
                results.lock().unwrap()[i] = Some(result);
            });
        }
    });
    results
        .into_inner()
        .unwrap()
        .into_iter()
        .map(Option::unwrap)
        .collect()
}

pub fn validate_host_cpus(cpus: &[u32]) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Execution("hardware jobs require Linux".into()));
    }
    validate_cpus(cpus, |cpu| {
        Ok(cpu_list(&std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list"
        ))?)?
        .into_iter()
        .collect())
    })
}

pub struct Harness<'a> {
    pub runner: &'a FixtureRunner,
    pub cpus: &'a [u32],
    pub cancellation: &'a CancellationToken,
    pub lease_wait: Duration,
}
impl crate::acceptance::Launcher for Harness<'_> {
    fn launch(&mut self, jobs: &[Job]) -> Vec<crate::acceptance::Observation> {
        dispatch(
            jobs,
            self.cpus,
            self.runner,
            self.cancellation,
            self.lease_wait,
        )
        .into_iter()
        .map(|r| crate::acceptance::Observation::from_result(r, &self.runner.store))
        .collect()
    }
}
#[derive(Clone, Debug)]
pub struct CampaignOptions {
    pub first_seed: u64,
    pub max_seeds: u64,
    pub time_budget: Duration,
    pub continue_after_failure: bool,
    pub lease_wait: Duration,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CampaignReport {
    pub attempted: u64,
    pub failures: u64,
    pub unresolved: u64,
    pub stopped: String,
}
pub fn campaign(
    base: &Job,
    options: &CampaignOptions,
    cpus: &[u32],
    runner: &dyn JobRunner,
    cancellation: &CancellationToken,
    mut retain: impl FnMut(&[JobResult]) -> Result<()>,
) -> Result<CampaignReport> {
    if options.max_seeds == 0
        || cpus.is_empty()
        || options.time_budget.is_zero()
        || options
            .first_seed
            .checked_add(options.max_seeds - 1)
            .is_none()
    {
        return Err(Error::Execution(
            "invalid campaign seed range, CPUs, or time budget".into(),
        ));
    }
    let start = Instant::now();
    let mut report = CampaignReport {
        attempted: 0,
        failures: 0,
        unresolved: 0,
        stopped: "seed limit".into(),
    };
    while report.attempted < options.max_seeds {
        if cancellation.is_cancelled() {
            report.stopped = "cancelled".into();
            break;
        }
        if start.elapsed() >= options.time_budget {
            report.stopped = "time budget".into();
            break;
        }
        let count = (options.max_seeds - report.attempted).min(cpus.len() as u64);
        let jobs: Vec<_> = (0..count)
            .map(|i| {
                let mut job = base.clone();
                job.seed = options.first_seed + report.attempted + i;
                job
            })
            .collect();
        let results = dispatch(&jobs, cpus, runner, cancellation, options.lease_wait);
        retain(&results)?;
        report.attempted += count;
        report.failures += results
            .iter()
            .filter(|r| {
                r.run
                    .as_ref()
                    .is_some_and(|r| r.record.outcome == Outcome::Fail)
            })
            .count() as u64;
        report.unresolved += results.iter().filter(|r| !r.completed()).count() as u64;
        if report.unresolved > 0 {
            report.stopped = "unresolved attempt".into();
            break;
        }
        if report.failures > 0 && !options.continue_after_failure {
            report.stopped = "application failure".into();
            break;
        }
    }
    Ok(report)
}
