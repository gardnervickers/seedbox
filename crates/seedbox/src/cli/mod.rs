//! The `seedbox` command line. Style rules for changes: `AGENTS.md`, "CLI".

mod defaults;
mod signals;

use clap::{Args, Parser, Subcommand, ValueEnum};
use seedbox::{
    bundle, config,
    counter::CounterProfile,
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, PreparationBackend, WitnessMode,
    },
    oci::OciTools,
    orchestration::{self, Orchestrator, Outcome, ReplayOptions, ReplayStatus, RunOptions},
    preparation::OciPreparer,
    store::ContentStore,
    templates::Template,
    vmm::{recorded_counter, VmmExecutor},
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
    /// Check Compose services and directed network policies, without images or guests.
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
    /// Run even when this CPU's branch counter profile is unqualified; its results are unverified.
    #[arg(long)]
    allow_unqualified_cpu: bool,
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
struct RuntimeInputs {
    /// Guest kernel bzImage [default: bzImage in $SEEDBOX_ARTIFACTS, $SEEDBOX_WORK/artifacts,
    /// ./artifacts, then ~/seedbox-sim/artifacts for the user and any sudo caller].
    #[arg(long, env = "SEEDBOX_KERNEL")]
    kernel: Option<PathBuf>,
    /// VMM binary [default: next to an installed seedbox, then this checkout's `cargo build-vmm`
    /// output (built by `cargo run` if missing), then PATH].
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
        counter: CounterProfile,
        scratch: &Path,
    ) -> Result<(OciPreparer, VmmExecutor)> {
        let engine = defaults::engine(self.vmm.clone())?;
        let witness = if self.full_state {
            WitnessMode::FullState
        } else {
            WitnessMode::Exits
        };
        let executor = VmmExecutor::new(&engine, store, cpu, self.memory_mb, witness, counter)?;
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
            // Preparation runs no guest, so it may run on any machine; off a
            // supported CPU it prepares for the qualified Zen profile.
            let counter = CounterProfile::host().unwrap_or(CounterProfile::Zen);
            let (preparer, _) = inputs.backends(&store, None, counter, scratch.path())?;
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
            // Report a CPU without a usable profile only after the input
            // checks; until then, Zen stands in.
            let detected = CounterProfile::host();
            let counter = *detected.as_ref().unwrap_or(&CounterProfile::Zen);
            let (preparer, executor) = inputs.backends(&store, cpu.cpu, counter, scratch.path())?;
            let executor = performance.apply(executor)?;
            // After the fast input checks, before slow image preparation.
            let counter = cpu.admit(detected?)?;
            require_kvm_host(counter)?;
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
            let counter = cpu.admit(CounterProfile::host()?)?;
            same_counter(
                &run,
                recorded_counter(&previous.identity.inputs.profile)?,
                counter,
            )?;
            require_kvm_host(counter)?;
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
                counter,
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

/// Fail fast when this host cannot execute: no KVM, or no MSR access when the
/// counter profile needs a workaround.
fn require_kvm_host(counter: CounterProfile) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Setup(
            "run and replay require the qualified Linux KVM host".into(),
        ));
    }
    for (path, fix) in host_devices(counter) {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|error| Error::Setup(format!("{path}: {error}; {fix}")))?;
    }
    Ok(())
}

/// Devices `run` and `replay` need: KVM, plus MSR access only when the
/// counter profile has a host workaround.
fn host_devices(counter: CounterProfile) -> Vec<(&'static str, &'static str)> {
    let mut devices = vec![(
        "/dev/kvm",
        "enable KVM and run as root (e.g. sudo -E env PATH=\"$PATH\" seedbox ...)",
    )];
    if counter.workaround().is_some() {
        devices.push((
            "/dev/cpu/0/msr",
            "run as root (e.g. sudo -E env PATH=\"$PATH\" seedbox ...) with the msr module loaded",
        ));
    }
    devices
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

fn print_omissions(record: &orchestration::RunRecord) {
    if !record.omitted_logs.is_empty() {
        println!("omitted files: {}", record.omitted_logs.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_profiles_with_a_workaround_need_msr_access() {
        let paths = |counter| {
            host_devices(counter)
                .into_iter()
                .map(|(path, _)| path)
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(CounterProfile::Zen), ["/dev/kvm", "/dev/cpu/0/msr"]);
        assert_eq!(paths(CounterProfile::IntelIceLake), ["/dev/kvm"]);
    }

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
