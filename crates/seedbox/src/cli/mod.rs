//! The `seedbox` command line. Style rules for changes: `AGENTS.md`, "CLI".

mod backends;
mod defaults;
mod diagnostics;
mod output;
mod signals;
use backends::{Executor, Preparer};

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox::{
    bundle, config, diff,
    execution::{Backend, CancellationToken, ExecutionBackend, PreparationBackend, WitnessMode},
    exploration::{self, ExploreOptions},
    logs,
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
    version = seedbox::engine::SIMULATOR_VERSION,
    override_usage = "seedbox [RUN OPTIONS] [COMPOSE]\n       seedbox <COMMAND>",
    after_help = "Run is the default: seedbox --kernel PATH compose.yaml. See `seedbox help run` for run options.",
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
        after_help = "Success means profile compatibility, not cross-host replay qualification. Doctor never builds or downloads missing runtime inputs; run `python3 tools/build.py` or pass --kernel."
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
    },
    /// Prepare images and run a simulation (Linux KVM host; or Apple Silicon Mac).
    #[command(after_help = format!("{EXIT_CODES}\n\n{VMM_FAILURE_HELP}"))]
    Run {
        #[command(flatten)]
        compose: ComposeArg,
        /// Simulation seed [default: x-seedbox.seed from the Compose file].
        #[arg(long)]
        seed: Option<u64>,
        /// Capture the world before this simulated nanosecond and retain a branch with ordinary logs and diff evidence.
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
        performance: Performance,
    },
    /// Explore a finite seed sequence with one frozen preparation (Linux KVM host; or Apple Silicon Mac).
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
        /// Maximum active attempts. Omit --cpu for Linux parallel execution.
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
        performance: Performance,
    },
    /// Re-execute a recorded run from its retained inputs and compare (Linux KVM host; or Apple Silicon Mac).
    #[command(after_help = format!("{EXIT_CODES}\n\n{VMM_FAILURE_HELP}"))]
    Replay {
        /// Run directory, as printed by `run`.
        run: PathBuf,
        #[command(flatten)]
        cpu: CpuArg,
        #[command(flatten)]
        state: StateArg,
        /// Replay with this VMM instead of the recorded one.
        #[arg(long)]
        vmm: Option<PathBuf>,
        // Last: its help heading applies to every flag declared after it.
        #[command(flatten)]
        performance: Performance,
    },
    /// Pause host execution at a slice boundary (same execution user or root).
    #[command(after_help = HOST_CONTROL_HELP)]
    Pause {
        /// Directory of the executing run or replay attempt.
        run: PathBuf,
    },
    /// Resume host execution from a paused slice boundary (same execution user or root).
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

/// Apple Silicon Mac runs the built-in HVF engine; every other host runs KVM.
const MAC: bool = cfg!(all(target_os = "macos", target_arch = "aarch64"));
const CHECKPOINT_REQUIRES_KVM: &str =
    "--checkpoint-at-ns and --branch-seed require Linux KVM; omit them on Mac";
const CPU_REQUIRES_KVM: &str = "--cpu requires Linux KVM; omit it on Mac";
const VMM_REQUIRES_KVM: &str = "--vmm requires Linux KVM; use the built-in Mac engine";

#[derive(Args)]
struct ComposeArg {
    /// Compose file, or a directory containing compose.yaml.
    #[arg(default_value = ".")]
    compose: PathBuf,
}

impl ComposeArg {
    fn load(&self) -> Result<config::Simulation> {
        let path = defaults::compose_file(&self.compose)?;
        let architecture = if MAC {
            seedbox::guest::Architecture::Arm64
        } else {
            seedbox::guest::Architecture::Amd64
        };
        config::load_for_architecture(&path, &std::env::vars().collect(), architecture)
    }
}

#[derive(Args)]
struct CpuArg {
    /// Host CPU to pin the VMM to [default: first free core other than CPU 0's].
    #[arg(long, env = "SEEDBOX_CPU", value_parser = clap::value_parser!(u32).range(0..1024))]
    cpu: Option<u32>,
}

