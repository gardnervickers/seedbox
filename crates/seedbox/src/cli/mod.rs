//! The `seedbox` command line. Style rules for changes: `AGENTS.md`, "CLI".

mod backends;
mod bundles;
mod completions;
mod daemon;
mod defaults;
mod diagnostics;
mod faults;
mod observe;
mod output;
mod playback_args;
mod signals;
mod simulations;
use backends::{Executor, Preparer};
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
use seedbox_application::vmm::VmmExecutor;

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox_application::{
    config,
    execution::{CancellationToken, ExecutionBackend, PreparationBackend, WitnessMode},
    orchestration::Orchestrator,
    preparation::OciPreparer,
    store::ContentStore,
    templates::Template,
    Error, Result,
};
use signals::Signals;
use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

const EXIT_CODES: &str = "\
Exit status:
  0    PASS, or a replay/compare MATCH
  1    FAIL, replay MISMATCH, or compare MISMATCH
  2    ERROR, a replay that is NOT_COMPARABLE, or a usage or setup error
  3    INCOMPLETE (cancelled or otherwise incomplete)
       A cancelled or incomplete replay reports 2 (NOT_COMPARABLE)
  124  TIMEOUT";

#[derive(Parser)]
#[command(
    version = seedbox_application::engine::SIMULATOR_VERSION,
    override_usage = "seedbox [RUN OPTIONS] [COMPOSE]\n       seedbox <COMMAND>",
    after_help = "Run is the default: seedbox seedbox.yaml. See `seedbox help run` for run options.",
    about = "Deterministic simulation of Compose applications on real Linux machines",
    long_about = "Deterministic simulation of Compose applications on real Linux machines.\n\n\
                  A seed plus the retained inputs regenerate the same execution. \
                  `run` and `replay` support Linux x86-64 KVM (KVM device access) \
                  or native HVF on Apple Silicon Mac."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Generate shell integration with live simulation and machine completion.
    Completions {
        /// Shell to generate integration for.
        #[arg(value_enum)]
        shell: completions::Shell,
    },
    #[command(name = "__complete", hide = true)]
    Complete {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        words: Vec<String>,
    },
    /// Gracefully stop the local simulation server and its active work.
    Shutdown {
        /// Server state directory [default: user state directory].
        #[arg(long, env = "SEEDBOX_STATE")]
        state: Option<PathBuf>,
    },
    /// Serve the local simulation API in the foreground.
    Serve {
        /// Report startup to the launching client.
        #[arg(long, hide = true)]
        startup_handshake: bool,
        /// Server state directory [default: user state directory].
        #[arg(long, env = "SEEDBOX_STATE")]
        state: Option<PathBuf>,
        #[command(flatten)]
        inputs: RuntimeInputs,
    },
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
    /// Check execution-profile compatibility without images or guests (Linux KVM host; or Apple Silicon Mac).
    #[command(
        after_help = "Success means profile compatibility, not cross-host replay qualification. Doctor never builds or downloads missing runtime inputs; run `cargo xtask build-assets` or pass --kernel."
    )]
    Doctor {
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
    },
    /// Prepare images and run a simulation (Linux KVM host; or Apple Silicon Mac).
    #[command(after_help = format!("{EXIT_CODES}\n\nPreparation has a 15-minute deadline, separate from the simulation execution budget.\n\nUse diagnostics ID for retained worker errors after completion."))]
    Run {
        #[command(flatten)]
        compose: ComposeArg,
        /// Simulation seed [default: project seed, otherwise allocated once by the server].
        #[arg(long)]
        seed: Option<u64>,
        /// Pause before guest execution; use --detach to return immediately.
        #[arg(long)]
        paused: bool,
        /// Return after submission while the simulation continues on the server.
        #[arg(long)]
        detach: bool,
        /// Print exactly one simulation resource as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Compare deterministic results of two completed simulations.
    #[command(after_help = EXIT_CODES)]
    Compare {
        /// Baseline simulation ID.
        baseline: seedbox_api::SimulationId,
        /// Simulation ID to compare with the baseline.
        simulation: seedbox_api::SimulationId,
        /// Print the comparison as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Rerun a simulation using its retained inputs and original seed.
    Rerun {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Return after submission.
        #[arg(long)]
        detach: bool,
        /// Print exactly one simulation resource as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Start a new simulation from a captured checkpoint.
    Branch {
        /// Short source simulation ID.
        id: seedbox_api::SimulationId,
        /// Checkpoint request ID returned by checkpoint capture.
        checkpoint: String,
        /// Return after submission.
        #[arg(long)]
        detach: bool,
        /// Print exactly one simulation resource as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// List resources known to the server.
    Get {
        #[command(subcommand)]
        resource: CollectionArg,
    },
    /// Describe a simulation or checkpoint as structured text.
    Describe {
        #[command(subcommand)]
        resource: ResourceArg,
    },
    /// Request that a simulation stop.
    Stop {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Stop every simulation in the seed range started by this ID.
        #[arg(long)]
        all_seeds: bool,
    },
    /// Print retained structured events as newline-delimited JSON.
    Events {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Keep printing new events until the simulation completes or Ctrl-C.
        #[arg(long)]
        follow: bool,
    },
    /// Wait until a simulation meets a condition.
    #[command(after_help = WAIT_HELP)]
    Wait {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Condition to wait for (see below).
        #[arg(long = "for", value_name = "CONDITION", value_parser = observe::condition)]
        condition: observe::Condition,
        /// Give up after this much host time, such as 30s or 5m [default: unlimited].
        #[arg(long, value_parser = wall_time)]
        timeout: Option<u64>,
        /// Print exactly one result document as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open the interactive simulation console.
    Tui {
        /// Short simulation ID to open first [default: the simulation list].
        id: Option<seedbox_api::SimulationId>,
    },
    /// Inspect playback state or a retained control receipt.
    Playback {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Control request ID [default: inspect current playback state].
        #[arg(value_parser = playback_args::request_id)]
        request_id: Option<String>,
        /// Print JSON instead of YAML.
        #[arg(long)]
        json: bool,
    },
    /// Run consecutive seeds on the server using one frozen preparation.
    #[command(after_help = EXIT_CODES)]
    Explore {
        #[command(flatten)]
        compose: ComposeArg,
        /// First simulation seed [default: project seed, otherwise allocated by the server].
        #[arg(long)]
        seed: Option<u64>,
        /// Number of consecutive seeds to execute.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=4096))]
        count: u32,
        /// Pause before guest execution; use --detach to return immediately.
        #[arg(long)]
        paused: bool,
        /// Return after the server accepts the seed range.
        #[arg(long)]
        detach: bool,
        /// Print one seed-range status document as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Rerun a simulation through the server and compare its retained results.
    #[command(after_help = EXIT_CODES)]
    Replay {
        /// Completed simulation ID.
        id: seedbox_api::SimulationId,
        /// Print the comparison as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Delete a retained resource.
    Delete {
        #[command(subcommand)]
        resource: DeleteArg,
    },
    /// Inject a YAML or JSON fault action at a committed simulation boundary.
    Inject {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// Fault action file, or - to read standard input.
        action: PathBuf,
    },
    /// Heal a named fault handle in a simulation.
    Heal {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// Handle assigned by the original fault action.
        handle: String,
    },
    /// Pause a simulation at its next committed boundary.
    Pause(playback_args::Mutation),
    /// Resume a simulation at unlimited speed.
    Resume(playback_args::Mutation),
    /// Advance one committed round and remain paused.
    Step(playback_args::Mutation),
    /// Save a checkpoint at a committed simulation boundary.
    Checkpoint(playback_args::Mutation),
    /// Set simulated seconds per wall-clock second; 0 pauses and unlimited removes pacing.
    Rate {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// Playback rate: 0, a positive integer or fraction, or unlimited.
        rate: playback_args::Rate,
    },
    /// Export a completed resolved simulation and its inputs, checkpoints and evidence.
    Export {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Bundle file to create without overwriting [default: SIMULATION-ID.seedbox.tar].
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        #[command(flatten)]
        transfer: Transfer,
        /// Print exactly one export receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Import a bundle as a fresh completed simulation, without executing it.
    Import {
        /// Bundle file to import.
        bundle: PathBuf,
        #[command(flatten)]
        transfer: Transfer,
        /// Print exactly one simulation document as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show completed simulation raw diagnostics, distinct from timed application logs.
    Diagnostics {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Exact retained file name [default: list files and omissions].
        name: Option<String>,
        #[command(flatten)]
        transfer: Transfer,
        /// Print one metadata document, or one lossless hexadecimal file document.
        #[arg(long)]
        json: bool,
    },
    /// Print retained application output.
    Logs {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Machine name [default: all machines].
        machine: Option<String>,
        /// Keep printing new output until the simulation completes or Ctrl-C.
        #[arg(long)]
        follow: bool,
        /// Print only the last N lines (records with --json) of retained output.
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
        /// Only output committed at or after this simulated time, such as 12s [default: start].
        #[arg(long, value_name = "TIME", value_parser = sim_time)]
        since: Option<u64>,
        /// Only output committed before this simulated time, such as 20s [default: end].
        #[arg(long, value_name = "TIME", value_parser = sim_time)]
        until: Option<u64>,
        /// Only output from this machine incarnation; 0 is the first boot [default: all].
        #[arg(long, value_name = "N")]
        incarnation: Option<u32>,
        /// Select stdout or stderr [default: both].
        #[arg(long, value_enum)]
        stream: Option<ApplicationLogStream>,
        /// Print metadata and bytes as newline-delimited JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ApplicationLogStream {
    /// Application standard output.
    Stdout,
    /// Application standard error.
    Stderr,
}
impl From<ApplicationLogStream> for seedbox_api::LogStream {
    fn from(value: ApplicationLogStream) -> Self {
        match value {
            ApplicationLogStream::Stdout => Self::Stdout,
            ApplicationLogStream::Stderr => Self::Stderr,
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

/// Apple Silicon Mac runs the built-in HVF engine; every other host runs KVM.
const MAC: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

#[derive(Args)]
struct ComposeArg {
    /// Seedbox project file, or a directory containing seedbox.yaml.
    #[arg(default_value = ".", value_name = "PROJECT")]
    compose: PathBuf,
}

impl ComposeArg {
    fn load(&self) -> Result<config::Simulation> {
        let path = defaults::compose_file(&self.compose)?;
        let architecture = if MAC {
            seedbox_application::guest::Architecture::Arm64
        } else {
            seedbox_application::guest::Architecture::Amd64
        };
        config::load_for_architecture(&path, &std::env::vars().collect(), architecture)
    }
}

#[derive(Args)]
struct ExecutionArgs {
    /// Stop after this much active host time, excluding explicit pauses [default: unlimited].
    #[arg(long, value_parser = wall_time)]
    wall_time: Option<u64>,
}

fn wall_time(value: &str) -> std::result::Result<u64, String> {
    config::duration_ns(value).map_err(|error| error.to_string())
}

/// A simulated time such as 250ms, 12s or 1m.
fn sim_time(value: &str) -> std::result::Result<u64, String> {
    config::duration_ns(value).map_err(|error| error.to_string())
}

const WAIT_HELP: &str = "Conditions:
  completed                    the simulation has completed, for any reason
  executing                    guest execution has started
  paused                       playback is paused
  sim-time=DURATION            simulated time has reached DURATION, such as 30s
  machine/NAME=CONDITION       starting, ready, paused, crashed or exited
  fault/ID-OR-HANDLE=STATUS    unrequested, requested, applied, failed or unavailable

Exit status:
  0    the condition was met
  2    the simulation completed first, or a usage or setup error
  124  TIMEOUT before the condition was met";

#[derive(Args)]
struct StateArg {
    /// Directory holding the content store and recorded runs.
    #[arg(long, env = "SEEDBOX_ARTIFACT_STATE", default_value = ".seedbox")]
    state: PathBuf,
}

#[derive(Args)]
struct Transfer {
    /// Maximum complete transfer bytes, including archive headers and padding.
    #[arg(long, default_value_t = seedbox_api::BUNDLE_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=seedbox_api::BUNDLE_MAX_BYTES))]
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
    /// Guest kernel Image on Mac, or bzImage on Linux [default: embedded kernel, otherwise beside the executable].
    #[arg(long, env = "SEEDBOX_KERNEL")]
    kernel: Option<PathBuf>,
}

#[derive(Args)]
#[command(next_help_heading = "Runtime inputs")]
struct RuntimeInputs {
    #[command(flatten)]
    kvm: KvmInputs,
    /// Guest helper archive override [default: embedded by the complete build].
    #[arg(long, env = "SEEDBOX_RUNTIME_ASSETS")]
    runtime_assets: Option<PathBuf>,
    /// Static guest supervisor [default: embedded guest helper].
    #[arg(long, env = "SEEDBOX_GUEST_INIT")]
    guest_init: Option<PathBuf>,
    /// Static Linux mke2fs for named volumes [default: embedded guest helper].
    #[arg(long, env = "SEEDBOX_MKE2FS")]
    mke2fs: Option<PathBuf>,
    /// Also record and compare every machine's RAM and registers (also retains per-page hashes).
    #[arg(long)]
    full_state: bool,
}

impl RuntimeInputs {
    /// Resolve every input and build the matching preparer and executor.
    fn backends(
        &self,
        simulation: &config::Simulation,
        cancellation: &CancellationToken,
    ) -> Result<(Preparer, Executor)> {
        validate_machine(simulation)?;
        let machine = &simulation.machine;
        let engine = defaults::engine()?;
        for (flag, path) in [
            ("--kernel", &self.kvm.kernel),
            ("--runtime-assets", &self.runtime_assets),
            ("--guest-init", &self.guest_init),
            ("--mke2fs", &self.mke2fs),
        ] {
            if let Some(path) = path {
                defaults::existing(flag, path.clone())?;
            }
        }
        let witness = if self.full_state {
            WitnessMode::FullState
        } else {
            WitnessMode::Exits
        };
        let kernel = backends::guest_kernel(self.kvm.kernel.clone())?;
        let runtime = Some(backends::guest_assets(self.runtime_assets.clone())?);
        let resolve = |flag, explicit: &Option<PathBuf>, name| match explicit {
            Some(path) => defaults::existing(flag, path.clone()),
            None => runtime
                .as_ref()
                .map(|assets| assets.input(name))
                .ok_or_else(|| {
                    Error::Setup(format!(
                        "{flag}: missing guest helper; run `cargo xtask build-assets`"
                    ))
                }),
        };
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let executor = {
            Executor::Mac(seedbox_application::mac::MacExecutor::new(
                engine,
                &kernel,
                machine.memory_mb,
                witness,
                simulation.disk.torn_writes,
            )?)
        };
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let executor = {
            Executor::Linux(
                VmmExecutor::new(engine, machine.memory_mb, witness)?
                    .with_torn_writes(simulation.disk.torn_writes)
                    .with_page_timer(machine.page_timer)
                    .with_deferred_tx(machine.deferred_tx),
            )
        };
        let inputs = OciPreparer {
            docker: backends::container_engine()?,
            kernel,
            init: resolve("--guest-init", &self.guest_init, "compose-init")?,
            progress: resolve("--runtime-assets", &None, "progress.tar")?,
            mke2fs: Some(resolve("--mke2fs", &self.mke2fs, "mke2fs")?),
            profile: executor.profile(),
        };
        Ok((
            Preparer {
                inputs,
                _helpers: runtime,
                cancellation: cancellation.clone(),
            },
            executor,
        ))
    }
}

/// Reject world machine options the selected platform cannot execute, before any state.
fn validate_machine(simulation: &config::Simulation) -> Result<()> {
    if MAC && (simulation.machine.page_timer || simulation.machine.deferred_tx) {
        return Err(Error::Setup(
            "x-seedbox.machine page_timer and deferred_tx require Linux KVM; remove them on Mac"
                .into(),
        ));
    }
    Ok(())
}

pub fn main() -> ExitCode {
    let mut arguments: Vec<_> = std::env::args_os().collect();
    let first = arguments.get(1).and_then(|argument| argument.to_str());
    use clap::CommandFactory;
    let root = Cli::command();
    let explicit_command = first.is_some_and(|name| {
        root.find_subcommand(name).is_some()
            || matches!(name, "help" | "--help" | "-h" | "--version" | "-V")
    });
    if !explicit_command {
        arguments.insert(1, "run".into());
    }
    match run(Cli::parse_from(arguments)) {
        Ok(code) => ExitCode::from(code),
        // The reader stopped early, as `seedbox events ID | head` does; the
        // output it took was complete, so there is nothing to report.
        Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Completions { shell } => completions::script(shell),
        Command::Complete { words } => completions::complete(&words),
        Command::Shutdown { state } => daemon::shutdown(state),
        Command::Serve {
            state,
            inputs,
            startup_handshake,
        } => daemon::serve(state, inputs, startup_handshake),
        Command::Inject { mutation, action } => faults::inject(mutation, &action),
        Command::Heal { mutation, handle } => simulations::mutate_control(
            mutation,
            seedbox_api::ControlCommand::InjectFault {
                action: seedbox_api::FaultAction::Heal { handle },
            },
        ),
        Command::Pause(mutation) => simulations::mutate_control(
            mutation,
            seedbox_api::ControlCommand::SetRate {
                rate: seedbox_api::PlaybackRate::Paused,
            },
        ),
        Command::Resume(mutation) => simulations::mutate_control(
            mutation,
            seedbox_api::ControlCommand::SetRate {
                rate: seedbox_api::PlaybackRate::Unlimited,
            },
        ),
        Command::Step(mutation) => {
            simulations::mutate_control(mutation, seedbox_api::ControlCommand::Step)
        }
        Command::Checkpoint(mutation) => {
            simulations::mutate_control(mutation, seedbox_api::ControlCommand::Checkpoint)
        }
        Command::Rate { mutation, rate } => simulations::mutate_control(
            mutation,
            seedbox_api::ControlCommand::SetRate { rate: rate.0 },
        ),
        Command::Init {
            directory,
            template,
        } => init(&directory, template),
        Command::Validate { compose, json } => validate(&compose, json),
        Command::Doctor { inputs, json } => doctor(inputs, json),
        Command::Prepare {
            compose,
            output,
            state,
            inputs,
        } => prepare(&compose, &output, &state, &inputs),
        Command::Run {
            compose,
            seed,
            paused,
            detach,
            json,
            execution,
        } => simulations::run(&compose, seed, paused, detach, json, execution.wall_time),
        Command::Branch {
            id,
            checkpoint,
            detach,
            json,
            execution,
        } => simulations::branch(&id, checkpoint, detach, json, execution.wall_time),
        Command::Rerun {
            id,
            detach,
            json,
            execution,
        } => simulations::rerun(&id, detach, json, execution.wall_time),
        Command::Compare {
            baseline,
            simulation,
            json,
        } => simulations::compare(&baseline, &simulation, json),
        Command::Get { resource } => match resource {
            CollectionArg::Simulation { id, watch, json } => {
                if watch {
                    simulations::watch(&id, json)
                } else {
                    simulations::get(&id, json)
                }
            }
            CollectionArg::Simulations { source, json } => match source {
                Some(id) => simulations::seed_range(&id, json),
                None => simulations::list(json),
            },
            CollectionArg::Capacity { json } => simulations::capacity(json),
            CollectionArg::Checkpoints { id, json } => simulations::checkpoints(&id, None, json),
            CollectionArg::World { id, json } => observe::world(&id, json),
            CollectionArg::Timeline { id, json } => observe::timeline(&id, json),
            CollectionArg::Properties { id, json } => observe::properties(&id, json),
        },
        Command::Describe { resource } => match resource {
            ResourceArg::Simulation { id, json } => simulations::describe(&id, json),
            ResourceArg::Checkpoint { id, name, json } => {
                simulations::checkpoints(&id, Some(&name), json)
            }
        },
        Command::Stop { id, all_seeds } => {
            if all_seeds {
                simulations::stop_seeds(&id)
            } else {
                simulations::stop(&id)
            }
        }
        Command::Events { id, follow } => observe::events(&id, follow),
        Command::Wait {
            id,
            condition,
            timeout,
            json,
        } => observe::wait(&id, &condition, timeout, json),
        Command::Tui { id } => simulations::tui(id),
        Command::Playback {
            id,
            request_id,
            json,
        } => simulations::playback(&id, request_id.as_deref(), json),
        Command::Explore {
            compose,
            seed,
            count,
            paused,
            detach,
            json,
            execution,
        } => simulations::explore(
            &compose,
            seed,
            count,
            paused,
            detach,
            json,
            execution.wall_time,
        ),
        Command::Delete {
            resource: DeleteArg::Simulation { id },
        } => simulations::delete(&id),
        Command::Delete {
            resource: DeleteArg::Checkpoint { id, name },
        } => simulations::delete_checkpoint(&id, &name),
        Command::Replay {
            id,
            json,
            execution,
        } => simulations::replay(&id, json, execution.wall_time),
        Command::Export {
            id,
            output,
            transfer,
            json,
        } => bundles::export(&id, output, transfer.max_bytes, json),
        Command::Import {
            bundle,
            transfer,
            json,
        } => bundles::import(&bundle, transfer.max_bytes, json),
        Command::Diagnostics {
            id,
            name,
            transfer,
            json,
        } => diagnostics::diagnostics(&id, name.as_deref(), transfer.max_bytes, json),
        Command::Logs {
            id,
            machine,
            follow,
            tail,
            since,
            until,
            incarnation,
            stream,
            json,
        } => observe::logs(
            &id,
            observe::LogRequest {
                machine: machine.as_deref(),
                filter: observe::LogFilter {
                    stream: stream.map(Into::into),
                    since_ns: since,
                    until_ns: until,
                    incarnation,
                },
                follow,
                tail,
                json,
            },
        ),
    }
}

fn init(directory: &Path, template: TemplateArg) -> Result<u8> {
    if directory.exists() {
        return Err(Error::Setup(format!(
            "{}: already exists; choose a new directory",
            directory.display()
        )));
    }
    seedbox_application::templates::init(directory, template.into())?;
    output::status("CREATED", directory.display());
    Ok(0)
}

fn validate(compose: &ComposeArg, json: bool) -> Result<u8> {
    let simulation = compose.load()?;
    if json {
        output::json(&simulation)?;
    } else {
        output::status(
            "VALID",
            format_args!(
                "{} ({} services; image compatibility is checked by `prepare`)",
                simulation.name,
                simulation.services.len()
            ),
        );
    }
    Ok(0)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn doctor(inputs: KvmInputs, json: bool) -> Result<u8> {
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let engine = defaults::engine()?;
    let kernel = match inputs.kernel {
        Some(path) => defaults::existing("--kernel", path)?,
        None => backends::guest_kernel(None)?,
    };
    if cancellation.is_cancelled() {
        return Err(Error::Setup("doctor cancelled".into()));
    }
    let report = seedbox_application::mac::MacExecutor::doctor(&engine, &kernel)?;
    if json {
        output::json(&report)?;
    } else {
        output::status("VALID", "compatible execution profile");
        doctor_engine(&report.engine);
        output::field("kernel", report.kernel.hex());
    }
    Ok(0)
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn doctor(inputs: KvmInputs, json: bool) -> Result<u8> {
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let engine = defaults::engine()?;
    let kernel = backends::guest_kernel(inputs.kernel)?;
    let report = VmmExecutor::doctor(engine, &kernel)?;
    if json {
        output::json(&report)?;
    } else {
        output::status("VALID", "compatible execution profile");
        doctor_engine(&report.engine);
        output::field("kernel", report.kernel.hex());
    }
    Ok(0)
}

fn doctor_engine(engine: &seedbox_application::execution::EngineDescriptor) {
    output::field("simulator", &engine.simulator_version);
    output::field("revision", engine.revision.as_deref().unwrap_or("unknown"));
}

fn prepare(
    compose: &ComposeArg,
    output: &Path,
    state: &StateArg,
    inputs: &RuntimeInputs,
) -> Result<u8> {
    if output.exists() {
        return Err(Error::Setup(format!(
            "--output {}: already exists",
            output.display()
        )));
    }
    let simulation = compose.load()?;
    validate_machine(&simulation)?;
    let (store, _) = state.open()?;
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let (preparer, _) = inputs.backends(&simulation, &cancellation)?;
    if cancellation.is_cancelled() {
        return Err(Error::PreparationCancelled);
    }
    let prepared = preparer.prepare(&simulation, &store)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(output)?;
    serde_json::to_writer_pretty(file, &prepared)?;
    output::status("PREPARED", output.display());
    Ok(0)
}

#[derive(Subcommand)]
enum DeleteArg {
    /// Stop and delete a simulation, its descendants and their private files.
    Simulation {
        /// Root simulation ID.
        id: seedbox_api::SimulationId,
    },

    /// Delete an unreferenced checkpoint from a completed simulation.
    Checkpoint {
        /// Source simulation ID.
        id: seedbox_api::SimulationId,
        /// Checkpoint request ID.
        name: String,
    },
}

#[derive(Subcommand)]
enum CollectionArg {
    /// Show resident execution slots, paused occupants and queue reasons.
    Capacity {
        /// Print JSON instead of tables.
        #[arg(long)]
        json: bool,
    },
    /// Show one simulation and optionally watch its status.
    Simulation {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Stream status until completion or Ctrl-C; never stop the simulation.
        #[arg(long)]
        watch: bool,
        /// Print JSON; with --watch, emit newline-delimited documents.
        #[arg(long)]
        json: bool,
    },
    /// List simulations retained by the server.
    Simulations {
        /// Source simulation ID to restrict the result to its seed range.
        source: Option<seedbox_api::SimulationId>,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List checkpoint captures belonging to a simulation.
    Checkpoints {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show a simulation's machines, links and faults as observed now.
    World {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Print the world document as JSON instead of tables.
        #[arg(long)]
        json: bool,
    },
    /// List a simulation's guest properties and how each was judged.
    Properties {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Print the property assessment as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// List a simulation's recorded and scheduled faults and what became of each.
    Timeline {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Print the timeline document as JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ResourceArg {
    /// Describe one run of one seed as resubmittable YAML.
    Simulation {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Print JSON instead of YAML.
        #[arg(long)]
        json: bool,
    },
    /// Describe a checkpoint capture and its timeline position.
    Checkpoint {
        /// Short simulation ID.
        id: seedbox_api::SimulationId,
        /// Checkpoint request ID.
        #[arg(value_parser = playback_args::request_id)]
        name: String,
        /// Print JSON instead of YAML.
        #[arg(long)]
        json: bool,
    },
}
