//! The `seedbox` command line. Style rules for changes: `AGENTS.md`, "CLI".

mod defaults;
mod diagnostics;
mod signals;

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox::{
    bundle, config,
    counter::CounterProfile,
    diff,
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, PreparationBackend, Timebase,
        WitnessMode,
    },
    exploration::{self, ExploreOptions},
    logs,
    oci::OciTools,
    orchestration::{self, Orchestrator, Outcome, ReplayOptions, ReplayStatus, RunOptions},
    preparation::OciPreparer,
    store::ContentStore,
    templates::Template,
    vmm::{recorded_counter, recorded_timebase, recorded_torn_writes, VmmExecutor},
    Error, Result,
};
use signals::Signals;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

const EXIT_CODES: &str = "\
Exit status:
  0    PASS, or a replay/diff MATCH
  1    FAIL, replay MISMATCH, or diff DIFFERENT
  2    ERROR, a replay that is NOT_COMPARABLE, or a usage or setup error
  3    INCOMPLETE (wall-time limit or cancelled)
       A cancelled or incomplete replay reports 2 (NOT_COMPARABLE)
  124  TIMEOUT (simulated-time limit)";

const HOST_CONTROL_HELP: &str = "\
Host execution control:
  seedbox pause RUN     Wait for a host slice-boundary pause
  seedbox resume RUN    Resume the same live world
Pause preserves guest time and witnesses. Wall-time limits and cancellation remain active.
The executing attempt has an output/host-control locator; replay uses its new attempt.";
const VMM_FAILURE_HELP: &str = "Text VMM failures also show up to 5 non-empty lines from the final 8 KiB of output/vmm.stderr on stderr; JSON output is unchanged.";