impl CpuArg {
    /// `--cpu` pins a KVM core; the Mac engine selects no core.
    fn reject_on_mac(&self) -> Result<()> {
        if MAC && self.cpu.is_some() {
            return Err(Error::Setup(CPU_REQUIRES_KVM.into()));
        }
        Ok(())
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

impl Transfer {
    fn options(&self, cancellation: CancellationToken) -> bundle::Options {
        bundle::Options {
            max_bytes: self.max_bytes,
            cancellation,
        }
    }
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
    /// Guest kernel Image on Mac, or bzImage on Linux [default: kernel beside the executable].
    #[arg(long, env = "SEEDBOX_KERNEL")]
    kernel: Option<PathBuf>,
    /// Override with a legacy standalone VMM [default: the engine built into seedbox].
    #[arg(long, env = "SEEDBOX_VMM")]
    vmm: Option<PathBuf>,
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
    fn validate_platform(&self) -> Result<()> {
        if MAC && self.kvm.vmm.is_some() {
            return Err(Error::Setup(VMM_REQUIRES_KVM.into()));
        }
        Ok(())
    }

    /// Resolve every input and build the matching preparer and executor.
    fn backends(
        &self,
        store: &ContentStore,
        cpu: Option<u32>,
        simulation: &config::Simulation,
        cancellation: &CancellationToken,
    ) -> Result<(Preparer, Executor)> {
        self.validate_platform()?;
        validate_machine(simulation)?;
        let machine = &simulation.machine;
        let engine = defaults::engine(self.kvm.vmm.clone())?;
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
                        "{flag}: missing guest helper; run `python3 tools/build.py`"
                    ))
                }),
        };
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        let executor = {
            if cpu.is_some() {
                return Err(Error::Setup(CPU_REQUIRES_KVM.into()));
            }
            Executor::Mac(seedbox::mac::MacExecutor::new(
                &engine,
                &kernel,
                store,
                machine.memory_mb,
                witness,
                simulation.disk.torn_writes,
            )?)
        };
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        let executor = {
            let executor = if self.kvm.vmm.is_some() {
                VmmExecutor::new(&engine, store, cpu, machine.memory_mb, witness)?
                    .with_detected_contract(store, &engine)?
            } else {
                VmmExecutor::new_internal(&engine, store, cpu, machine.memory_mb, witness)?
            };
            Executor::Linux(
                executor
                    .with_torn_writes(simulation.disk.torn_writes)
                    .with_page_timer(machine.page_timer)
                    .with_deferred_tx(machine.deferred_tx),
            )
        };
        let inputs = OciPreparer {
            docker: backends::container_engine()?,
            kernel,
            engine,
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

    /// Backends for a fresh `run` or `explore`, admitted on this host before any preparation.
    fn admitted_backends(
        &self,
        store: &ContentStore,
        cpu: &CpuArg,
        simulation: &config::Simulation,
        cancellation: &CancellationToken,
        checkpoint: Option<(u64, u64)>,
        performance: &Performance,
    ) -> Result<(Preparer, Executor)> {
        cpu.reject_on_mac()?;
        let (mut preparer, executor) = self.backends(store, cpu.cpu, simulation, cancellation)?;
        if cancellation.is_cancelled() {
            return Err(Error::PreparationCancelled);
        }
        let executor = match checkpoint {
            Some((at, branch)) => executor.with_checkpoint(at, branch)?,
            None => executor,
        };
        preparer.profile = executor.profile();
        Ok((preparer, performance.apply(executor)?))
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

/// Host-side execution settings: they change speed, never results.
#[derive(Args)]
#[command(next_help_heading = "Performance (never changes results)")]
struct Performance {
    /// Watchdog interval in milliseconds; never changes results.
    #[arg(long, default_value_t = launch::DEFAULT_WATCHDOG_MS, value_parser = clap::value_parser!(u64).range(1..=60000))]
    watchdog_ms: u64,
}

use seedbox_engine_core::launch;

impl Performance {
    fn apply(&self, executor: Executor) -> Result<Executor> {
        executor.performance(self.watchdog_ms)
    }
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
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Pause { run } => host_control(&run, true),
        Command::Resume { run } => host_control(&run, false),
        Command::Init {
            directory,
            template,
        } => init(&directory, template),
        Command::Validate { compose, json } => validate(&compose, json),
        Command::Doctor { cpu, inputs, json } => doctor(&cpu, inputs, json),
        Command::Prepare {
            compose,
            output,
            state,
            inputs,
        } => prepare(&compose, &output, &state, &inputs),
        Command::Run {
            compose,
            seed,
            checkpoint_at_ns,
            branch_seed,
            cpu,
            state,
            inputs,
            performance,
        } => run_simulation(
            &compose,
            seed,
            checkpoint_at_ns.zip(branch_seed),
            &cpu,
            &state,
            &inputs,
            &performance,
        ),
        Command::Explore {
            compose,
            seed,
            count,
            workers,
            json,
            cpu,
            state,
            inputs,
            performance,
        } => explore(
            &compose,
            ExploreOptions {
                seed,
                count,
                workers,
                cancellation: CancellationToken::default(),
            },
            json,
            &cpu,
            &state,
            &inputs,
            &performance,
        ),
        Command::Replay {
            run,
            cpu,
            state,
            performance,
            vmm,
        } => replay(run, &cpu, &state, &performance, vmm),
        Command::Export {
            run,
            output,
            state,
            transfer,
        } => export(run, output, &state, &transfer),
        Command::Import {
            bundle,
            state,
            transfer,
        } => import(bundle, &state, &transfer),
        Command::Diff { run_a, run_b, json } => compare(&run_a, &run_b, json),
        Command::Logs {
            run,
            machine,
            stream,
            since_ns,
            until_ns,
            json,
        } => show_logs(
            run,
            logs::Filter {
                machine,
                stream: stream.map(Into::into),
                since_ns,
                until_ns,
            },
            json,
        ),
        Command::Inspect { run, json } => inspect(run, json),
    }
}

fn host_control(run: &Path, paused: bool) -> Result<u8> {
    let status = seedbox::host_control::set_paused(run, paused)?;
    output::status(&output::word(&status), run.display());
    Ok(0)
}

fn init(directory: &Path, template: TemplateArg) -> Result<u8> {
    if directory.exists() {
        return Err(Error::Setup(format!(
            "{}: already exists; choose a new directory",
            directory.display()
        )));
    }
    seedbox::templates::init(directory, template.into())?;
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
fn doctor(cpu: &CpuArg, inputs: KvmInputs, json: bool) -> Result<u8> {
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    cpu.reject_on_mac()?;
    if let Some(path) = inputs.vmm {
        defaults::existing("--vmm", path)?;
        if let Some(kernel) = inputs.kernel {
            defaults::existing("--kernel", kernel)?;
        }
        return Err(Error::Setup(VMM_REQUIRES_KVM.into()));
    }
    let engine = defaults::engine(None)?;
    let kernel = match inputs.kernel {
        Some(path) => defaults::existing("--kernel", path)?,
        None => backends::guest_kernel(None)?,
    };
    if cancellation.is_cancelled() {
        return Err(Error::Setup("doctor cancelled".into()));
    }
    let report = seedbox::mac::MacExecutor::doctor(&engine, &kernel)?;
    if json {
        output::json(&report)?;
    } else {
        output::status("VALID", "compatible execution profile");
        output::field("simulator", &report.simulator_version);
        output::field("engine", report.engine.hex());
        output::field("kernel", report.kernel.hex());
    }
    Ok(0)
}

#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
fn doctor(cpu: &CpuArg, inputs: KvmInputs, json: bool) -> Result<u8> {
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let internal = inputs.vmm.is_none();
    let engine = defaults::engine(inputs.vmm)?;
    let kernel = backends::guest_kernel(inputs.kernel)?;
    let report = if internal {
        VmmExecutor::doctor_internal(&engine, &kernel, cpu.cpu)?
    } else {
        VmmExecutor::doctor(&engine, &kernel, cpu.cpu)?
    };
    if json {
        output::json(&report)?;
    } else {
        output::status("VALID", "compatible execution profile");
        output::field("cpu", report.cpu);
        output::field("cpus", output::Csv(&report.cpus));
        if let Some(version) = &report.simulator_version {
            output::field("simulator", version);
        }
        output::field("engine", report.engine.hex());
        output::field("kernel", report.kernel.hex());
    }
    Ok(0)
}

fn prepare(
    compose: &ComposeArg,
    output: &Path,
    state: &StateArg,
    inputs: &RuntimeInputs,
) -> Result<u8> {
    inputs.validate_platform()?;
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
    let (preparer, _) = inputs.backends(&store, None, &simulation, &cancellation)?;
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

fn run_simulation(
    compose: &ComposeArg,
    seed: Option<u64>,
    checkpoint: Option<(u64, u64)>,
    cpu: &CpuArg,
    state: &StateArg,
    inputs: &RuntimeInputs,
    performance: &Performance,
) -> Result<u8> {
    cpu.reject_on_mac()?;
    // clap requires both checkpoint flags together.
    if MAC && checkpoint.is_some() {
        return Err(Error::Setup(CHECKPOINT_REQUIRES_KVM.into()));
    }
    inputs.validate_platform()?;
    let simulation = compose.load()?;
    validate_machine(&simulation)?;
    if checkpoint.is_some_and(|(at, _)| at >= simulation.limits.simulated_time_ns) {
        return Err(Error::Setup("--checkpoint-at-ns must be before the simulated time limit; choose an earlier checkpoint".into()));
    }
    let (store, orchestrator) = state.open()?;
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let (preparer, executor) = inputs.admitted_backends(
        &store,
        cpu,
        &simulation,
        &cancellation,
        checkpoint,
        performance,
    )?;
    // Backend admission checks the selected core before preparation.
    let result = orchestrator.run(
        &simulation,
        RunOptions { seed, cancellation },
        &preparer,
        &executor,
    )?;
    Ok(report(&result))
}

fn explore(
    compose: &ComposeArg,
    options: ExploreOptions,
    json: bool,
    cpu: &CpuArg,
    state: &StateArg,
    inputs: &RuntimeInputs,
    performance: &Performance,
) -> Result<u8> {
    inputs.validate_platform()?;
    let resources = Resources::start();
    let simulation = compose.load()?;
    validate_machine(&simulation)?;
    let _signals = Signals::register(&options.cancellation)?;
    exploration::bounds(&simulation, &options)?;
    if options.workers > 1 && cpu.cpu.is_some() {
        return Err(Error::Setup("--workers: parallel exploration requires automatic core selection; omit --cpu and SEEDBOX_CPU".into()));
    }
    cpu.reject_on_mac()?;
    let (store, orchestrator) = state.open()?;
    let cancellation = options.cancellation.clone();
    let (preparer, executor) =
        inputs.admitted_backends(&store, cpu, &simulation, &cancellation, None, performance)?;
    // Exploration retains any backend admission refusal in its batch checkpoint.
    let mut result = orchestrator.explore(&simulation, options, &preparer, &executor)?;
    result.record_resources(resources.finish())?;
    if json {
        output::json(&result)?;
    } else {
        output::status(&output::word(&result.outcome), result.directory.display());
        output::field(
            "seeds",
            format_args!("{}..={}", result.first_seed, result.last_seed),
        );
        output::field("workers", result.workers);
        output::field("peak-running", result.peak_running);
        output::field(
            "active-attempt-seconds",
            output::Seconds(result.active_attempt_seconds),
        );
        output::field("attempted", result.attempts.len());
        output::field("pass", result.counts.pass);
        output::field("fail", result.counts.fail);
        output::field("timeout", result.counts.timeout);
        output::field("error-count", result.counts.error);
        output::field("incomplete", result.counts.incomplete);
        output::field("unfinished", result.counts.unfinished);
        output::field("undispatched", result.undispatched_count);
        output::field("elapsed-seconds", output::Seconds(result.total_seconds));
        for attempt in &result.attempts {
            if let (Some(directory), Some(outcome)) = (&attempt.directory, &attempt.outcome) {
                eprintln!(
                    "seed {}: {} {}",
                    attempt.seed,
                    output::word(outcome),
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

fn replay(
    run: PathBuf,
    cpu: &CpuArg,
    state: &StateArg,
    performance: &Performance,
    vmm: Option<PathBuf>,
) -> Result<u8> {
    let allow_engine_change = vmm.is_some();
    let run = defaults::run_directory(run)?;
    let previous = orchestration::inspect(&run)?;
    let profile = &previous.identity.inputs.profile;
    let is_mac = profile.backend == Backend::Hvf;
    let (store, orchestrator) = state.open()?;
    let scratch = tempfile::tempdir()?;
    let engine = match vmm {
        Some(path) => defaults::existing("--vmm", path)?,
        None => store.materialize(&profile.engine, scratch.path())?,
    };
    let executor = if is_mac {
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        {
            cpu.reject_on_mac()?;
            Executor::Mac(seedbox::mac::MacExecutor::from_retained(
                &engine,
                &store,
                profile.clone(),
            )?)
        }
        #[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
        {
            return Err(Error::Setup(
                "replay of an HVF world requires an Apple Silicon Mac".into(),
            ));
        }
    } else {
        // The recorded profile carries every semantic setting.
        Executor::Linux(if allow_engine_change {
            VmmExecutor::from_override(&engine, &store, profile.clone(), cpu.cpu)?
        } else {
            VmmExecutor::from_retained(&engine, &store, profile.clone(), cpu.cpu)?
        })
    };
    let executor = performance.apply(executor)?;
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

fn export(
    run: PathBuf,
    output: Option<PathBuf>,
    state: &StateArg,
    transfer: &Transfer,
) -> Result<u8> {
    let run = defaults::run_directory(run)?;
    let output = match output {
        Some(path) => path,
        None => {
            let name = run
                .file_name()
                .ok_or_else(|| Error::Setup(format!("{}: no run directory name", run.display())))?;
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
    let options = transfer.options(cancellation);
    let store = ContentStore::new(state.state.join("objects"));
    let record = bundle::export(&run, &store, &output, &options).map_err(|e| {
        Error::Setup(format!(
            "export {} to {}: {e}",
            run.display(),
            output.display()
        ))
    })?;
    output::status("EXPORTED", output.display());
    print_omissions(&record);
    Ok(0)
}

fn import(path: PathBuf, state: &StateArg, transfer: &Transfer) -> Result<u8> {
    let path = defaults::existing("bundle", path)?;
    let cancellation = CancellationToken::default();
    let _signals = Signals::register(&cancellation)?;
    let options = transfer.options(cancellation);
    // Import performs its own no-follow opening; StateArg::open would
    // canonicalize away root symlink evidence.
    let run = bundle::import(&path, &state.state, &options).map_err(|e| {
        Error::Setup(format!(
            "import {} into {}: {e}",
            path.display(),
            state.state.display()
        ))
    })?;
    output::status("IMPORTED", run.directory.display());
    print_omissions(&run.record);
    Ok(0)
}

fn compare(run_a: &Path, run_b: &Path, json: bool) -> Result<u8> {
    let comparison = diff::diff(run_a, run_b)?;
    if json {
        output::json(&comparison)?;
    } else {
        output::status(
            &output::word(&comparison.status),
            format_args!("{} <> {}", run_a.display(), run_b.display()),
        );
        output::field(
            "world witness",
            serde_json::to_string(&comparison.world_witness)?,
        );
        for machine in &comparison.machines {
            output::field("machine", serde_json::to_string(machine)?);
            if let Some(pages) = &machine.ram_pages {
                diagnostics::ram_pages(&machine.machine, pages);
            }
        }
        output::field("divergence", serde_json::to_string(&comparison.divergence)?);
        if let Some(divergence) = &comparison.divergence {
            diagnostics::divergence(divergence);
        }
        output::field(
            "first record",
            serde_json::to_string(&comparison.first_record)?,
        );
        for (label, differences) in [
            ("identity", &comparison.identity_differences),
            ("result", &comparison.result_differences),
            ("evidence", &comparison.evidence_differences),
        ] {
            for difference in differences {
                output::field(label, serde_json::to_string(difference)?);
            }
        }
        for limitation in &comparison.limitations {
            output::field("limitation", limitation);
        }
    }
    Ok(match comparison.status {
        diff::Status::Match => 0,
        diff::Status::Different => 1,
    })
}

fn show_logs(run: PathBuf, filter: logs::Filter, json: bool) -> Result<u8> {
    use std::io::Write;
    let run = defaults::run_directory(run)?;
    let mut reader = logs::Reader::open(&run, &filter)?;
    let mut output = std::io::BufWriter::new(std::io::stdout().lock());
    if json {
        reader.write_json(&mut output)?;
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
        reader.write_raw(&mut output)?;
    }
    output.flush()?;
    Ok(0)
}

fn inspect(run: PathBuf, json: bool) -> Result<u8> {
    let run = defaults::run_directory(run)?;
    let record = orchestration::inspect(&run)?;
    if json {
        output::json(&record)?;
        return Ok(0);
    }
    output::status(
        &output::word(&record.outcome),
        format_args!("{} (seed {})", record.attempt, record.identity.seed),
    );
    if let Some(replay) = record.replay {
        output::field("replay", output::word(&replay.status));
    }
    if let Some(error) = record.error {
        output::field("error", &error);
        diagnostics::vmm_failure(&run, &error);
    }
    Ok(0)
}

/// Print a finished run and return its exit status (see `EXIT_CODES`).
fn report(run: &orchestration::Run) -> u8 {
    // Scripts parse the run directory from this one `OUTCOME: path` line.
    output::status(&output::word(&run.record.outcome), run.directory.display());
    if let Some(engine) = &run
        .record
        .identity
        .inputs
        .profile
        .semantic_parameters
        .engine
    {
        output::field("simulator", &engine.simulator_version);
    }
    if let Some(error) = &run.record.error {
        eprintln!("{error}");
        diagnostics::vmm_failure(&run.directory, error);
    }
    if let Some(replay) = &run.record.replay {
        output::field("replay", output::word(&replay.status));
        diagnostics::replay_pages(replay);
        diagnostics::replay_divergence(replay);
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
    // SAFETY: rusage is plain C data, so all-zero bytes are a valid value.
    let mut usage = unsafe { std::mem::zeroed() };
    // SAFETY: `usage` is a valid, writable rusage for the duration of the call.
    if unsafe { libc::getrusage(who, &mut usage) } == 0 {
        Some(usage)
    } else {
        None
    }
}

fn print_omissions(record: &orchestration::RunRecord) {
    if !record.omitted_logs.is_empty() {
        output::field("omitted files", record.omitted_logs.len());
    }
}
