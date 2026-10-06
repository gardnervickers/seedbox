use clap::{Args, Parser, Subcommand};
use seedbox::{
    config,
    execution::{CancellationToken, ExecutionBackend, PreparationBackend, WitnessMode},
    oci::OciTools,
    orchestration::{self, Orchestrator, Outcome, ReplayOptions, ReplayStatus, RunOptions},
    preparation::OciPreparer,
    store::ContentStore,
    vmm::VmmExecutor,
    Error, Result,
};
use std::{collections::BTreeMap, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(
    version,
    about = "Seedbox simulation configuration and recorded results"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a self-contained Compose example.
    Init {
        directory: PathBuf,
        #[arg(long, value_parser = ["etcd", "redis"])]
        template: String,
    },
    /// Resolve OCI inputs and produce retained guest artifacts without execution.
    Prepare {
        compose: PathBuf,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[arg(long, default_value = ".seedbox")]
        state: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Prepare OCI images and run a simulation on a qualified Linux KVM host.
    Run {
        compose: PathBuf,
        #[command(flatten)]
        inputs: RuntimeInputs,
        #[arg(long)]
        seed: Option<u64>,
        #[arg(long)]
        cpu: u32,
        #[arg(long, default_value = ".seedbox")]
        state: PathBuf,
        #[arg(long, default_value = "adaptive")]
        margin: String,
        /// Initial undo margin in branches; zero disables undo.
        #[arg(long, default_value_t = 256)]
        undo: u64,
    },
    /// Fresh execution using only retained inputs, without image preparation.
    Replay {
        run: PathBuf,
        #[arg(long)]
        cpu: u32,
        #[arg(long, default_value = ".seedbox")]
        state: PathBuf,
        #[arg(long, default_value = "adaptive")]
        margin: String,
        #[arg(long, default_value_t = 256)]
        undo: u64,
        /// Explicitly compare an alternate VMM binary against the recorded run.
        #[arg(long, requires = "allow_engine_change")]
        vmm: Option<PathBuf>,
        #[arg(long)]
        allow_engine_change: bool,
    },
    /// Validate the authoring subset without resolving images or running guests.
    Validate {
        compose: PathBuf,
        /// Print the resolved configuration (including environment values).
        #[arg(long)]
        json: bool,
    },
    /// Read and verify a saved run manifest.
    Inspect {
        run: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Args)]
struct RuntimeInputs {
    #[arg(long)]
    kernel: PathBuf,
    #[arg(long)]
    vmm: PathBuf,
    #[arg(long)]
    guest_init: PathBuf,
    /// Static Linux amd64 BusyBox (defaults to SEEDBOX_BUSYBOX from the Nix shell).
    #[arg(long)]
    busybox: Option<PathBuf>,
    #[arg(long, default_value_t = 1024)]
    memory_mb: u32,
    #[arg(long)]
    full_state: bool,
    /// Docker-compatible build client; the Nix Linux shell supplies podman.
    #[arg(long, default_value = "podman")]
    builder: PathBuf,
}

impl RuntimeInputs {
    fn preparer(&self, profile: seedbox::execution::ExecutionProfile) -> Result<OciPreparer> {
        let busybox = self
            .busybox
            .clone()
            .or_else(|| std::env::var_os("SEEDBOX_BUSYBOX").map(PathBuf::from))
            .ok_or_else(|| {
                Error::Execution("provide --busybox or enter the Linux Nix shell".into())
            })?;
        Ok(OciPreparer {
            tools: OciTools {
                docker: self.builder.clone(),
                ..OciTools::default()
            },
            kernel: self.kernel.clone(),
            engine: self.vmm.clone(),
            init: self.guest_init.clone(),
            busybox,
            profile,
        })
    }
    fn executor(&self, store: &ContentStore, cpu: u32) -> Result<VmmExecutor> {
        VmmExecutor::new(
            &self.vmm,
            store,
            cpu,
            self.memory_mb,
            if self.full_state {
                WitnessMode::FullState
            } else {
                WitnessMode::Exits
            },
        )
    }
}

struct Signals(Vec<signal_hook::SigId>);
impl Signals {
    fn register(token: &CancellationToken) -> Result<Self> {
        let mut handlers = Self(Vec::new());
        for signal in [
            signal_hook::consts::SIGINT,
            signal_hook::consts::SIGTERM,
            signal_hook::consts::SIGHUP,
        ] {
            let token = token.clone();
            // The callback performs only an atomic store; cleanup happens in
            // normal executor code after the VMM is stopped and reaped.
            handlers
                .0
                .push(unsafe { signal_hook::low_level::register(signal, move || token.cancel()) }?);
        }
        Ok(handlers)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn state(path: PathBuf) -> Result<(ContentStore, Orchestrator)> {
    std::fs::create_dir_all(&path)?;
    let path = path.canonicalize()?;
    let store = ContentStore::new(path.join("objects"));
    Ok((store.clone(), Orchestrator::new(store, path.join("runs"))))
}

fn outcome(run: &orchestration::Run) -> u8 {
    println!(
        "{}: {}",
        format!("{:?}", run.record.outcome).to_ascii_uppercase(),
        run.directory.display()
    );
    if let Some(error) = &run.record.error {
        eprintln!("{error}");
    }
    if let Some(replay) = &run.record.replay {
        println!(
            "replay: {}",
            match replay.status {
                ReplayStatus::Match => "MATCH",
                ReplayStatus::Mismatch => "MISMATCH",
                ReplayStatus::NotComparable => "NOT_COMPARABLE",
            }
        );
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

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Init {
            directory,
            template,
        } => {
            let template = match template.as_str() {
                "etcd" => seedbox::templates::Template::Etcd,
                "redis" => seedbox::templates::Template::Redis,
                _ => unreachable!("clap validates template names"),
            };
            seedbox::templates::init(&directory, template)?;
            println!("CREATED: {}", directory.display());
        }
        Command::Prepare {
            compose,
            inputs,
            state: directory,
            output,
        } => {
            let simulation = config::load(&compose, &std::env::vars().collect())?;
            let (store, _) = state(directory)?;
            let executor = inputs.executor(&store, 0)?;
            let prepared = inputs
                .preparer(executor.profile())?
                .prepare(&simulation, &store)?;
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&output)?;
            serde_json::to_writer_pretty(file, &prepared)?;
            println!("PREPARED: {}", output.display());
        }
        Command::Run {
            compose,
            inputs,
            seed,
            cpu,
            state: directory,
            margin,
            undo,
        } => {
            if !cfg!(target_os = "linux") {
                return Err(Error::Execution(
                    "run requires the qualified Linux KVM host".into(),
                ));
            }
            let simulation = config::load(&compose, &std::env::vars().collect())?;
            let (store, orchestrator) = state(directory)?;
            let executor = inputs
                .executor(&store, cpu)?
                .with_margin(&margin)?
                .with_undo(undo)?;
            let preparer = inputs.preparer(executor.profile())?;
            let cancellation = CancellationToken::default();
            let _signals = Signals::register(&cancellation)?;
            let result = orchestrator.run(
                &simulation,
                RunOptions { seed, cancellation },
                &preparer,
                &executor,
            )?;
            return Ok(outcome(&result));
        }
        Command::Replay {
            run,
            cpu,
            state: directory,
            margin,
            undo,
            vmm,
            allow_engine_change,
        } => {
            if !cfg!(target_os = "linux") {
                return Err(Error::Execution(
                    "replay requires the qualified Linux KVM host".into(),
                ));
            }
            let previous = orchestration::inspect(&run)?;
            let (store, orchestrator) = state(directory)?;
            let temporary = tempfile::tempdir()?;
            let engine = match vmm {
                Some(path) => path,
                None => {
                    store.materialize(&previous.identity.inputs.profile.engine, temporary.path())?
                }
            };
            let profile = &previous.identity.inputs.profile;
            let memory = profile
                .semantic_parameters
                .get("memory_mb")
                .and_then(|value| value.parse().ok())
                .ok_or_else(|| Error::Integrity("recorded profile missing memory_mb".into()))?;
            let executor =
                VmmExecutor::new(&engine, &store, cpu, memory, profile.witness_mode.clone())?
                    .with_margin(&margin)?
                    .with_undo(undo)?;
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
            return Ok(outcome(&result));
        }
        Command::Validate { compose, json } => {
            let environment: BTreeMap<_, _> = std::env::vars().collect();
            let simulation = config::load(&compose, &environment)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&simulation)?);
            } else {
                println!(
                    "VALID: {} ({} services; image compatibility requires preparation)",
                    simulation.name,
                    simulation.services.len()
                );
            }
        }
        Command::Inspect { run, json } => {
            let record = orchestration::inspect(&run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&record)?);
            } else {
                println!(
                    "{}: {:?} (seed {})",
                    record.attempt, record.outcome, record.identity.seed
                );
                if let Some(replay) = record.replay {
                    println!("replay: {:?}", replay.status);
                }
                if let Some(error) = record.error {
                    println!("error: {error}");
                }
            }
        }
    }
    Ok(0)
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}