#[derive(Parser)]
#[command(
    version,
    about = "Deterministic simulation of Compose applications on real Linux machines",
    long_about = "Deterministic simulation of Compose applications on real Linux machines.\n\n\
                  A seed plus the retained inputs regenerate the same execution. \
                  `run` and `replay` need a qualified Linux KVM host and root."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a self-contained example project.
    Init {
        /// Directory to create.
        directory: PathBuf,
        /// Example application to start from.
        #[arg(long, value_enum, default_value_t = TemplateArg::Etcd)]
        template: TemplateArg,
    },
    /// Check Compose services and directed network policies, without images or guests.
    Validate {
        #[command(flatten)]
        compose: ComposeArg,
        /// Print the resolved configuration as JSON (includes environment values).
        #[arg(long)]
        json: bool,
    },
    /// Check execution-profile compatibility without images or guests (Linux KVM host, root).
    #[command(
        after_help = "Success means profile compatibility, not cross-host replay qualification. Doctor never builds missing runtime inputs."
    )]
    Doctor {
        #[command(flatten)]
        cpu: CpuArg,
        /// Print the admitted observations as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        inputs: KvmInputs,
    },
    /// Resolve images and build guest artifacts, without running.
    Prepare {
        #[command(flatten)]
        compose: ComposeArg,
        /// Write the prepared-inputs JSON here; must not exist.
        #[arg(long, short)]
        output: PathBuf,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[command(flatten)]
        build: BuildOptions,
    },
    /// Prepare images and run a simulation (Linux KVM host, root).
    #[command(after_help = format!("{EXIT_CODES}\n\n{VMM_FAILURE_HELP}"))]
    Run {
        #[command(flatten)]
        compose: ComposeArg,
        /// Simulation seed [default: x-seedbox.seed from the Compose file].
        #[arg(long)]
        seed: Option<u64>,
        /// Capture the world before this simulated nanosecond and continue a branch.
        #[arg(long, requires = "branch_seed", value_parser = clap::value_parser!(u64))]
        checkpoint_at_ns: Option<u64>,
        /// Branch seed for simulator and guest kernel randomness; the parent seed continues unchanged.
        #[arg(long, requires = "checkpoint_at_ns", value_parser = clap::value_parser!(u64))]
        branch_seed: Option<u64>,
        #[command(flatten)]
        cpu: CpuArg,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[command(flatten)]
        build: BuildOptions,
        #[command(flatten)]
        performance: Performance,
    },
    /// Explore a finite seed sequence with one frozen preparation (Linux KVM host, root).
    #[command(after_help = format!("{EXIT_CODES}\n\n{VMM_FAILURE_HELP}"))]
    Explore {
        #[command(flatten)]
        compose: ComposeArg,
        /// First simulation seed [default: x-seedbox.seed from the Compose file].
        #[arg(long)]
        seed: Option<u64>,
        /// Number of consecutive seeds to execute.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
        count: u64,
        /// Maximum active attempts; omit --cpu for parallel execution.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=32))]
        workers: u32,
        /// Print exactly one exploration report as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        cpu: CpuArg,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[command(flatten)]
        build: BuildOptions,
        #[command(flatten)]
        performance: Performance,
    },
    /// Re-execute a recorded run from its retained inputs and compare (Linux KVM host, root).
    #[command(after_help = format!("{EXIT_CODES}\n\n{VMM_FAILURE_HELP}"))]
    Replay {
        /// Run directory, as printed by `run`.
        run: PathBuf,
        #[command(flatten)]
        cpu: CpuArg,
        #[command(flatten)]
        state: StateArg,
        /// Replay with this VMM instead of the recorded one.
        #[arg(long, requires = "allow_engine_change")]
        vmm: Option<PathBuf>,
        /// Allow --vmm to differ from the recorded VMM.
        #[arg(long)]
        allow_engine_change: bool,
        // Last: its help heading applies to every flag declared after it.
        #[command(flatten)]
        performance: Performance,
    },
    /// Pause host execution at a slice boundary (Linux KVM host, same user or root).
    #[command(after_help = HOST_CONTROL_HELP)]
    Pause {
        /// Directory of the executing run or replay attempt.
        run: PathBuf,
    },
    /// Resume host execution from a paused slice boundary (Linux KVM host, same user or root).
    #[command(after_help = HOST_CONTROL_HELP)]
    Resume {
        /// Directory of the executing run or replay attempt.
        run: PathBuf,
    },
    /// Export a recorded run and its retained inputs and logs.
    Export {
        /// Run directory, as printed by run or replay.
        run: PathBuf,
        /// Bundle file to create [default: RUN-directory-name.seedbox.tar].
        #[arg(long, short)]
        output: Option<PathBuf>,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        transfer: Transfer,
    },
    /// Import a bundle into fresh run storage, without executing it.
    Import {
        /// Bundle file to import.
        bundle: PathBuf,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        transfer: Transfer,
    },
    /// Compare retained run evidence, including terminal state parts, without executing guests.
    #[command(after_help = EXIT_CODES)]
    Diff {
        /// First retained run directory.
        run_a: PathBuf,
        /// Second retained run directory.
        run_b: PathBuf,
        /// Print exactly one comparison report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show committed-time logs as raw bytes without executing guests.
    Logs {
        /// Retained run directory.
        run: PathBuf,
        /// Select one machine from the retained run.
        #[arg(long)]
        machine: Option<String>,
        /// Select the console or management stream [default: both].
        #[arg(long, value_enum)]
        stream: Option<LogStream>,
        /// Include chunks whose first byte commits at or after this simulated nanosecond.
        #[arg(long)]
        since_ns: Option<u64>,
        /// Include chunks whose first byte commits before this simulated nanosecond.
        #[arg(long)]
        until_ns: Option<u64>,
        /// Print one JSON document with lossless hexadecimal payloads.
        #[arg(long)]
        json: bool,
    },
    /// Show a recorded run, including its error on stdout, after verifying its manifest.
    #[command(after_help = VMM_FAILURE_HELP)]
    Inspect {
        /// Run directory, as printed by `run` or `replay`.
        run: PathBuf,
        /// Print the full record as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum LogStream {
    /// Raw COM1 console bytes.
    Console,
    /// Raw management transport bytes.
    Mgmt,
}
impl From<LogStream> for logs::Stream {
    fn from(value: LogStream) -> Self {
        match value {
            LogStream::Console => Self::Console,
            LogStream::Mgmt => Self::Mgmt,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum TemplateArg {
    /// Three-member etcd cluster with an acknowledgement-ledger checker.
    Etcd,
    /// Redis with a PING/SET/GET checker.
    Redis,
}

impl From<TemplateArg> for Template {
    fn from(template: TemplateArg) -> Self {
        match template {
            TemplateArg::Etcd => Template::Etcd,
            TemplateArg::Redis => Template::Redis,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum TimebaseArg {
    /// Count retired guest branches with the host PMU.
    Counter,
    /// Advance time at guest exits, syscalls and clock reads.
    Pv,
}

impl From<TimebaseArg> for Timebase {
    fn from(timebase: TimebaseArg) -> Self {
        match timebase {
            TimebaseArg::Counter => Self::Counter,
            TimebaseArg::Pv => Self::Pv,
        }
    }
}

#[derive(Args)]
struct ComposeArg {
    /// Compose file, or a directory containing compose.yaml.
    #[arg(default_value = ".")]
    compose: PathBuf,
}

impl ComposeArg {
    fn load(&self) -> Result<config::Simulation> {
        let path = defaults::compose_file(&self.compose)?;
        config::load(&path, &std::env::vars().collect())
    }
}

#[derive(Args)]
struct CpuArg {
    /// Host CPU to pin the VMM to [default: first free core other than CPU 0's].
    #[arg(long, env = "SEEDBOX_CPU", value_parser = clap::value_parser!(u32).range(0..1024))]
    cpu: Option<u32>,
    /// Run even when this CPU's branch counter profile is unqualified; its results are unverified.
    #[arg(long)]
    allow_unqualified_cpu: bool,
    /// No effect: every built engine and guest kernel is admitted. Kept so existing scripts parse.
    #[arg(long)]
    allow_dev_engine: bool,
}

impl CpuArg {
    /// The host's branch counter profile, refused when unqualified unless allowed.
    fn admit(&self, counter: CounterProfile) -> Result<CounterProfile> {
        admit_counter(counter, self.allow_unqualified_cpu)?;
        Ok(counter)
    }
}

#[derive(Args)]
struct StateArg {
    /// Directory holding the content store and recorded runs.
    #[arg(long, env = "SEEDBOX_STATE", default_value = ".seedbox")]
    state: PathBuf,
}

#[derive(Args)]
struct Transfer {
    /// Maximum unique object payload and materialized log bytes.
    #[arg(long, default_value_t = bundle::DEFAULT_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..))]
    max_bytes: u64,
}

impl StateArg {
    fn open(&self) -> Result<(ContentStore, Orchestrator)> {
        let unusable = |error| Error::Setup(format!("--state {}: {error}", self.state.display()));
        std::fs::create_dir_all(&self.state).map_err(unusable)?;
        let path = self.state.canonicalize().map_err(unusable)?;
        let store = ContentStore::new(path.join("objects"));
        Ok((store.clone(), Orchestrator::new(store, path.join("runs"))))
    }
}

#[derive(Args)]
#[command(next_help_heading = "Runtime inputs")]
struct KvmInputs {
    /// Guest kernel bzImage [default: bzImage in $SEEDBOX_ARTIFACTS, $SEEDBOX_WORK/artifacts,
    /// ./artifacts, then ~/seedbox-sim/artifacts for the user and any sudo caller].
    #[arg(long, env = "SEEDBOX_KERNEL")]
    kernel: Option<PathBuf>,
    /// VMM binary [default: next to an installed seedbox, then this checkout's `cargo build-vmm`
    /// output (built by `cargo run` for run/prepare if missing), then PATH].
    #[arg(long, env = "SEEDBOX_VMM")]
    vmm: Option<PathBuf>,
}

#[derive(Args)]
#[command(next_help_heading = "Runtime inputs")]
struct RuntimeInputs {
    #[command(flatten)]
    kvm: KvmInputs,
    /// Static guest supervisor [default: the copy built into this binary].
    #[arg(long, env = "SEEDBOX_GUEST_INIT")]
    guest_init: Option<PathBuf>,
    /// Static Linux amd64 BusyBox [default: busybox on PATH, if static].
    #[arg(long, env = "SEEDBOX_BUSYBOX")]
    busybox: Option<PathBuf>,
    /// Static Linux amd64 mke2fs, for named volumes [default: mke2fs on PATH, if static].
    #[arg(long, env = "SEEDBOX_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Docker-compatible client for `build:` services.
    #[arg(long, default_value = "podman")]
    builder: PathBuf,
    /// Guest RAM per service, in MiB.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(64..=65536))]
    memory_mb: u32,
    /// Timebase for guest progress; changes run identity and results.
    #[arg(long, value_enum, default_value_t = TimebaseArg::Counter)]
    timebase: TimebaseArg,
    /// Also record and compare every machine's RAM and registers, retaining per-page RAM hashes.
    #[arg(long)]
    full_state: bool,
    /// Keep a seeded subset of unflushed disk sectors on crash (512-byte atomicity; off by default).
    #[arg(long)]
    torn_writes: bool,
}

impl RuntimeInputs {
    /// Resolve every input and build the matching preparer and executor.
    fn backends(
        &self,
        store: &ContentStore,
        cpu: Option<u32>,
        counter: CounterProfile,
        scratch: &Path,
    ) -> Result<(OciPreparer, VmmExecutor)> {
        let engine = defaults::engine(self.kvm.vmm.clone())?;
        let witness = if self.full_state {
            WitnessMode::FullState
        } else {
            WitnessMode::Exits
        };
        let executor = VmmExecutor::new(&engine, store, cpu, self.memory_mb, witness, counter)?
            .with_torn_writes(self.torn_writes)
            .with_timebase(self.timebase.into());
        let preparer = self.preparer(engine, store, scratch, executor.profile())?;
        Ok((preparer, executor))
    }

    fn preparer(
        &self,
        engine: PathBuf,
        store: &ContentStore,
        scratch: &Path,
        profile: ExecutionProfile,
    ) -> Result<OciPreparer> {
        let mut tools = OciTools {
            docker: self.builder.clone(),
            ..OciTools::default()
        };
        for tool in [&mut tools.skopeo, &mut tools.umoci] {
            if let Ok(resolved) = defaults::preparation_tool(tool) {
                *tool = resolved;
            }
        }
        Ok(OciPreparer {
            tools,
            kernel: defaults::kernel(self.kvm.kernel.clone())?,
            engine,
            init: defaults::guest_init(self.guest_init.clone(), store, scratch)?,
            busybox: defaults::busybox(self.busybox.clone())?,
            mke2fs: defaults::mke2fs(self.mke2fs.clone())?,
            profile,
        })
    }
}

#[derive(Args)]
#[command(next_help_heading = "Preparation")]
struct BuildOptions {
    /// Clamp new build-layer timestamps to this Unix epoch (qualified native Podman only) [default: absent].
    #[arg(long, allow_hyphen_values = true, value_parser = clap::value_parser!(u32))]
    build_epoch: Option<u32>,
}

impl BuildOptions {
    fn configure(&self, simulation: &config::Simulation, inputs: &mut RuntimeInputs) -> Result<()> {
        if simulation
            .services
            .values()
            .any(|service| matches!(service.source, config::ImageSource::Build { .. }))
        {
            match defaults::preparation_tool(&inputs.builder) {
                Ok(builder) => inputs.builder = builder,
                Err(error) if self.build_epoch.is_some() => return Err(Error::Setup(format!("--build-epoch: {error}; use the qualified project-Nix local Podman or omit --build-epoch"))),
                Err(_) => (), // Preserve ordinary custom-builder discovery at execution.
            }
            OciTools {
                docker: inputs.builder.clone(),
                build_epoch: self.build_epoch,
                ..OciTools::default()
            }
            .validate_build_epoch()?;
        }
        Ok(())
    }
}

/// Stop-placement settings: they change speed, never results.
#[derive(Args)]
#[command(next_help_heading = "Performance (never changes results)")]
struct Performance {
    /// PMU approach and stepping distance in branches: a count or `adaptive`.
    #[arg(long, default_value = "adaptive", value_parser = parse_margin)]
    margin: String,
    /// Initial undo margin in branches; 0 disables undo.
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u64).range(0..=1 << 18))]
    undo: u64,
}

fn parse_margin(value: &str) -> std::result::Result<String, String> {
    match value.parse::<u64>() {
        _ if value == "adaptive" => Ok(value.into()),
        Ok(branches) if branches > 0 => Ok(value.into()),
        _ => Err("expected `adaptive` or a positive branch count".into()),
    }
}

impl Performance {
    fn apply(&self, executor: VmmExecutor) -> Result<VmmExecutor> {
        executor.with_margin(&self.margin)?.with_undo(self.undo)
    }
}

pub fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Pause { run } => {
            let status = seedbox::host_control::set_paused(&run, true)?;
            println!("{}: {}", word(&status), run.display());
            Ok(0)
        }
        Command::Resume { run } => {
            let status = seedbox::host_control::set_paused(&run, false)?;
            println!("{}: {}", word(&status), run.display());
            Ok(0)
        }
        Command::Init {
            directory,
            template,
        } => {
            if directory.exists() {
                return Err(Error::Setup(format!(
                    "{}: already exists; choose a new directory",
                    directory.display()
                )));
            }
            seedbox::templates::init(&directory, template.into())?;
            println!("CREATED: {}", directory.display());
            Ok(0)
        }
        Command::Validate { compose, json } => {
            let simulation = compose.load()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&simulation)?);
            } else {
                println!(
                    "VALID: {} ({} services; image compatibility is checked by `prepare`)",
                    simulation.name,
                    simulation.services.len()
                );
            }
            Ok(0)
        }
        Command::Doctor { cpu, inputs, json } => {
            let engine = defaults::engine_lookup(inputs.vmm)?;
            let kernel = defaults::kernel(inputs.kernel)?;
            let report = VmmExecutor::doctor(&engine, &kernel, cpu.cpu)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("VALID: compatible execution profile");
                println!("cpu: {}", report.cpu);
                println!(
                    "cpus: {}",
                    report
                        .cpus
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                );
                println!("engine: {}", report.engine.hex());
                println!("kernel: {}", report.kernel.hex());
            }
            Ok(0)
        }
        Command::Prepare {
            compose,
            output,
            state,
            mut inputs,
            build,
        } => {
            if output.exists() {
                return Err(Error::Setup(format!(
                    "--output {}: already exists",
                    output.display()
                )));
            }
            let simulation = compose.load()?;
            build.configure(&simulation, &mut inputs)?;
            let (store, _) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let counter = CounterProfile::host().unwrap_or(CounterProfile::Zen);
            let (mut preparer, _) = inputs.backends(&store, None, counter, scratch.path())?;
            preparer.tools.build_epoch = build.build_epoch;
            let prepared = preparer.prepare(&simulation, &store)?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)?;
            serde_json::to_writer_pretty(file, &prepared)?;
            println!("PREPARED: {}", output.display());
            Ok(0)
        }
        Command::Run {
            compose,
            seed,
            checkpoint_at_ns,
            branch_seed,
            cpu,
            state,
            mut inputs,
            build,
            performance,
        } => {
            let simulation = compose.load()?;
            if checkpoint_at_ns.is_some_and(|t| t >= simulation.limits.simulated_time_ns) {
                return Err(Error::Setup("--checkpoint-at-ns must be before the simulated time limit; choose an earlier checkpoint".into()));
            }
            build.configure(&simulation, &mut inputs)?;
            let (store, orchestrator) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let detected = CounterProfile::host();
            let counter = *detected.as_ref().unwrap_or(&CounterProfile::Zen);
            let (mut preparer, executor) =
                inputs.backends(&store, cpu.cpu, counter, scratch.path())?;
            cpu.admit(detected?)?;
            let executor = executor.with_unqualified_cpu(cpu.allow_unqualified_cpu);
            let executor = match (checkpoint_at_ns, branch_seed) {
                (Some(at), Some(branch)) => executor.with_checkpoint(at, branch),
                _ => executor,
            };
            preparer.profile = executor.profile();
            preparer.tools.build_epoch = build.build_epoch;
            let executor = performance.apply(executor)?;
            // Backend admission checks the selected core before preparation.
            let cancellation = CancellationToken::default();
            let _signals = Signals::register(&cancellation)?;
            let result = orchestrator.run(
                &simulation,
                RunOptions { seed, cancellation },
                &preparer,
                &executor,
            )?;
            Ok(report(&result))
        }
        Command::Explore {
            compose,
            seed,
            count,
            workers,
            json,
            cpu,
            state,
            mut inputs,
            build,
            performance,
        } => {
            let resources = Resources::start();
            let simulation = compose.load()?;
            build.configure(&simulation, &mut inputs)?;
            let cancellation = CancellationToken::default();
            let options = ExploreOptions {
                seed,
                count,
                workers,
                cancellation: cancellation.clone(),
            };
            exploration::bounds(&simulation, &options)?;
            if workers > 1 && cpu.cpu.is_some() {
                return Err(Error::Setup("--workers: parallel exploration requires automatic core selection; omit --cpu and SEEDBOX_CPU".into()));
            }
            let (store, orchestrator) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let detected = CounterProfile::host();
            let counter = *detected.as_ref().unwrap_or(&CounterProfile::Zen);
            let (mut preparer, executor) =
                inputs.backends(&store, cpu.cpu, counter, scratch.path())?;
            preparer.tools.build_epoch = build.build_epoch;
            preparer.tools.skopeo = defaults::preparation_tool(&preparer.tools.skopeo)?;
            preparer.tools.umoci = defaults::preparation_tool(&preparer.tools.umoci)?;
            if simulation
                .services
                .values()
                .any(|s| matches!(s.source, config::ImageSource::Build { .. }))
            {
                preparer.tools.docker = defaults::preparation_tool(&preparer.tools.docker)?;
            }
            cpu.admit(detected?)?;
            let executor = executor.with_unqualified_cpu(cpu.allow_unqualified_cpu);
            preparer.profile = executor.profile();
            let executor = performance.apply(executor)?;
            // Exploration retains any backend admission refusal in its batch checkpoint.
            let _signals = Signals::register(&cancellation)?;
            let mut result = orchestrator.explore(&simulation, options, &preparer, &executor)?;
            result.record_resources(resources.finish())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("{}: {}", word(&result.outcome), result.directory.display());
                println!("seeds: {}..={}", result.first_seed, result.last_seed);
                println!("workers: {}", result.workers);
                println!("peak-running: {}", result.peak_running);
                println!(
                    "active-attempt-seconds: {:.3}",
                    result.active_attempt_seconds
                );
                println!("attempted: {}", result.attempts.len());
                println!("pass: {}", result.counts.pass);
                println!("fail: {}", result.counts.fail);
                println!("timeout: {}", result.counts.timeout);
                println!("error-count: {}", result.counts.error);
                println!("incomplete: {}", result.counts.incomplete);
                println!("unfinished: {}", result.counts.unfinished);
                println!("undispatched: {}", result.undispatched_count);
                println!("elapsed-seconds: {:.3}", result.total_seconds);
                for attempt in &result.attempts {
                    if let (Some(directory), Some(outcome)) = (&attempt.directory, &attempt.outcome)
                    {
                        eprintln!(
                            "seed {}: {} {}",
                            attempt.seed,
                            word(outcome),
                            directory.display()
                        );
                        if let Some(error) = &attempt.error {
                            eprintln!("error: seed {}: {error}", attempt.seed);
                            diagnostics::vmm_failure(directory, error);
                        }
                    }
                }
                if let Some(error) = &result.error {
                    eprintln!("error: {error}");
                }
            }
            Ok(outcome_code(&result.outcome))
        }
        Command::Replay {
            run,
            cpu,
            state,
            performance,
            vmm,
            allow_engine_change,
        } => {
            let run = defaults::run_directory(run)?;
            let previous = orchestration::inspect(&run)?;
            let timebase = recorded_timebase(&previous.identity.inputs.profile)?;
            let detected = CounterProfile::host();
            let counter = *detected.as_ref().unwrap_or(&CounterProfile::Zen);
            // Historical/incompatible records must reach the retained-proof gate
            // and remain captured as NOT_COMPARABLE before querying this host.
            if seedbox::admission::validate_profile(&previous.identity.inputs.profile).is_ok() {
                cpu.admit(detected?)?;
                same_counter(
                    &run,
                    recorded_counter(&previous.identity.inputs.profile)?,
                    counter,
                )?;
            }
            let (store, orchestrator) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let profile = &previous.identity.inputs.profile;
            let engine = match vmm {
                Some(path) => defaults::existing("--vmm", path)?,
                None => store.materialize(&profile.engine, scratch.path())?,
            };
            let memory = profile
                .semantic_parameters
                .get("memory_mb")
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| Error::Integrity("recorded profile missing memory_mb".into()))?;
            let executor = performance.apply(
                VmmExecutor::new(
                    &engine,
                    &store,
                    cpu.cpu,
                    memory,
                    profile.witness_mode.clone(),
                    counter,
                )?
                .with_timebase(timebase)
                .with_torn_writes(recorded_torn_writes(profile)?)
                .with_unqualified_cpu(cpu.allow_unqualified_cpu)
                .with_recorded_checkpoint(profile)?,
            )?;
            let cancellation = CancellationToken::default();
            let _signals = Signals::register(&cancellation)?;
            let result = orchestrator.replay_with_options(
                &run,
                &executor,
                ReplayOptions {
                    cancellation,
                    allow_engine_change,
                },
            )?;
            Ok(report(&result))
        }
        Command::Export {
            run,
            output,
            state,
            transfer,
        } => {
            let run = defaults::run_directory(run)?;
            let output = match output {
                Some(path) => path,
                None => {
                    let name = run.file_name().ok_or_else(|| {
                        Error::Setup(format!("{}: no run directory name", run.display()))
                    })?;
                    let mut name = name.to_os_string();
                    name.push(".seedbox.tar");
                    PathBuf::from(name)
                }
            };
            if output.symlink_metadata().is_ok() {
                return Err(Error::Setup(format!(
                    "--output {}: already exists; choose a new file",
                    output.display()
                )));
            }
            let cancellation = CancellationToken::default();
            let _signals = Signals::register(&cancellation)?;
            let options = bundle::Options {
                max_bytes: transfer.max_bytes,
                cancellation,
            };
            let store = ContentStore::new(state.state.join("objects"));
            let record = bundle::export(&run, &store, &output, &options).map_err(|e| {
                Error::Setup(format!(
                    "export {} to {}: {e}",
                    run.display(),
                    output.display()
                ))
            })?;
            println!("EXPORTED: {}", output.display());
            print_omissions(&record);
            Ok(0)
        }
        Command::Import {
            bundle: path,
            state,
            transfer,
        } => {
            let path = defaults::existing("bundle", path)?;
            let cancellation = CancellationToken::default();
            let _signals = Signals::register(&cancellation)?;
            let options = bundle::Options {
                max_bytes: transfer.max_bytes,
                cancellation,
            };
            // Import performs its own no-follow opening; StateArg::open would
            // canonicalize away root symlink evidence.
            let run = bundle::import(&path, &state.state, &options).map_err(|e| {
                Error::Setup(format!(
                    "import {} into {}: {e}",
                    path.display(),
                    state.state.display()
                ))
            })?;
            println!("IMPORTED: {}", run.directory.display());
            print_omissions(&run.record);
            Ok(0)
        }
        Command::Diff { run_a, run_b, json } => {
            let comparison = diff::diff(&run_a, &run_b)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&comparison)?);
            } else {
                println!(
                    "{}: {} <> {}",
                    word(&comparison.status),
                    run_a.display(),
                    run_b.display()
                );
                println!(
                    "world witness: {}",
                    serde_json::to_string(&comparison.world_witness)?
                );
                for machine in &comparison.machines {
                    println!("machine: {}", serde_json::to_string(machine)?);
                    if let Some(pages) = &machine.ram_pages {
                        diagnostics::ram_pages(&machine.machine, pages);
                    }
                }
                println!(
                    "first record: {}",
                    serde_json::to_string(&comparison.first_record)?
                );
                for (label, differences) in [
                    ("identity", &comparison.identity_differences),
                    ("result", &comparison.result_differences),
                    ("evidence", &comparison.evidence_differences),
                ] {
                    for difference in differences {
                        println!("{label}: {}", serde_json::to_string(difference)?);
                    }
                }
                for limitation in &comparison.limitations {
                    println!("limitation: {limitation}");
                }
            }
            Ok(match comparison.status {
                diff::Status::Match => 0,
                diff::Status::Different => 1,
            })
        }
        Command::Logs {
            run,
            machine,
            stream,
            since_ns,
            until_ns,
            json,
        } => {
            let run = defaults::run_directory(run)?;
            let mut reader = logs::Reader::open(
                &run,
                &logs::Filter {
                    machine,
                    stream: stream.map(Into::into),
                    since_ns,
                    until_ns,
                },
            )?;
            use std::io::Write;
            let mut output = std::io::BufWriter::new(std::io::stdout().lock());
            if json {
                writeln!(
                    output,
                    "{{\n  \"status\": {},\n  \"omissions\": {},\n  \"chunks\": [",
                    serde_json::to_string(&reader.report.status)?,
                    serde_json::to_string_pretty(&reader.report.omissions)?
                )?;
            } else {
                let mut diagnostics = std::io::stderr().lock();
                writeln!(diagnostics, "logs: {}", run.display())?;
                for (name, reason) in &reader.report.omissions {
                    writeln!(
                        diagnostics,
                        "omission: {}",
                        serde_json::to_string(&(name, reason))?
                    )?;
                }
            }
            for index in 0..reader.report.chunks.len() {
                if json {
                    output.write_all(b"    ")?;
                    reader.write_chunk_json(index, &mut output)?;
                    if index + 1 < reader.report.chunks.len() {
                        output.write_all(b",")?;
                    }
                    output.write_all(b"\n")?;
                } else {
                    reader.copy_chunk(index, &mut output)?;
                }
            }
            if json {
                writeln!(output, "  ]\n}}")?;
            }
            output.flush()?;
            Ok(0)
        }
        Command::Inspect { run, json } => {
            let run = defaults::run_directory(run)?;
            let record = orchestration::inspect(&run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&record)?);
                return Ok(0);
            }
            println!(
                "{}: {} (seed {})",
                word(&record.outcome),
                record.attempt,
                record.identity.seed
            );
            print_timebase(&record);
            if let Some(replay) = record.replay {
                println!("replay: {}", word(&replay.status));
            }
            if let Some(error) = record.error {
                println!("error: {error}");
                diagnostics::vmm_failure(&run, &error);
            }
            Ok(0)
        }
    }
}

