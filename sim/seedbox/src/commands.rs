//! Hardware CLI boundary. Suite/campaign policy and execution live in the
//! library modules; this module only resolves paths and writes reports.
use clap::Args;
use seedbox::{
    acceptance::{self, Cases, SuiteReport},
    execution::{CancellationToken, WitnessMode},
    fixture::Fixture,
    runner::{self, CampaignOptions, FixtureRunner, Harness, Job, JobRunner},
    store::ContentStore,
    Error, Result,
};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Args)]
pub struct HostArgs {
    #[arg(long)]
    pub vmm: PathBuf,
    #[arg(long)]
    pub artifacts: Option<PathBuf>,
    #[arg(long, default_value = "2-7")]
    pub cpus: String,
    #[arg(long, default_value_t = 1800)]
    pub timeout: u64,
    #[arg(long, default_value_t = 0)]
    pub lease_wait: u64,
    #[arg(long, default_value = ".seedbox")]
    pub state: PathBuf,
}
impl HostArgs {
    fn cpus(&self) -> Result<Vec<u32>> {
        let cpus = runner::cpu_list(&self.cpus)?;
        runner::validate_host_cpus(&cpus)?;
        Ok(cpus)
    }
    fn prepare(&self, sources: BTreeMap<String, PathBuf>) -> Result<FixtureRunner> {
        let wall = self
            .timeout
            .checked_mul(1_000_000_000)
            .filter(|n| *n > 0)
            .ok_or_else(|| Error::Execution("invalid timeout".into()))?;
        let fixtures = sources
            .into_iter()
            .map(|(name, path)| Ok((name, Fixture::load(&path, self.artifacts.as_deref(), wall)?)))
            .collect::<Result<_>>()?;
        std::fs::create_dir_all(&self.state)?;
        let state = self.state.canonicalize()?;
        FixtureRunner::prepare(
            fixtures,
            &self.vmm,
            ContentStore::new(state.join("objects")),
            state.join("runs"),
        )
    }
}
#[derive(Args)]
pub struct AcceptanceArgs {
    #[arg(value_parser=["smoke","full"])]
    pub tier: String,
    #[arg(long, default_value = "sim")]
    pub sim: PathBuf,
    #[arg(long)]
    pub cases: Option<PathBuf>,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long)]
    pub performance_only: Option<PathBuf>,
    #[command(flatten)]
    pub host: HostArgs,
}
fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}
pub fn acceptance(args: AcceptanceArgs, cancellation: &CancellationToken) -> Result<u8> {
    let cpus = args.host.cpus()?;
    let cases = Cases::load(
        &args
            .cases
            .unwrap_or_else(|| args.sim.join("acceptance/cases.json")),
    )?;
    let sources = cases
        .scenarios
        .iter()
        .map(|(name, path)| (name.clone(), args.sim.join(path)))
        .collect();
    let runner = args.host.prepare(sources)?;
    std::fs::create_dir(&args.output)?;
    let mut launcher = Harness {
        runner: &runner,
        cpus: &cpus,
        cancellation,
        lease_wait: Duration::from_secs(args.host.lease_wait),
    };
    let checks = acceptance::suite(
        &cases,
        args.tier == "full",
        args.performance_only,
        &mut launcher,
    )?;
    let report = SuiteReport::new(checks);
    for check in &report.checks {
        println!("{:?}: {}: {}", check.verdict, check.name, check.detail);
    }
    write_json(&args.output.join("report.json"), &report)?;
    println!("report: {}", args.output.join("report.json").display());
    Ok(report.exit_code())
}
#[derive(Args)]
pub struct CampaignArgs {
    pub world: PathBuf,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, default_value_t = 1)]
    pub first_seed: u64,
    #[arg(long, default_value_t = 1000000)]
    pub max_seeds: u64,
    #[arg(long, default_value_t = 3600)]
    pub seconds: u64,
    #[arg(long)]
    pub keep_going: bool,
    #[arg(long, default_value = "adaptive")]
    pub margin: String,
    #[arg(long, default_value_t = 256)]
    pub undo: u64,
    #[arg(long)]
    pub full_state: bool,
    #[command(flatten)]
    pub host: HostArgs,
}
pub fn campaign(args: CampaignArgs, cancellation: &CancellationToken) -> Result<u8> {
    let cpus = args.host.cpus()?;
    let runner = args
        .host
        .prepare(BTreeMap::from([("campaign".into(), args.world)]))?;
    std::fs::create_dir(&args.output)?;
    let mut index = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(args.output.join("attempts.jsonl"))?;
    let mut base = Job::new("campaign", args.first_seed);
    base.margin = args.margin;
    base.undo = args.undo;
    if args.full_state {
        base.witness = WitnessMode::FullState;
    }
    let options = CampaignOptions {
        first_seed: args.first_seed,
        max_seeds: args.max_seeds,
        time_budget: Duration::from_secs(args.seconds),
        continue_after_failure: args.keep_going,
        lease_wait: Duration::from_secs(args.host.lease_wait),
    };
    let report = runner::campaign(&base, &options, &cpus, &runner, cancellation, |results| {
        for result in results {
            let observation = acceptance::Observation::from_result(result.clone(), &runner.store);
            println!(
                "seed={} {:?}: {}",
                result.job.seed,
                observation.outcome,
                observation
                    .evidence
                    .as_ref()
                    .map_or(String::new(), |p| p.display().to_string())
            );
            serde_json::to_writer(&mut index, &observation)?;
            index.write_all(b"\n")?;
        }
        index.sync_all()?;
        Ok(())
    })?;
    write_json(&args.output.join("report.json"), &report)?;
    Ok(if report.unresolved > 0 || report.stopped == "cancelled" {
        3
    } else {
        0
    })
}
#[derive(Args)]
pub struct FixtureArgs {
    pub world: PathBuf,
    #[arg(long)]
    pub seed: u64,
    #[arg(long, default_value = "adaptive")]
    pub margin: String,
    #[arg(long, default_value_t = 256)]
    pub undo: u64,
    #[arg(long)]
    pub full_state: bool,
    #[command(flatten)]
    pub host: HostArgs,
}
pub fn fixture(
    args: FixtureArgs,
    cancellation: &CancellationToken,
) -> Result<seedbox::orchestration::Run> {
    let cpus = args.host.cpus()?;
    if cpus.len() != 1 {
        return Err(Error::Execution("fixture requires exactly one CPU".into()));
    }
    let runner = args
        .host
        .prepare(BTreeMap::from([("fixture".into(), args.world)]))?;
    let mut job = Job::new("fixture", args.seed);
    job.margin = args.margin;
    job.undo = args.undo;
    if args.full_state {
        job.witness = WitnessMode::FullState;
    }
    runner.run(&job, cpus[0], cancellation)
}

