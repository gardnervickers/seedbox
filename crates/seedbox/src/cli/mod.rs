//! The `seedbox` command line. Style rules for changes: `AGENTS.md`, "CLI".

mod defaults;
mod signals;

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox::{
    config,
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, PreparationBackend, WitnessMode,
    },
    oci::OciTools,
    orchestration::{self, Orchestrator, Outcome, ReplayOptions, ReplayStatus, RunOptions},
    preparation::OciPreparer,
    store::ContentStore,
    templates::Template,
    vmm::VmmExecutor,
    Error, Result,
};
use signals::Signals;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

const EXIT_CODES: &str = "\
Exit status:
  0    PASS, or a replay MATCH
  1    FAIL, or a replay MISMATCH
  2    ERROR, a replay that is NOT_COMPARABLE, or a usage or setup error
  3    INCOMPLETE (wall-time limit or cancelled)
  124  TIMEOUT (simulated-time limit)";

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
    /// Check a Compose file against the supported subset, without images or guests.
    Validate {
        #[command(flatten)]
        compose: ComposeArg,
        /// Print the resolved configuration as JSON (includes environment values).
        #[arg(long)]
        json: bool,
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
    },
    /// Prepare images and run a simulation (Linux KVM host, root).
    #[command(after_help = EXIT_CODES)]
    Run {
        #[command(flatten)]
        compose: ComposeArg,
        /// Simulation seed [default: x-seedbox.seed from the Compose file].
        #[arg(long)]
        seed: Option<u64>,
        #[command(flatten)]
        cpu: CpuArg,
        #[command(flatten)]
        state: StateArg,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[command(flatten)]
        performance: Performance,
    },
    /// Re-execute a recorded run from its retained inputs and compare (Linux KVM host, root).
    #[command(after_help = EXIT_CODES)]
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
    /// Show a recorded run after verifying its manifest.
    Inspect {
        /// Run directory, as printed by `run` or `replay`.
        run: PathBuf,
        /// Print the full record as JSON.
        #[arg(long)]
        json: bool,
    },
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
}

#[derive(Args)]
struct StateArg {
    /// Directory holding the content store and recorded runs.
    #[arg(long, env = "SEEDBOX_STATE", default_value = ".seedbox")]
    state: PathBuf,
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
struct RuntimeInputs {
    /// Guest kernel bzImage [default: bzImage in $SEEDBOX_ARTIFACTS, $SEEDBOX_WORK/artifacts,
    /// ./artifacts, then ~/seedbox-sim/artifacts for the user and any sudo caller].
    #[arg(long, env = "SEEDBOX_KERNEL")]
    kernel: Option<PathBuf>,
    /// VMM binary [default: seedbox-vmm next to this binary or in target/release, then on PATH].
    #[arg(long, env = "SEEDBOX_VMM")]
    vmm: Option<PathBuf>,
    /// Static guest supervisor [default: the copy built into this binary].
    #[arg(long, env = "SEEDBOX_GUEST_INIT")]
    guest_init: Option<PathBuf>,
    /// Static Linux amd64 BusyBox [default: busybox on PATH, if static].
    #[arg(long, env = "SEEDBOX_BUSYBOX")]
    busybox: Option<PathBuf>,
    /// Docker-compatible client for `build:` services.
    #[arg(long, default_value = "podman")]
    builder: PathBuf,
    /// Guest RAM per service, in MiB.
    #[arg(long, default_value_t = 1024, value_parser = clap::value_parser!(u32).range(64..=65536))]
    memory_mb: u32,
    /// Also record and compare every machine's RAM and registers.
    #[arg(long)]
    full_state: bool,
}

impl RuntimeInputs {
    /// Resolve every input and build the matching preparer and executor.
    fn backends(
        &self,
        store: &ContentStore,
        cpu: Option<u32>,
        scratch: &Path,
    ) -> Result<(OciPreparer, VmmExecutor)> {
        let engine = defaults::engine(self.vmm.clone())?;
        let witness = if self.full_state {
            WitnessMode::FullState
        } else {
            WitnessMode::Exits
        };
        let executor = VmmExecutor::new(&engine, store, cpu, self.memory_mb, witness)?;
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
        Ok(OciPreparer {
            tools: OciTools {
                docker: self.builder.clone(),
                ..OciTools::default()
            },
            kernel: defaults::kernel(self.kernel.clone())?,
            engine,
            init: defaults::guest_init(self.guest_init.clone(), store, scratch)?,
            busybox: defaults::busybox(self.busybox.clone())?,
            profile,
        })
    }
}

/// Stop-placement settings: they change speed, never results.
#[derive(Args)]
#[command(next_help_heading = "Performance (never changes results)")]
struct Performance {
    /// Branches before a soft stop at which syscall exits are requested: a count or `adaptive`.
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
        Command::Prepare {
            compose,
            output,
            state,
            inputs,
        } => {
            if output.exists() {
                return Err(Error::Setup(format!(
                    "--output {}: already exists",
                    output.display()
                )));
            }
            let simulation = compose.load()?;
            let (store, _) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let (preparer, _) = inputs.backends(&store, None, scratch.path())?;
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
            cpu,
            state,
            inputs,
            performance,
        } => {
            let simulation = compose.load()?;
            let (store, orchestrator) = state.open()?;
            let scratch = tempfile::tempdir()?;
            let (preparer, executor) = inputs.backends(&store, cpu.cpu, scratch.path())?;
            let executor = performance.apply(executor)?;
            // After the fast input checks, before slow image preparation.
            require_kvm_host()?;
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
            require_kvm_host()?;
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
            let executor = performance.apply(VmmExecutor::new(
                &engine,
                &store,
                cpu.cpu,
                memory,
                profile.witness_mode.clone(),
            )?)?;
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
            if let Some(replay) = record.replay {
                println!("replay: {}", word(&replay.status));
            }
            if let Some(error) = record.error {
                println!("error: {error}");
            }
            Ok(0)
        }
    }
}

/// Fail fast when this host cannot execute: no KVM, or no MSR access.
fn require_kvm_host() -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Setup(
            "run and replay require the qualified Linux KVM host".into(),
        ));
    }
    for (path, fix) in [
        (
            "/dev/kvm",
            "enable KVM and run as root (e.g. sudo -E env PATH=\"$PATH\" seedbox ...)",
        ),
        (
            "/dev/cpu/0/msr",
            "run as root (e.g. sudo -E env PATH=\"$PATH\" seedbox ...) with the msr module loaded",
        ),
    ] {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| Error::Setup(format!("{path}: {error}; {fix}")))?;
    }
    Ok(())
}

/// Print a finished run and return its exit status (see `EXIT_CODES`).
fn report(run: &orchestration::Run) -> u8 {
    // Scripts parse the run directory from this one `OUTCOME: path` line.
    println!("{}: {}", word(&run.record.outcome), run.directory.display());
    if let Some(error) = &run.record.error {
        eprintln!("{error}");
    }
    if let Some(replay) = &run.record.replay {
        println!("replay: {}", word(&replay.status));
        return match replay.status {
            ReplayStatus::Match => 0,
            ReplayStatus::Mismatch => 1,
            ReplayStatus::NotComparable => 2,
        };
    }
    match run.record.outcome {
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
