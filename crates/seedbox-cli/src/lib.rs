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
mod server_args;
mod signals;
use server_args::ServerArgs;
mod simulations;
use backends::{Executor, Preparer};

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox_application::{
    config,
    execution::{CancellationToken, ExecutionBackend, WitnessMode},
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
    name = "seedbox",
    version = seedbox_application::engine::SIMULATOR_VERSION,
    override_usage = "seedbox <COMMAND>\n       seedbox [RUN OPTIONS] [PROJECT | CHECKPOINT]",
    about = "Repeatable simulations of real Linux applications.",
    after_help = "Examples:\n  seedbox init my-test\n  seedbox run my-test\n  seedbox logs SIMULATION --follow\n  seedbox replay SIMULATION\n\n  seedbox checkpoint SIMULATION -o before-bug.seedbox\n  seedbox run before-bug.seedbox\n\nRun is the default command. With no arguments, run the project in the current directory.\nA checkpoint contains the inputs and state needed to start a new simulation without the original server.\nUse `seedbox help COMMAND` for command options."
)]
struct Cli {
    #[command(flatten)]
    server: ServerArgs,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create an example project.
    Init {
        /// Directory to create.
        directory: PathBuf,
        /// Example application to start from.
        #[arg(long, value_enum, default_value_t = TemplateArg::Etcd)]
        template: TemplateArg,
    },
    /// Validate a project and check host readiness.
    Check {
        #[command(flatten)]
        project: ProjectArg,
        /// Print project configuration and host observations as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Start a new simulation from a project or checkpoint.
    #[command(after_help = EXIT_CODES)]
    Run {
        /// Project file, project directory, or self-contained checkpoint [default: current directory].
        #[arg(value_name = "PROJECT|CHECKPOINT", conflicts_with = "from")]
        input: Option<PathBuf>,
        /// Repeat the retained inputs of an existing simulation.
        #[arg(long, conflicts_with_all = ["input", "seed", "count"])]
        from: Option<seedbox_api::SimulationId>,
        #[command(flatten)]
        backend: BackendArgs,
        /// Simulation seed [default: project seed, otherwise allocated by the server].
        #[arg(long)]
        seed: Option<u64>,
        /// Run this many consecutive seeds using one frozen preparation [default: 1].
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..=4096))]
        count: Option<u32>,
        /// Pause before guest execution; use --detach to return immediately.
        #[arg(long)]
        paused: bool,
        /// Return after submission while execution continues on the server.
        #[arg(long)]
        detach: bool,
        /// Print one simulation document, or a seed-range document with --count.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Repeat a simulation and verify matching results.
    #[command(after_help = EXIT_CODES)]
    Replay {
        /// Completed simulation ID.
        id: seedbox_api::SimulationId,
        #[command(flatten)]
        backend: BackendArgs,
        /// Print the comparison as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        execution: ExecutionArgs,
    },
    /// Compare two completed simulations.
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
    /// List simulations or inspect one.
    Show {
        /// Simulation ID [default: list simulations].
        id: Option<seedbox_api::SimulationId>,
        /// Information to display [default: status].
        #[arg(long, value_enum, requires = "id")]
        view: Option<ShowView>,
        /// Stream status until completion or Ctrl-C.
        #[arg(long, requires = "id", conflicts_with_all = ["view", "file", "all_seeds"])]
        watch: bool,
        /// Show the seed range containing this simulation.
        #[arg(long, requires = "id", conflicts_with_all = ["view", "file", "watch"])]
        all_seeds: bool,
        /// Read this exact raw diagnostic file; requires --view diagnostics.
        #[arg(long, requires = "id")]
        file: Option<String>,
        #[command(flatten)]
        transfer: Transfer,
        /// Print JSON instead of text; --watch emits newline-delimited documents.
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
    /// Read simulator events as newline-delimited JSON.
    Events {
        /// Simulation ID.
        id: seedbox_api::SimulationId,
        /// Stream events until completion or Ctrl-C.
        #[arg(long)]
        follow: bool,
    },
    /// Wait for a simulation condition.
    #[command(after_help = WAIT_HELP)]
    Wait {
        /// Simulation ID.
        id: seedbox_api::SimulationId,
        /// Condition to wait for (see below).
        #[arg(long = "for", value_name = "CONDITION", value_parser = observe::condition)]
        condition: observe::Condition,
        /// Give up after this much host time, such as 30s or 5m [default: unlimited].
        #[arg(long, value_parser = wall_time)]
        timeout: Option<u64>,
        /// Print one result document as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open the interactive console.
    Tui {
        /// Simulation ID to open [default: simulation list].
        id: Option<seedbox_api::SimulationId>,
    },
    /// Stop a simulation and retain its results.
    Stop {
        /// Simulation ID.
        id: seedbox_api::SimulationId,
        /// Stop every simulation in this seed range.
        #[arg(long)]
        all_seeds: bool,
    },
    /// Delete a simulation and its resources.
    Delete {
        /// Simulation ID; active execution is stopped before deletion.
        id: seedbox_api::SimulationId,
    },
    /// Pause, resume, step, change speed, or inject faults.
    Control {
        #[command(subcommand)]
        command: ControlCommand,
    },
    /// Save a self-contained checkpoint to a file.
    #[command(
        after_help = "Capture requires application-ready machines. The command pauses execution for capture and restores the previous playback state.\nStart a new simulation with `seedbox run FILE`; add --paused --detach to inspect the captured state first."
    )]
    Checkpoint {
        /// Simulation ID to capture.
        id: seedbox_api::SimulationId,
        /// Checkpoint file to create without overwriting [default: SIMULATION.seedbox].
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
        /// Print one checkpoint receipt as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Start, inspect, or stop the local server.
    Server {
        #[command(subcommand)]
        command: ServerCommand,
    },
    /// Generate shell integration.
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
}