/// Refuse an unqualified counter profile unless the caller allowed it, and
/// then say that its results are unverified.
fn admit_counter(counter: CounterProfile, allow_unqualified: bool) -> Result<()> {
    if counter.qualified() {
        return Ok(());
    }
    if !allow_unqualified {
        return Err(Error::Setup(format!(
            "this host's branch counter profile {} is unqualified, so its results are \
             unverified; pass --allow-unqualified-cpu to run anyway",
            counter.name()
        )));
    }
    eprintln!(
        "warning: branch counter profile {} is unqualified; results are unverified",
        counter.name()
    );
    Ok(())
}

/// A run replays only with the branch counter it was recorded with.
fn same_counter(run: &Path, recorded: CounterProfile, host: CounterProfile) -> Result<()> {
    if recorded == host {
        return Ok(());
    }
    Err(Error::Setup(format!(
        "{}: recorded with the {} branch counter, but this host has {}; \
         replay it on a host with the same counter",
        run.display(),
        recorded.name(),
        host.name()
    )))
}

/// Print a finished run and return its exit status (see `EXIT_CODES`).
fn report(run: &orchestration::Run) -> u8 {
    // Scripts parse the run directory from this one `OUTCOME: path` line.
    println!("{}: {}", word(&run.record.outcome), run.directory.display());
    print_timebase(&run.record);
    if let Some(error) = &run.record.error {
        eprintln!("{error}");
        diagnostics::vmm_failure(&run.directory, error);
    }
    if let Some(replay) = &run.record.replay {
        println!("replay: {}", word(&replay.status));
        diagnostics::replay_pages(replay);
        return match replay.status {
            ReplayStatus::Match => 0,
            ReplayStatus::Mismatch => 1,
            ReplayStatus::NotComparable => 2,
        };
    }
    outcome_code(&run.record.outcome)
}
fn outcome_code(outcome: &Outcome) -> u8 {
    match outcome {
        Outcome::Pass => 0,
        Outcome::Fail => 1,
        Outcome::Error => 2,
        Outcome::Incomplete => 3,
        Outcome::Timeout => 124,
    }
}