#[derive(Args)]
pub struct BatchArgs {
    pub world: PathBuf,
    #[arg(long, required = true, value_delimiter = ',')]
    pub seeds: Vec<u64>,
    #[arg(long)]
    pub output: PathBuf,
    #[arg(long, default_value = "adaptive")]
    pub margin: String,
    #[arg(long, default_value_t = 256)]
    pub undo: u64,
    #[arg(long)]
    pub full_state: bool,
    #[command(flatten)]
    pub host: HostArgs,
}
pub fn batch(args: BatchArgs, cancellation: &CancellationToken) -> Result<u8> {
    let cpus = args.host.cpus()?;
    let runner = args
        .host
        .prepare(BTreeMap::from([("batch".into(), args.world)]))?;
    std::fs::create_dir(&args.output)?;
    let jobs: Vec<_> = args
        .seeds
        .into_iter()
        .map(|seed| {
            let mut job = Job::new("batch", seed);
            job.margin = args.margin.clone();
            job.undo = args.undo;
            if args.full_state {
                job.witness = WitnessMode::FullState;
            }
            job
        })
        .collect();
    let observations: Vec<_> = runner::dispatch(
        &jobs,
        &cpus,
        &runner,
        cancellation,
        Duration::from_secs(args.host.lease_wait),
    )
    .into_iter()
    .map(|r| acceptance::Observation::from_result(r, &runner.store))
    .collect();
    write_json(&args.output.join("report.json"), &observations)?;
    for run in &observations {
        println!(
            "seed={} {:?}: {}",
            run.job.seed,
            run.outcome,
            run.evidence
                .as_ref()
                .map_or(String::new(), |p| p.display().to_string())
        );
    }
    Ok(if observations.iter().all(|r| r.completed()) {
        0
    } else {
        3
    })
}