#[derive(Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum ShowView {
    /// Current execution status.
    #[default]
    Status,
    /// Resubmittable simulation definition as YAML.
    Definition,
    /// Current machines, links and faults.
    World,
    /// Scheduled and recorded fault inputs.
    Timeline,
    /// Current playback state.
    Playback,
    /// Retained raw worker diagnostics and omissions.
    Diagnostics,
}

#[derive(Subcommand)]
enum ControlCommand {
    /// Pause at the next committed boundary.
    Pause(playback_args::Mutation),
    /// Resume at unlimited speed.
    Resume(playback_args::Mutation),
    /// Advance one committed round and remain paused.
    Step(playback_args::Mutation),
    /// Set simulated seconds per wall-clock second.
    Rate {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// Playback rate: 0, a positive integer or fraction, or unlimited.
        rate: playback_args::Rate,
    },
    /// Inject a fault at a committed simulation boundary.
    Inject {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// YAML or JSON fault action file, or - for standard input.
        action: PathBuf,
    },
    /// Heal a named fault.
    Heal {
        #[command(flatten)]
        mutation: playback_args::Mutation,
        /// Handle assigned by the original fault action.
        handle: String,
    },
    /// Inspect whether a control request was applied.
    Receipt {
        /// Simulation ID.
        id: seedbox_api::SimulationId,
        /// Control request ID.
        #[arg(value_parser = playback_args::request_id)]
        request: String,
        /// Print JSON instead of YAML.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Start the local server.
    Start {
        /// Keep the server in the foreground for a service manager.
        #[arg(long)]
        foreground: bool,
        /// Report readiness to the launching client.
        #[arg(long, hide = true)]
        startup_handshake: bool,
    },
    /// Show server availability, occupied capacity and queued work.
    Status {
        /// Print one server status document as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Gracefully stop the server and its active simulations.
    Stop,
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

/// TCG runs only on the normal Driver runtime; the host list comes from the
/// same mapping that selects Driver guests.
fn tcg_refusal() -> Error {
    Error::Setup(format!(
        "--backend tcg requires {}, and the normal Driver runtime",
        seedbox_driver_execution::driver_world::platform::hosts_for(
            seedbox_spec::backend::CpuBackend::Tcg
        )
    ))
}

/// Guest-ISA discovery only; execution always uses Driver.
const MAC: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));

#[derive(Args)]
struct ProjectArg {
    /// Seedbox project file, or a directory containing seedbox.yaml.
    #[arg(default_value = ".", value_name = "PROJECT")]
    project: PathBuf,
}

impl ProjectArg {
    fn load(&self) -> Result<config::Simulation> {
        let path = defaults::project_file(&self.project)?;
        let architecture = if MAC {
            seedbox_application::guest::Architecture::Arm64
        } else {
            seedbox_application::guest::Architecture::Amd64
        };
        config::load_for_architecture(&path, &std::env::vars().collect(), architecture)
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendArg {
    /// Use native hardware-accelerated execution.
    Pv,
    /// Emulate the x86-64 CPU in software without KVM.
    Tcg,
}
#[derive(Args)]
struct BackendArgs {
    /// CPU backend [default: pv for projects, recorded backend for retained inputs].
    #[arg(long, value_enum)]
    backend: Option<BackendArg>,
}
struct BackendSelection {
    backend: Option<seedbox_spec::backend::CpuBackend>,
    wall_time: Option<u64>,
}
impl BackendArgs {
    fn selection(self, execution: ExecutionArgs) -> BackendSelection {
        BackendSelection {
            backend: self.backend.map(|backend| match backend {
                BackendArg::Pv => seedbox_spec::backend::CpuBackend::Pv,
                BackendArg::Tcg => seedbox_spec::backend::CpuBackend::Tcg,
            }),
            wall_time: execution.wall_time,
        }
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
struct Transfer {
    /// Maximum complete transfer bytes, including archive headers and padding.
    #[arg(long, default_value_t = seedbox_api::BUNDLE_MAX_BYTES, value_parser = clap::value_parser!(u64).range(1..=seedbox_api::BUNDLE_MAX_BYTES))]
    max_bytes: u64,
}

#[derive(Args, Clone, Copy, Default)]
#[command(next_help_heading = "Runtime inputs")]
struct RuntimeInputs {
    /// Also record and compare every machine's RAM and registers (also retains per-page hashes).
    #[arg(long)]
    full_state: bool,
}

impl RuntimeInputs {
    // Only Driver hosts (Linux x86-64 or macOS aarch64) select it.
    fn use_driver(&self) -> bool {
        seedbox_driver_execution::driver_world::platform::Host::current().runs_driver()
    }

    /// Resolve every input and build the matching preparer and executor.
    fn backends(
        &self,
        _store: &ContentStore,
        simulation: &config::Simulation,
        cpu_backend: seedbox_spec::backend::CpuBackend,
        cancellation: &CancellationToken,
        scratch: &Path,
    ) -> Result<(Preparer, Executor)> {
        let engine = defaults::engine()?;
        if !self.use_driver() {
            return Err(tcg_refusal());
        }
        let witness = if self.full_state {
            WitnessMode::FullState
        } else {
            WitnessMode::Exits
        };
        let executor = seedbox_driver_execution::driver_world::DriverExecutor::for_preparation(
            simulation,
            witness,
            cpu_backend,
        )?;
        let architecture = seedbox_driver_execution::driver_world::platform::Host::current()
            .guest(cpu_backend)
            .ok_or_else(|| {
                Error::Setup("the Driver runtime requires Linux x86-64 or macOS aarch64".into())
            })?;
        let runtime = backends::driver_runtime(None, scratch)?;
        let selected = runtime.select(cpu_backend, architecture)?;
        let inputs = OciPreparer {
            docker: backends::container_engine()?,
            kernel: selected.guest_assets.join("kernel"),
            engine: engine.to_owned(),
            init: selected.guest_assets.join("init"),
            progress: selected.guest_assets.join("progress.tar"),
            mke2fs: Some(selected.guest_assets.join("mke2fs")),
            profile: executor.profile(),
        };
        Ok((
            Preparer {
                inputs,
                cancellation: cancellation.clone(),
                driver_inputs: selected.clone(),
            },
            Executor(Box::new(executor.with_runtime(selected)?)),
        ))
    }
}

/// Run the public command line using process arguments, environment and stdio.
/// The executable must dispatch its private engine worker before calling this.
/// Clap retains process termination for help, version and argument errors; other
/// commands return their existing exit status, with broken pipes treated as success.
pub fn main() -> ExitCode {
    let mut arguments: Vec<_> = std::env::args_os().collect();
    let explicit_command = has_explicit_command(&arguments);
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

// Recognize root options before an explicit command without confusing their
// values with project paths. Run-specific options still select implicit run.
fn has_explicit_command(arguments: &[std::ffi::OsString]) -> bool {
    use clap::CommandFactory;
    let mut root = Cli::command();
    root.build();
    let mut words = arguments.iter().skip(1);
    while let Some(word) = words.next() {
        let Some(word) = word.to_str() else {
            return false;
        };
        if matches!(word, "help" | "--help" | "-h" | "--version" | "-V") {
            return true;
        }
        if !word.starts_with('-') {
            return root.find_subcommand(word).is_some();
        }
        let (name, inline) = word
            .split_once('=')
            .map_or((word, false), |(name, _)| (name, true));
        let Some(arg) = root.get_arguments().find(|arg| {
            arg.get_long()
                .is_some_and(|long| name.strip_prefix("--") == Some(long))
        }) else {
            return false;
        };
        if !inline && arg.get_num_args().is_some_and(|count| count.takes_values()) {
            words.next();
        }
    }
    false
}

fn run(cli: Cli) -> Result<u8> {
    let server = &cli.server;
    match cli.command {
        Command::Init {
            directory,
            template,
        } => init(&directory, template),
        Command::Check { project, json } => check(&project, json),
        Command::Run {
            input,
            from,
            backend,
            seed,
            count,
            paused,
            detach,
            json,
            execution,
        } => {
            let selection = backend.selection(execution);
            if let Some(id) = from {
                return simulations::run_from(server, &id, paused, detach, json, selection);
            }
            let input = input.unwrap_or_else(|| PathBuf::from("."));
            if bundles::is_checkpoint(&input)? {
                if seed.is_some() || count.is_some() {
                    return Err(Error::Setup(
                        "--seed and --count require a project, not a checkpoint".into(),
                    ));
                }
                return simulations::run_checkpoint(
                    server, &input, paused, detach, json, selection,
                );
            }
            let project = ProjectArg { project: input };
            match count {
                Some(count) => simulations::run_count(
                    server, &project, seed, count, paused, detach, json, selection,
                ),
                None => simulations::run(server, &project, seed, paused, detach, json, selection),
            }
        }
        Command::Replay {
            id,
            backend,
            json,
            execution,
        } => simulations::replay(server, &id, json, backend.selection(execution)),
        Command::Compare {
            baseline,
            simulation,
            json,
        } => simulations::compare(server, &baseline, &simulation, json),
        Command::Show {
            id,
            view,
            watch,
            all_seeds,
            file,
            transfer,
            json,
        } => {
            let view = view.unwrap_or_default();
            if file.is_some() && view != ShowView::Diagnostics {
                return Err(Error::Setup("--file requires --view diagnostics".into()));
            }
            let Some(id) = id else {
                return simulations::list(server, json);
            };
            if watch {
                return simulations::watch(server, &id, json);
            }
            if all_seeds {
                return simulations::seed_range(server, &id, json);
            }
            match view {
                ShowView::Status => simulations::show_status(server, &id, json),
                ShowView::Definition => simulations::show_definition(server, &id, json),
                ShowView::World => observe::world(server, &id, json),
                ShowView::Timeline => observe::timeline(server, &id, json),
                ShowView::Playback => simulations::playback(server, &id, None, json),
                ShowView::Diagnostics => {
                    diagnostics::diagnostics(server, &id, file.as_deref(), transfer.max_bytes, json)
                }
            }
        }
        Command::Events { id, follow } => observe::events(server, &id, follow),
        Command::Wait {
            id,
            condition,
            timeout,
            json,
        } => observe::wait(server, &id, &condition, timeout, json),
        Command::Tui { id } => simulations::tui(server, id),
        Command::Stop { id, all_seeds } => {
            if all_seeds {
                simulations::stop_seeds(server, &id)
            } else {
                simulations::stop(server, &id)
            }
        }
        Command::Delete { id } => simulations::delete(server, &id),
        Command::Control { command } => control(server, command),
        Command::Checkpoint { id, output, json } => bundles::checkpoint(
            server,
            &id,
            output.unwrap_or_else(|| PathBuf::from(format!("{id}.seedbox"))),
            json,
        ),
        Command::Server { command } => match command {
            ServerCommand::Start {
                foreground,
                startup_handshake,
            } => daemon::start(server, foreground, startup_handshake),
            ServerCommand::Status { json } => daemon::status(server, json),
            ServerCommand::Stop => daemon::shutdown(server.state.clone()),
        },
        Command::Completions { shell } => completions::script(shell),
        Command::Complete { words } => completions::complete(&words),
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
            server,
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

fn control(server: &ServerArgs, command: ControlCommand) -> Result<u8> {
    use seedbox_api::{ControlCommand as Action, PlaybackRate};
    let (mutation, action) = match command {
        ControlCommand::Inject { mutation, action } => {
            return faults::inject(server, mutation, &action)
        }
        ControlCommand::Receipt { id, request, json } => {
            return simulations::playback(server, &id, Some(&request), json)
        }
        ControlCommand::Pause(mutation) => (
            mutation,
            Action::SetRate {
                rate: PlaybackRate::Paused,
            },
        ),
        ControlCommand::Resume(mutation) => (
            mutation,
            Action::SetRate {
                rate: PlaybackRate::Unlimited,
            },
        ),
        ControlCommand::Step(mutation) => (mutation, Action::Step),
        ControlCommand::Rate { mutation, rate } => (mutation, Action::SetRate { rate: rate.0 }),
        ControlCommand::Heal { mutation, handle } => (
            mutation,
            Action::InjectFault {
                action: seedbox_api::FaultAction::Heal { handle },
            },
        ),
    };
    simulations::mutate_control(server, mutation, action)
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

fn check(project: &ProjectArg, json: bool) -> Result<u8> {
    let simulation = project.load()?;
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let engine = defaults::engine()?;
    let scratch = tempfile::tempdir()
        .map_err(|error| Error::Setup(format!("check runtime scratch: {error}")))?;
    let (kernel, _runtime) = backends::doctor_kernel(scratch.path())?;
    if cancellation.is_cancelled() {
        return Err(Error::Setup("check cancelled".into()));
    }
    let report = seedbox_driver_execution::driver_world::doctor(&engine, &kernel)?;
    if json {
        output::json(&serde_json::json!({ "project": simulation, "host": report }))?;
    } else {
        output::status("VALID", &simulation.name);
        output::field("project", "valid configuration; images checked during run");
        output::field("host", "compatible execution profile");
        output::field("simulator", &report.simulator_version);
        output::field("engine", report.engine.hex());
        output::field("kernel", report.kernel.hex());
    }
    Ok(0)
}

#[cfg(test)]
mod parser_tests {
    use super::*;

    #[test]
    fn root_options_do_not_hide_explicit_commands_or_turn_paths_into_commands() {
        for (words, expected) in [
            (vec!["seedbox", "--state", "run", "show"], true),
            (
                vec![
                    "seedbox",
                    "--state=/tmp/state",
                    "--full-state",
                    "server",
                    "start",
                ],
                true,
            ),
            (vec!["seedbox", "--full-state", "example.yaml"], false),
            (vec!["seedbox", "--state", "/tmp/state"], false),
            (vec!["seedbox", "--seed", "3"], false),
            (vec!["seedbox", "--state", "/tmp/state", "--help"], true),
        ] {
            let words = words
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>();
            assert_eq!(has_explicit_command(&words), expected, "{words:?}");
        }
    }
}