/// The serialized (SCREAMING_SNAKE_CASE) name of a status enum, as in `--json`.
fn word(value: &impl serde::Serialize) -> String {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(word)) => word,
        _ => "UNKNOWN".into(),
    }
}

/// Invocation-wide CPU deltas and separate process-family RSS high-water marks.
struct Resources {
    #[cfg(unix)]
    self_usage: Option<libc::rusage>,
    #[cfg(unix)]
    child_usage: Option<libc::rusage>,
}
impl Resources {
    fn start() -> Self {
        Self {
            #[cfg(unix)]
            self_usage: usage(libc::RUSAGE_SELF),
            #[cfg(unix)]
            child_usage: usage(libc::RUSAGE_CHILDREN),
        }
    }
    fn finish(self) -> Option<exploration::ProcessResources> {
        #[cfg(unix)]
        {
            let current = usage(libc::RUSAGE_SELF)?;
            let children = usage(libc::RUSAGE_CHILDREN)?;
            let before = self.self_usage?;
            let before_children = self.child_usage?;
            let seconds =
                |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1_000_000.0;
            let bytes = |rss: libc::c_long| {
                if cfg!(target_os = "macos") {
                    rss as u64
                } else {
                    (rss as u64).saturating_mul(1024)
                }
            };
            Some(exploration::ProcessResources {
                self_user_seconds: seconds(current.ru_utime) - seconds(before.ru_utime),
                self_system_seconds: seconds(current.ru_stime) - seconds(before.ru_stime),
                reaped_children_user_seconds: seconds(children.ru_utime)
                    - seconds(before_children.ru_utime),
                reaped_children_system_seconds: seconds(children.ru_stime)
                    - seconds(before_children.ru_stime),
                self_rss_high_water_bytes: bytes(current.ru_maxrss),
                reaped_children_rss_high_water_bytes: bytes(children.ru_maxrss),
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }
}
#[cfg(unix)]
fn usage(who: libc::c_int) -> Option<libc::rusage> {
    let mut usage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(who, &mut usage) } == 0 {
        Some(usage)
    } else {
        None
    }
}

fn print_timebase(record: &orchestration::RunRecord) {
    let timebase = record
        .identity
        .inputs
        .profile
        .semantic_parameters
        .get("timebase")
        .map(String::as_str)
        .unwrap_or("counter");
    println!("timebase: {timebase}");
}

fn print_omissions(record: &orchestration::RunRecord) {
    if !record.omitted_logs.is_empty() {
        println!("omitted files: {}", record.omitted_logs.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_on_another_vendor_names_both_counters() {
        let run = Path::new("/runs/abc");
        assert!(same_counter(run, CounterProfile::Zen, CounterProfile::Zen).is_ok());
        let error =
            same_counter(run, CounterProfile::Zen, CounterProfile::IntelIceLake).unwrap_err();
        assert_eq!(
            error.to_string(),
            "/runs/abc: recorded with the amd-zen branch counter, but this host has \
             intel-0x5111c4; replay it on a host with the same counter"
        );
    }

    #[test]
    fn unqualified_counters_need_explicit_permission() {
        assert!(admit_counter(CounterProfile::Zen, false).is_ok());
        assert_eq!(
            admit_counter(CounterProfile::IntelSkylake, false)
                .unwrap_err()
                .to_string(),
            "this host's branch counter profile intel-0x5101c4 is unqualified, so its \
             results are unverified; pass --allow-unqualified-cpu to run anyway"
        );
        assert!(admit_counter(CounterProfile::IntelSkylake, true).is_ok());
    }
}
