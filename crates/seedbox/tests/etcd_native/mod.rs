//! Fast preparation smoke through the normal CLI/service, not bd-207 acceptance.
//! Canonical acceptance must run examples/etcd unchanged. No BackendProvider,
//! prepared guest manifest, substituted oracle or image. Both PV and TCG use
//! the same flow, with TCG explicitly selected through the public CLI.
use super::etcd_project::*;
use seedbox_api::{InputSource, Simulation, SimulationComparison};
use seedbox_test_time::HangGuard;
use std::{
    ffi::OsString,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

use crate::support::{failure_capture, json_diff};

type Result<T> = std::result::Result<T, String>;

struct Cli {
    executable: PathBuf,
    directory: PathBuf,
    state: PathBuf,
}
impl Cli {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        // This case owns a complete embedded Driver build. Inherited bootstrap
        // paths (including nix develop's static mke2fs) select legacy mode and
        // must not override that build. Preserve the engine connection settings.
        for key in [
            "SEEDBOX_VMM",
            "SEEDBOX_KERNEL",
            "SEEDBOX_RUNTIME_ASSETS",
            "SEEDBOX_GUEST_INIT",
            "SEEDBOX_MKE2FS",
            "SEEDBOX_ASSET_DIR",
        ] {
            command.env_remove(key);
        }
        command
            .env("SEEDBOX_STATE", &self.state)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    async fn invoke(&self, guard: &HangGuard, label: &str, args: &[OsString]) -> Result<Vec<u8>> {
        eprintln!("etcd CLI {label}: {args:?}");
        let stdout = self.directory.join(format!("{label}.stdout"));
        let stderr = self.directory.join(format!("{label}.stderr"));
        let mut command = self.command();
        command
            .args(args)
            .stdout(fs::File::create(&stdout).map_err(|e| e.to_string())?)
            .stderr(fs::File::create(&stderr).map_err(|e| e.to_string())?);
        let mut child = seedbox_test::process::spawn_with(|| command.spawn())
            .map_err(|e| format!("spawn CLI {label}: {e}"))?;
        let status =
            tokio::time::timeout(guard.remaining().map_err(|e| e.to_string())?, child.wait())
                .await
                .map_err(|_| {
                    format!(
                        "infrastructure hang waiting for CLI {label}; see {}",
                        self.directory.display()
                    )
                })?
                .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!(
                "CLI {label}: {status}; stdout={} stderr={}",
                stdout.display(),
                stderr.display()
            ));
        }
        fs::read(stdout).map_err(|e| e.to_string())
    }

    async fn replay(&self, guard: &HangGuard, baseline: &str) -> Result<Vec<u8>> {
        let result = self
            .invoke(
                guard,
                "replay",
                &words(&["replay", baseline, "--json", "--wall-time", "20m"]),
            )
            .await;
        if let Err(error) = &result {
            let directory = self.directory.join("failure");
            failure_capture::note(&directory, error);
            let diagnostics = failure_capture::budget();
            let capture = async {
                let bytes = failure_capture::read(&self.directory.join("replay.stdout"))?;
                fs::write(directory.join("replay.stdout"), &bytes).map_err(|e| e.to_string())?;
                let stderr = failure_capture::read(&self.directory.join("replay.stderr"))?;
                fs::write(directory.join("replay.stderr"), stderr).map_err(|e| e.to_string())?;
                let comparison: SimulationComparison = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("replay comparison: {e}"))?;
                if comparison.baseline.as_str() != baseline
                    || comparison.simulation.as_str() == baseline
                {
                    return Err(
                        "replay comparison does not name a fresh simulation of this baseline"
                            .into(),
                    );
                }
                // The definition diff needs the replay's resolved definition.
                if let Err(error) = self
                    .describe(
                        &bounded_diagnostics(guard, &diagnostics)?,
                        "replay-result",
                        comparison.simulation.as_str(),
                    )
                    .await
                {
                    failure_capture::note(&directory, &format!("replay definition: {error}"));
                }
                self.capture_comparison_failure(
                    guard,
                    &diagnostics,
                    baseline,
                    comparison.simulation.as_str(),
                    "replay",
                )
                .await;
                // Use the same CLI environment as the scenario, with bounded pipe readers.
                let mut command = self.command().into_std();
                command.args([
                    "logs",
                    comparison.simulation.as_str(),
                    "oracle",
                    "--tail",
                    "100",
                ]);
                failure_capture::command_async(
                    command,
                    &directory,
                    "replay-oracle-log",
                    &bounded_diagnostics(guard, &diagnostics)?,
                )
                .await
            };
            if let Err(error) = capture.await {
                failure_capture::note(&directory, &error);
            }
            self.log_comparison_summary("replay", "replay");
        }
        result
    }

    async fn describe(&self, guard: &HangGuard, label: &str, id: &str) -> Result<Simulation> {
        let bytes = self
            .invoke(
                guard,
                label,
                &words(&["show", "--view", "definition", id, "--json"]),
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{label}: {e}"))
    }

    /// Retain terminal errors while the catalogue still exists. Observation
    /// failure is diagnostic only and never replaces the original case failure.
    async fn capture_failure_resources(&self, guard: &HangGuard) -> Result<()> {
        let bytes = self
            .invoke(guard, "failure-resources", &words(&["show", "--json"]))
            .await?;
        for summary in parse_resource_summaries(&bytes)? {
            let id = summary.metadata.id;
            let resource = self
                .describe(guard, &format!("failure-{id}"), id.as_str())
                .await?;
            eprintln!(
                "captured resource {id} before shutdown: {:?}",
                resource.status
            );
        }
        Ok(())
    }

    async fn fresh(
        &self,
        guard: &HangGuard,
        backend: Backend,
        project: &Path,
        seed: u64,
        label: &str,
    ) -> Result<Simulation> {
        let mut args = submit_arguments(project, seed, backend)?;
        // This request budget diagnoses a host hang; the actual oracle's write
        // count and simulated duration determine its result, not host speed.
        args.extend(words(&["--json", "--wall-time", "20m"]));
        let submitted: Simulation =
            serde_json::from_slice(&self.invoke(guard, label, &args).await?)
                .map_err(|e| e.to_string())?;
        let id = submitted.metadata.id.to_string();
        self.invoke(
            guard,
            &format!("{label}-paused"),
            &words(&["wait", &id, "--for", "paused"]),
        )
        .await?;
        let prepared = self
            .describe(guard, &format!("{label}-prepared"), &id)
            .await?;
        require_definition(&prepared, backend, seed)?;
        self.invoke(
            guard,
            &format!("{label}-resume"),
            &words(&["control", "resume", &id]),
        )
        .await?;
        self.invoke(
            guard,
            &format!("{label}-completed"),
            &words(&["wait", &id, "--for", "completed"]),
        )
        .await?;
        let completed = self
            .describe(guard, &format!("{label}-result"), &id)
            .await?;
        require_definition(&completed, backend, seed)?;
        require_pass(&completed.status)?;
        eprintln!(
            "etcd {} seed={seed} oracle={:?}; host timings (diagnostic only) {:?}",
            backend.name(),
            completed.status.verdict(),
            completed.status.stage_timings
        );
        require_oracle_output(
            &self
                .invoke(
                    guard,
                    &format!("{label}-oracle-log"),
                    &words(&["logs", &id, "oracle"]),
                )
                .await?,
        )?;
        Ok(completed)
    }
}

// Failure evidence stays small and belongs only to this test's private session.
const DIAGNOSTIC_LIMIT: u64 = 8 * 1024 * 1024;

fn diagnostic_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(DIAGNOSTIC_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > DIAGNOSTIC_LIMIT {
        return Err(format!(
            "diagnostic input exceeds 8 MiB: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

fn attempt_directory(state: &Path, id: &str) -> Result<PathBuf> {
    let mut found = None;
    for session in fs::read_dir(state.join("sessions")).map_err(|e| e.to_string())? {
        let attempts = session
            .map_err(|e| e.to_string())?
            .path()
            .join("runs")
            .join(id)
            .join("attempts");
        if !attempts.is_dir() {
            continue;
        }
        for entry in fs::read_dir(attempts).map_err(|e| e.to_string())? {
            let attempt = entry.map_err(|e| e.to_string())?.path();
            if !attempt.is_dir() {
                continue;
            }
            let manifest = diagnostic_bytes(&attempt.join("manifest.json"))?;
            seedbox_application::record::inspect_bytes(&manifest).map_err(|e| e.to_string())?;
            if found.replace(attempt).is_some() {
                return Err(format!("multiple retained attempts for simulation {id}"));
            }
        }
    }
    found.ok_or_else(|| format!("no retained attempt for simulation {id}"))
}

fn copy_state_report(
    attempt: &Path,
    destination: &Path,
    logs: &std::collections::BTreeMap<String, seedbox_application::store::ContentDigest>,
) -> Result<seedbox_application::evidence::StateReport> {
    let name = format!("machines/{}", seedbox_application::evidence::STATE_REPORT);
    let bytes = diagnostic_bytes(&attempt.join("output").join(&name))?;
    if logs.get(&name) != Some(&seedbox_application::store::ContentDigest::of(&bytes)) {
        return Err("stopped-state report log digest mismatch".into());
    }
    let state: seedbox_application::evidence::StateReport =
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    state.validate().map_err(|e| e.to_string())?;
    fs::write(destination, bytes).map_err(|e| e.to_string())?;
    Ok(state)
}

fn copy_oracle_page_hashes(
    attempt: &Path,
    destination: &Path,
    logs: &std::collections::BTreeMap<String, seedbox_application::store::ContentDigest>,
    state: &seedbox_application::evidence::StateReport,
) -> Result<()> {
    let oracle = state
        .machines
        .get("oracle")
        .ok_or("stopped-state report has no oracle page hashes")?;
    let name = format!("machines/{}", oracle.page_log);
    let expected = oracle.page_bytes().map_err(|e| e.to_string())?;
    if expected > DIAGNOSTIC_LIMIT {
        return Err(format!("oracle page hashes exceed 8 MiB: {name}"));
    }
    let recorded = logs
        .get(&name)
        .ok_or_else(|| format!("oracle page hash log absent from manifest: {name}"))?;
    let bytes = diagnostic_bytes(&attempt.join("output").join(&name))
        .map_err(|error| format!("oracle page hash log {name}: {error}"))?;
    if bytes.len() as u64 != expected {
        return Err(format!("oracle page hash length mismatch: {name}"));
    }
    if recorded != &seedbox_application::store::ContentDigest::of(&bytes) {
        return Err(format!("oracle page hash log digest mismatch: {name}"));
    }
    // The CI collector accepts .out and caps each file at 8 MiB. These are
    // page hashes, not guest RAM contents.
    fs::write(destination, bytes).map_err(|e| e.to_string())
}

fn copy_worker_stderr(
    attempt: &Path,
    destination: &Path,
    logs: &std::collections::BTreeMap<String, seedbox_application::store::ContentDigest>,
) -> Result<()> {
    let name = "vmm.stderr";
    let expected = logs
        .get(name)
        .ok_or("retained worker stderr is unavailable")?;
    let bytes = diagnostic_bytes(&attempt.join("output").join(name))?;
    if expected != &seedbox_application::store::ContentDigest::of(&bytes) {
        return Err("retained worker stderr digest mismatch".into());
    }
    fs::write(destination, bytes).map_err(|e| e.to_string())
}

impl Cli {
    async fn capture_comparison_failure(
        &self,
        scenario: &HangGuard,
        diagnostics: &HangGuard,
        baseline: &str,
        repeated: &str,
        other: &str,
    ) {
        let directory = self.directory.join("failure");
        let result = async {
            let guard = bounded_diagnostics(scenario, diagnostics)?;
            // Offline diffing can read large retained files. Run it in an owned
            // test subprocess so cancellation stops the actual work as well.
            let mut command =
                std::process::Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
            command
                .args([
                    "--exact",
                    "etcd_native::capture_comparison_worker",
                    "--nocapture",
                ])
                .env("SEEDBOX_CAPTURE_DIRECTORY", &self.directory)
                .env("SEEDBOX_CAPTURE_STATE", &self.state)
                .env("SEEDBOX_CAPTURE_BASELINE", baseline)
                .env("SEEDBOX_CAPTURE_REPEATED", repeated)
                .env("SEEDBOX_CAPTURE_OTHER", other);
            failure_capture::command_async(command, &directory, "comparison-capture", &guard).await
        }
        .await;
        if let Err(error) = result {
            failure_capture::note(&directory, &error);
        }
    }

    /// `other` labels the compared side ("repeat" or "replay").
    fn capture_comparison_files(&self, baseline: &str, repeated: &str, other: &str) {
        let directory = self.directory.join("failure");
        failure_capture::note(&directory, &format!("comparison {baseline} / {repeated}"));
        let capture = || -> Result<()> {
            let left = attempt_directory(&self.state, baseline)?;
            let right = attempt_directory(&self.state, repeated)?;
            for (label, attempt) in [("baseline", &left), (other, &right)] {
                let copy = || -> Result<()> {
                    let manifest = diagnostic_bytes(&attempt.join("manifest.json"))?;
                    let record = seedbox_application::record::inspect_bytes(&manifest)
                        .map_err(|e| e.to_string())?;
                    fs::write(directory.join(format!("{label}-manifest.json")), manifest)
                        .map_err(|e| e.to_string())?;
                    let state = copy_state_report(
                        attempt,
                        &directory.join(format!(
                            "{label}-{}",
                            seedbox_application::evidence::STATE_REPORT
                        )),
                        &record.logs,
                    )?;
                    copy_oracle_page_hashes(
                        attempt,
                        &directory.join(format!("{label}-oracle-pages.out")),
                        &record.logs,
                        &state,
                    )?;
                    if record.logs.contains_key("vmm.stderr") {
                        copy_worker_stderr(
                            attempt,
                            &directory.join(format!("{label}-vmm.stderr")),
                            &record.logs,
                        )?;
                    }
                    Ok(())
                };
                if let Err(error) = copy() {
                    failure_capture::note(
                        &directory,
                        &format!("etcd {label} diagnostic capture failed: {error}"),
                    );
                }
            }
            // These readers verify retained log digests and never execute guests.
            let state = seedbox_application::diff::diff(&left, &right)
                .map_err(|e| e.to_string())
                .and_then(|report| serde_json::to_vec_pretty(&report).map_err(|e| e.to_string()));
            let pages = seedbox_application::diff::replay_ram_pages(&left, &right)
                .map_err(|e| e.to_string())
                .and_then(|report| serde_json::to_vec_pretty(&report).map_err(|e| e.to_string()));
            for (name, report) in [
                ("compare-state.json", state),
                ("compare-ram-pages.json", pages),
            ] {
                let write = report.and_then(|bytes| {
                    if bytes.len() as u64 > DIAGNOSTIC_LIMIT {
                        return Err("comparison diagnostic exceeds 8 MiB".into());
                    }
                    fs::write(directory.join(name), bytes).map_err(|e| e.to_string())
                });
                if let Err(error) = write {
                    failure_capture::note(
                        &directory,
                        &format!("etcd {name} diagnostic capture failed: {error}"),
                    );
                }
            }
            Ok(())
        };
        if let Err(error) = capture() {
            failure_capture::note(
                &directory,
                &format!("etcd comparison diagnostic capture failed: {error}"),
            );
        }
    }

    /// CI keeps only the job log readily visible, so summarize the retained
    /// comparison evidence there: the CLI verdict, where the two resolved
    /// definitions differ (prepared inputs included), and the state diff head.
    /// Every input is a bounded file this test already wrote.
    fn log_comparison_summary(&self, other: &str, output: &str) {
        let read = |path: PathBuf| failure_capture::read(&path).ok();
        for stream in ["stdout", "stderr"] {
            match read(self.directory.join(format!("{output}.{stream}"))) {
                Some(bytes) => eprintln!(
                    "etcd {output} {stream}: {}",
                    String::from_utf8_lossy(&bytes[..bytes.len().min(SUMMARY_BYTES)])
                ),
                None => eprintln!("etcd {output} {stream} unavailable"),
            }
        }
        let spec = |name: String| {
            read(self.directory.join(name))
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .map(|resource| resource["spec"].clone())
        };
        match (
            spec("baseline-result.stdout".into()),
            spec(format!("{other}-result.stdout")),
        ) {
            (Some(baseline), Some(compared)) => {
                let mut differences = Vec::new();
                json_diff::json_differences("spec", &baseline, &compared, &mut differences);
                eprintln!(
                    "etcd baseline/{other} definitions differ at {} paths",
                    differences.len()
                );
                for difference in differences.iter().take(SUMMARY_LINES) {
                    eprintln!("  {difference}");
                }
            }
            _ => eprintln!("etcd baseline/{other} definition diff unavailable"),
        }
        if let Some(bytes) = read(self.directory.join("failure/compare-state.json")) {
            for line in String::from_utf8_lossy(&bytes).lines().take(SUMMARY_LINES) {
                eprintln!("etcd compare-state: {line}");
            }
        }
    }
}

const SUMMARY_BYTES: usize = 4096;
const SUMMARY_LINES: usize = 60;

// The CLI flattens catalogue pages into a bare array, not the API's
// SimulationList page object. Each array element is a summary object.
fn parse_resource_summaries(bytes: &[u8]) -> Result<Vec<seedbox_api::SimulationSummary>> {
    serde_json::from_slice(bytes).map_err(|e| e.to_string())
}

fn words(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn require_definition(simulation: &Simulation, backend: Backend, seed: u64) -> Result<()> {
    let InputSource::Resolved { definition } = &simulation.spec.inputs else {
        return Err("normal OCI submission did not retain a resolved definition".into());
    };
    require_backend(&definition.runtime.execution, backend)?;
    if definition.seed != seed
        || !definition.simulator.oracle_control
        || definition.machines.len() != 3
    {
        return Err("seed, three actual members or implicit oracle control was lost".into());
    }
    for name in ["n1", "n2", "n3"] {
        let member = definition
            .machines
            .get(name)
            .ok_or_else(|| format!("missing member {name}"))?;
        if member.volumes.len() != 1
            || !member
                .volumes
                .get(&format!("{name}-data"))
                .is_some_and(|volume| volume.target == "/data" && volume.size_bytes > 0)
        {
            return Err(format!("{name} did not retain its persistent /data disk"));
        }
    }
    Ok(())
}

async fn start_server(cli: &Cli, guard: &HangGuard) -> Result<Child> {
    let mut command = cli.command();
    command
        .args([
            "server",
            "start",
            "--foreground",
            "--startup-handshake",
            "--full-state",
        ])
        .stdout(Stdio::piped())
        .stderr(fs::File::create(cli.directory.join("server.stderr")).map_err(|e| e.to_string())?);
    let mut child =
        seedbox_test::process::spawn_with(|| command.spawn()).map_err(|e| e.to_string())?;
    let mut ready = String::new();
    let mut reader = BufReader::new(child.stdout.take().ok_or("missing startup pipe")?);
    tokio::time::timeout(
        guard.remaining().map_err(|e| e.to_string())?,
        reader.read_line(&mut ready),
    )
    .await
    .map_err(|_| "infrastructure hang during server startup")?
    .map_err(|e| e.to_string())?;
    let document: serde_json::Value = serde_json::from_str(&ready).map_err(|e| e.to_string())?;
    if document["status"] != "ready" {
        return Err(format!("bad startup handshake: {ready}"));
    }
    fs::write(cli.directory.join("server-ready.json"), ready).map_err(|e| e.to_string())?;
    Ok(child)
}

async fn scenario(cli: &Cli, backend: Backend, guard: &HangGuard, project: &Path) -> Result<()> {
    let baseline = cli
        .fresh(guard, backend, project, BASELINE_SEED, "baseline")
        .await?;
    let repeated = cli
        .fresh(guard, backend, project, BASELINE_SEED, "repeat")
        .await?;
    let baseline_id = baseline.metadata.id.to_string();
    let repeated_id = repeated.metadata.id.to_string();
    let comparison = cli
        .invoke(
            guard,
            "compare-repeat",
            &words(&["compare", &baseline_id, &repeated_id, "--json"]),
        )
        .await;
    if let Err(error) = &comparison {
        failure_capture::note(&cli.directory.join("failure"), error);
        cli.capture_comparison_failure(
            guard,
            &failure_capture::budget(),
            &baseline_id,
            &repeated_id,
            "repeat",
        )
        .await;
        cli.log_comparison_summary("repeat", "compare-repeat");
    }
    let compared: SimulationComparison =
        serde_json::from_slice(&comparison?).map_err(|e| e.to_string())?;
    if compared.baseline != baseline.metadata.id
        || compared.simulation != repeated.metadata.id
        || baseline.metadata.id == repeated.metadata.id
    {
        return Err(
            "repeat comparison did not compare two independently submitted simulations".into(),
        );
    }
    if let Err(error) = require_match(&compared) {
        failure_capture::note(&cli.directory.join("failure"), &error);
        cli.capture_comparison_failure(
            guard,
            &failure_capture::budget(),
            &baseline_id,
            &repeated_id,
            "repeat",
        )
        .await;
        cli.log_comparison_summary("repeat", "compare-repeat");
        return Err(error);
    }
    // Reports are already retained; bound memory by releasing completed native
    // worlds rather than accumulating multiple four-machine executions.
    cli.invoke(guard, "delete-repeat", &words(&["delete", &repeated_id]))
        .await?;
    // No new backend selection on replay: the retained runtime must own it.
    let replay: SimulationComparison =
        serde_json::from_slice(&cli.replay(guard, &baseline_id).await?)
            .map_err(|e| e.to_string())?;
    if replay.baseline != baseline.metadata.id
        || replay.simulation == baseline.metadata.id
        || replay.simulation == repeated.metadata.id
    {
        return Err("replay did not create/compare a fresh simulation from the original".into());
    }
    if let Err(error) = require_match(&replay) {
        let failure = cli.directory.join("failure");
        failure_capture::note(&failure, &error);
        let diagnostics = failure_capture::budget();
        // The definition diff needs the replay's resolved definition.
        let described = match bounded_diagnostics(guard, &diagnostics) {
            Ok(bounded) => cli
                .describe(&bounded, "replay-result", replay.simulation.as_str())
                .await
                .map(drop),
            Err(error) => Err(error),
        };
        if let Err(capture) = described {
            failure_capture::note(&failure, &format!("replay definition: {capture}"));
        }
        cli.capture_comparison_failure(
            guard,
            &diagnostics,
            &baseline_id,
            replay.simulation.as_str(),
            "replay",
        )
        .await;
        cli.log_comparison_summary("replay", "replay");
        return Err(error);
    }
    let replayed = cli
        .describe(guard, "replay-result", replay.simulation.as_str())
        .await?;
    require_definition(&replayed, backend, BASELINE_SEED)?;
    require_pass(&replayed.status)?;
    require_oracle_output(
        &cli.invoke(
            guard,
            "replay-oracle-log",
            &words(&["logs", replay.simulation.as_str(), "oracle"]),
        )
        .await?,
    )?;
    cli.invoke(
        guard,
        "delete-replay",
        &words(&["delete", replay.simulation.as_str()]),
    )
    .await?;
    cli.invoke(guard, "delete-baseline", &words(&["delete", &baseline_id]))
        .await?;
    // Diagnostics only; never assert a host speed or compare timings as evidence.
    eprintln!(
        "etcd {} seed123 baseline/repeat/replay PASS; witness/full-state MATCH",
        backend.name()
    );
    Ok(())
}

pub fn run(backend: Backend) {
    let executable = std::env::var_os("SEEDBOX_TEST_CLI")
        .map(PathBuf::from)
        .expect("set SEEDBOX_TEST_CLI to the normal release CLI with prepared runtime assets");
    assert!(
        executable.is_file(),
        "missing release CLI {}",
        executable.display()
    );
    let executable = executable.canonicalize().unwrap();
    let report_root = std::env::var_os("SEEDBOX_TEST_REPORT_DIR")
        .map(PathBuf::from)
        .expect("set SEEDBOX_TEST_REPORT_DIR to a caller-owned report directory");
    // Reports may live under a long checkout path. Keep the API socket's state
    // in the caller-owned short TMPDIR so Linux's sockaddr_un limit is respected.
    let scratch_root =
        std::env::var_os("TMPDIR").expect("set TMPDIR to a short caller-owned scratch directory");
    let state_directory = tempfile::Builder::new()
        .prefix(&format!("etcd-state-{}-", backend.name()))
        .tempdir_in(scratch_root)
        .unwrap();
    // Keep only small command reports. The large native cache/state is a
    // separate RAII directory created by this test and removed after shutdown.
    let directory = tempfile::Builder::new()
        .prefix(&format!("etcd-smoke-{}-", backend.name()))
        .tempdir_in(&report_root)
        .unwrap()
        .keep();
    eprintln!(
        "etcd {} retained evidence: {}",
        backend.name(),
        directory.display()
    );
    let cli = Cli {
        executable,
        state: state_directory.path().join("state"),
        directory,
    };
    // Resolve/write all host-only inputs before starting any owned server.
    let project = write_acceptance_project(&cli.directory);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result: Result<()> = runtime.block_on(async {
        let guard = HangGuard::new(
            "positive normal etcd CLI/service acceptance; expiration is infrastructure failure",
            Duration::from_secs(1200),
        );
        let mut server = start_server(&cli, &guard).await?;
        let result = scenario(&cli, backend, &guard, &project).await;
        if let Err(error) = &result {
            eprintln!("etcd scenario failed before cleanup: {error}");
        }
        // Cleanup owns a separate infrastructure budget, including on a failed
        // assertion. Shutdown cancels and reaps simulations before server exit.
        let cleanup = HangGuard::new(
            "graceful etcd test server and owned worker shutdown",
            Duration::from_secs(120),
        );
        if result.is_err() {
            if let Err(error) = cli.capture_failure_resources(&cleanup).await {
                eprintln!("resource diagnostic capture failed (original error retained): {error}");
            }
        }
        let shutdown = cli
            .invoke(&cleanup, "shutdown", &words(&["server", "stop"]))
            .await;
        if let Err(error) = &shutdown {
            eprintln!("graceful shutdown command failed: {error}; terminating our owned server");
            if let Some(pid) = server.id() {
                // SAFETY: this is our unreaped child, so the PID cannot have been reused.
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
        let reaped = tokio::time::timeout(
            cleanup.remaining().map_err(|e| e.to_string())?,
            server.wait(),
        )
        .await
        .map_err(|_| "infrastructure hang reaping etcd test server".to_owned())
        .and_then(|status| status.map_err(|e| e.to_string()));
        eprintln!("etcd server cleanup result: {reaped:?}");
        result?;
        shutdown?;
        let status = reaped?;
        if !status.success() {
            return Err(format!("server exit: {status}"));
        }
        Ok(())
    });
    if let Err(error) = result {
        panic!("{error}; retained evidence {}", cli.directory.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, ffi::OsStr};

    const CAPTURED_PAGE_GPAS: [u64; 4] = [0x0440_c000, 0x0459_1000, 0x0459_2000, 0x0459_8000];

    fn retained_attempt(state: &Path, id: &str, content: &[u8]) {
        use seedbox_application::{evidence::*, execution::*, record::*, store::ContentDigest};
        let attempt = state
            .join("sessions/session-fixture/runs")
            .join(id)
            .join("attempts/one");
        let output = attempt.join("output/machines");
        fs::create_dir_all(&output).unwrap();
        let digest = ContentDigest::of(content);
        let layout = [
            0x0440_c000u64.to_le_bytes().as_slice(),
            4096u64.to_le_bytes().as_slice(),
            0x0459_1000u64.to_le_bytes().as_slice(),
            (2 * 4096u64).to_le_bytes().as_slice(),
            0x0459_8000u64.to_le_bytes().as_slice(),
            4096u64.to_le_bytes().as_slice(),
        ]
        .concat();
        let raw_pages = [
            [content[0]; 4096],
            [content[1]; 4096],
            [content[2]; 4096],
            [content[3]; 4096],
        ];
        let ram = raw_pages
            .iter()
            .flat_map(|page| page.iter().copied())
            .collect::<Vec<_>>();
        let page_digests = raw_pages.each_ref().map(|page| ContentDigest::of(page));
        let state_report = StateReport {
            version: 1,
            machines: BTreeMap::from([(
                "oracle".into(),
                StateCapture {
                    boot: 1,
                    disposition: StateDisposition::Live,
                    digest: digest.clone(),
                    parts: BTreeMap::from([
                        ("ram".into(), ContentDigest::of(&ram)),
                        ("ram_regions".into(), ContentDigest::of(&layout)),
                    ]),
                    regions: vec![
                        RamRegion {
                            base: 0x0440_c000,
                            length: 4096,
                        },
                        RamRegion {
                            base: 0x0459_1000,
                            length: 2 * 4096,
                        },
                        RamRegion {
                            base: 0x0459_8000,
                            length: 4096,
                        },
                    ],
                    page_log: "oracle.pages".into(),
                },
            )]),
        };
        let bytes = serde_json::to_vec(&state_report).unwrap();
        fs::write(output.join(STATE_REPORT), &bytes).unwrap();
        let pages = page_digests
            .iter()
            .flat_map(|digest| {
                u64::from_str_radix(&digest.hex()[..16], 16)
                    .unwrap()
                    .to_be_bytes()
            })
            .collect::<Vec<_>>();
        let samples = [
            0u64.to_le_bytes(),
            1u64.to_le_bytes(),
            u64::from(content[0]).to_le_bytes(),
        ]
        .concat();
        fs::write(output.join("oracle.witness-log"), &samples).unwrap();
        fs::write(output.join("oracle.pages"), &pages).unwrap();
        let worker_stderr = CAPTURED_PAGE_GPAS
            .into_iter()
            .zip(raw_pages.iter())
            .zip(page_digests.iter())
            .map(|((gpa, page), digest)| {
            format!(
                "SEEDBOX_PRIVATE_PV_ORACLE_PAGE pid=7 sequence=0 seed=123 boot=1 time_ns=100 gpa={gpa:#010x} sha256={} bytes={}\n",
                digest.hex(),
                page.iter().map(|byte| format!("{byte:02x}")).collect::<String>()
            )
        })
        .collect::<String>();
        fs::write(attempt.join("output/vmm.stderr"), &worker_stderr).unwrap();
        let identity = RunIdentity {
            version: FORMAT_VERSION,
            seed: 123,
            configuration: seedbox_application::config::parse(
                "name: fixture\noracle: {image: busybox}\n",
                Path::new("/fixture/seedbox.yaml"),
                &BTreeMap::new(),
            )
            .unwrap(),
            inputs: PreparedInputs {
                machine_memory_mb: BTreeMap::from([("oracle".into(), 512)]),
                wall_clock_epoch: None,
                images: BTreeMap::new(),
                placement: BTreeMap::from([("oracle".into(), "oracle".into())]),
                guests: BTreeMap::from([("oracle".into(), ContentDigest::of(b"fixture-boot"))]),
                profile: ExecutionProfile {
                    backend: "fixture".into(),
                    witness_mode: WitnessMode::FullState,
                    semantic_parameters: Default::default(),
                },
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            },
            program: vec![],
            interventions: None,
        };
        let record = RunRecord {
            run_id: ContentDigest::of(&serde_json::to_vec(&identity).unwrap()),
            identity,
            attempt: "one".into(),
            performance_parameters: BTreeMap::new(),
            log_policy: Default::default(),
            omitted_logs: BTreeMap::new(),
            outcome: Outcome::Pass,
            report: Some(ExecutionReport {
                host_wait_ns: 0,
                simulated_time_ns: Some(100),
                reason: StopReason::Completed,
                workload_exit: Some(WorkloadExit {
                    service: "oracle".into(),
                    code: 0,
                }),
                witness: Some(digest.clone()),
                full_state: Some(BTreeMap::from([("oracle".into(), digest)])),
                faults: vec![],
                detail: None,
            }),
            error: None,
            logs: BTreeMap::from([
                (
                    format!("machines/{STATE_REPORT}"),
                    ContentDigest::of(&bytes),
                ),
                ("machines/oracle.pages".into(), ContentDigest::of(&pages)),
                (
                    "machines/oracle.witness-log".into(),
                    ContentDigest::of(&samples),
                ),
                (
                    "vmm.stderr".into(),
                    ContentDigest::of(worker_stderr.as_bytes()),
                ),
            ]),
        };
        write_record(&attempt, &record).unwrap();
    }

    fn assert_captured_pages(directory: &Path, label: &str, fills: [u8; 4]) {
        use seedbox_application::{record::inspect_bytes, store::ContentDigest};
        let manifest = fs::read(directory.join(format!("{label}-manifest.json"))).unwrap();
        let record = inspect_bytes(&manifest).unwrap();
        let vector = fs::read(directory.join(format!("{label}-oracle-pages.out"))).unwrap();
        assert_eq!(vector.len(), 32);
        assert_eq!(
            record.logs.get("machines/oracle.pages"),
            Some(&ContentDigest::of(&vector))
        );
        let stderr = fs::read_to_string(directory.join(format!("{label}-vmm.stderr"))).unwrap();
        let markers = stderr
            .lines()
            .filter(|line| line.starts_with("SEEDBOX_PRIVATE_PV_ORACLE_PAGE "))
            .collect::<Vec<_>>();
        assert_eq!(markers.len(), 4);
        for (index, (line, gpa, fill)) in markers
            .into_iter()
            .zip(CAPTURED_PAGE_GPAS)
            .zip(fills)
            .map(|((line, gpa), fill)| (line, gpa, fill))
            .enumerate()
        {
            assert!(line.contains(&format!("gpa={gpa:#010x}")), "{line}");
            let hash = line
                .split_whitespace()
                .find_map(|word| word.strip_prefix("sha256="))
                .unwrap();
            let encoded = line
                .split_whitespace()
                .find_map(|word| word.strip_prefix("bytes="))
                .unwrap();
            assert_eq!(encoded.len(), 8192);
            let (pairs, remainder) = encoded.as_bytes().as_chunks::<2>();
            assert!(remainder.is_empty());
            let page = pairs
                .iter()
                .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(page, vec![fill; 4096]);
            let digest = ContentDigest::of(&page);
            assert_eq!(hash, digest.hex());
            assert_eq!(
                &vector[index * 8..(index + 1) * 8],
                &u64::from_str_radix(&hash[..16], 16).unwrap().to_be_bytes()
            );
        }
    }

    #[tokio::test]
    async fn replay_nonzero_failure_captures_retained_comparison_before_cleanup() {
        let temporary = tempfile::tempdir().unwrap();
        let state = temporary.path().join("state");
        fs::create_dir_all(&state).unwrap();
        retained_attempt(&state, "awqwjt9m", b"baseline");
        retained_attempt(&state, "xmevbn3p", b"replay");
        fs::write(state.join("comparison.json"), br#"{
            "baseline":"awqwjt9m", "simulation":"xmevbn3p", "status":"MISMATCH",
            "outcomeMatches":true, "witnessMatches":false, "fullStateMatches":false, "faultHistoryMatches":true
        }"#).unwrap();
        let executable = temporary.path().join("cli");
        seedbox_test::process::install_executable(
            &executable,
            br#"#!/bin/sh
case "$1" in
replay) cat "$SEEDBOX_STATE/comparison.json"; exit 1;;
logs) test -d "$SEEDBOX_STATE/sessions" || exit 9
      printf 'RESULT pass despite witness mismatch\n';;
*) exit 8;;
esac
"#,
            0o755,
        )
        .unwrap();
        let cli = Cli {
            executable,
            directory: temporary.path().into(),
            state: state.clone(),
        };
        let guard = HangGuard::new("synthetic replay observation", Duration::from_secs(30));
        let error = cli.replay(&guard, "awqwjt9m").await.unwrap_err();
        // Remove the actual fixture attempts at the same boundary as scenario cleanup.
        fs::remove_dir_all(state).unwrap();
        assert!(error.starts_with("CLI replay: exit status: 1;"), "{error}");
        let failure = temporary.path().join("failure");
        for name in [
            "baseline-manifest.json",
            "replay-manifest.json",
            "baseline-driver-state.json",
            "replay-driver-state.json",
            "compare-state.json",
            "compare-ram-pages.json",
            "baseline-vmm.stderr",
            "replay-vmm.stderr",
        ] {
            assert!(failure.join(name).is_file(), "{name}");
        }
        for name in [
            "baseline-manifest.json",
            "replay-manifest.json",
            "baseline-driver-state.json",
            "replay-driver-state.json",
            "compare-state.json",
            "compare-ram-pages.json",
        ] {
            let bytes = fs::read(failure.join(name)).unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap();
        }
        assert_captured_pages(&failure, "baseline", *b"base");
        assert_captured_pages(&failure, "replay", *b"repl");
        assert!(fs::read_to_string(failure.join("replay-oracle-log.stdout"))
            .unwrap()
            .contains("RESULT pass"));
        let pages: serde_json::Value =
            serde_json::from_slice(&fs::read(failure.join("compare-ram-pages.json")).unwrap())
                .unwrap();
        assert_eq!(pages["machines"]["oracle"]["differing_pfns"], 4);
        let summary = fs::read_to_string(failure.join("summary.txt")).unwrap();
        assert!(summary.contains(&error));
        assert!(!summary.contains("capture failed"), "{summary}");
    }

    #[tokio::test]
    async fn replay_capture_errors_do_not_replace_the_original_failure() {
        let temporary = tempfile::tempdir().unwrap();
        let executable = temporary.path().join("cli");
        seedbox_test::process::install_executable(&executable, br#"#!/bin/sh
case "$1" in
replay) printf '{"baseline":"awqwjt9m","simulation":"xmevbn3p","status":"MISMATCH","outcomeMatches":true,"witnessMatches":false,"fullStateMatches":false,"faultHistoryMatches":true}'; exit 1;;
logs) printf 'oracle observation denied\n' >&2; exit 7;;
esac
"#, 0o755).unwrap();
        let cli = Cli {
            executable,
            directory: temporary.path().into(),
            state: temporary.path().join("absent"),
        };
        let guard = HangGuard::new("synthetic failed observation", Duration::from_secs(30));
        let error = cli.replay(&guard, "awqwjt9m").await.unwrap_err();
        assert!(error.starts_with("CLI replay: exit status: 1;"), "{error}");
        let summary = fs::read_to_string(temporary.path().join("failure/summary.txt")).unwrap();
        assert!(summary.contains(&error));
        assert!(summary.contains("comparison diagnostic capture failed"));
        assert!(summary.contains("exit status: 7"));
        assert!(
            fs::read_to_string(temporary.path().join("failure/replay-oracle-log.stderr"))
                .unwrap()
                .contains("denied")
        );
    }

    #[test]
    fn comparison_capture_copies_state_from_retained_machine_output() {
        use seedbox_application::{evidence::*, store::ContentDigest};
        let temporary = tempfile::tempdir().unwrap();
        let attempt = temporary.path().join("attempt");
        let output = attempt.join("output/machines");
        fs::create_dir_all(&output).unwrap();
        let layout = [0u64.to_le_bytes(), 4096u64.to_le_bytes()].concat();
        let report = StateReport {
            version: 1,
            machines: BTreeMap::from([(
                "oracle".into(),
                StateCapture {
                    boot: 1,
                    disposition: StateDisposition::Live,
                    digest: ContentDigest::of(b"stopped-state"),
                    parts: BTreeMap::from([
                        ("ram".into(), ContentDigest::of(b"ram")),
                        ("ram_regions".into(), ContentDigest::of(&layout)),
                    ]),
                    regions: vec![RamRegion {
                        base: 0,
                        length: 4096,
                    }],
                    page_log: "machines/state-0.pages".into(),
                },
            )]),
        };
        let bytes = serde_json::to_vec(&report).unwrap();
        fs::write(output.join(STATE_REPORT), &bytes).unwrap();
        // Page hashes and VM RAM/disks are unnecessary for metadata capture.
        let logs = BTreeMap::from([(
            format!("machines/{STATE_REPORT}"),
            ContentDigest::of(&bytes),
        )]);
        let destination = temporary.path().join("baseline-driver-state.json");
        copy_state_report(&attempt, &destination, &logs).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), bytes);
        assert_eq!(
            serde_json::from_slice::<StateReport>(&fs::read(destination).unwrap()).unwrap(),
            report
        );
    }

    #[test]
    fn diagnostic_files_preserve_bytes_and_reject_oversized_evidence() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("evidence.json");
        fs::write(&path, b"retained evidence").unwrap();
        assert_eq!(diagnostic_bytes(&path).unwrap(), b"retained evidence");
        fs::File::create(&path)
            .unwrap()
            .set_len(DIAGNOSTIC_LIMIT + 1)
            .unwrap();
        assert!(diagnostic_bytes(&path)
            .unwrap_err()
            .contains("exceeds 8 MiB"));
        assert!(diagnostic_bytes(temporary.path()).is_err());
    }

    #[test]
    fn failure_resources_parse_current_cli_summary_array() {
        let bytes = serde_json::to_vec(&serde_json::json!([{
            "metadata": {
                "id": "23456789",
                "revision": 1,
                "createdAtUnixMs": 0
            },
            "status": seedbox_api::SimulationStatus::default(),
            "oracle": "oracle"
        }]))
        .unwrap();
        let resources = parse_resource_summaries(&bytes).unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].metadata.id.as_str(), "23456789");
        assert!(parse_resource_summaries(b"[]").unwrap().is_empty());
        assert!(parse_resource_summaries(b"{}").is_err());
    }

    #[test]
    fn normal_cli_command_clears_legacy_bootstrap_overrides() {
        let cli = Cli {
            executable: "unused-cli".into(),
            directory: "unused-reports".into(),
            state: "owned-state".into(),
        };
        let command = cli.command();
        let environment: BTreeMap<OsString, Option<OsString>> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_os_string)))
            .collect();
        for key in [
            "SEEDBOX_VMM",
            "SEEDBOX_KERNEL",
            "SEEDBOX_RUNTIME_ASSETS",
            "SEEDBOX_GUEST_INIT",
            "SEEDBOX_MKE2FS",
            "SEEDBOX_ASSET_DIR",
        ] {
            assert_eq!(environment.get(OsStr::new(key)), Some(&None), "{key}");
        }
        assert_eq!(
            environment.get(OsStr::new("SEEDBOX_STATE")),
            Some(&Some(cli.state.into_os_string()))
        );
        assert!(
            !environment.contains_key(OsStr::new("DOCKER_HOST")),
            "preserve the caller's engine connection"
        );
    }
}

// The owned subprocess receives explicit paths and IDs. Normal discovery
// exercises the missing-session diagnostic against a private empty fixture.
#[test]
fn capture_comparison_worker() {
    let Some(directory) = std::env::var_os("SEEDBOX_CAPTURE_DIRECTORY") else {
        let temporary = tempfile::tempdir().unwrap();
        let cli = Cli {
            executable: PathBuf::new(),
            directory: temporary.path().to_owned(),
            state: temporary.path().join("absent"),
        };
        cli.capture_comparison_files("kmm5wkwf", "23456789", "repeat");
        let summary = fs::read_to_string(temporary.path().join("failure/summary.txt")).unwrap();
        assert!(
            summary.contains("comparison kmm5wkwf / 23456789"),
            "{summary}"
        );
        assert!(
            summary.contains("comparison diagnostic capture failed"),
            "{summary}"
        );
        return;
    };
    let cli = Cli {
        executable: PathBuf::new(),
        directory: directory.into(),
        state: std::env::var_os("SEEDBOX_CAPTURE_STATE")
            .expect("capture state")
            .into(),
    };
    cli.capture_comparison_files(
        &std::env::var("SEEDBOX_CAPTURE_BASELINE").expect("capture baseline"),
        &std::env::var("SEEDBOX_CAPTURE_REPEATED").expect("capture repeat"),
        &std::env::var("SEEDBOX_CAPTURE_OTHER").expect("capture side"),
    );
}

fn bounded_diagnostics(scenario: &HangGuard, diagnostics: &HangGuard) -> Result<HangGuard> {
    let remaining = scenario
        .remaining()
        .map_err(|e| e.to_string())?
        .min(diagnostics.remaining().map_err(|e| e.to_string())?);
    Ok(HangGuard::new("etcd failure diagnostics", remaining))
}

#[tokio::test]
async fn comparison_capture_cancels_a_wedged_retained_reader() {
    use std::{os::fd::AsRawFd, os::unix::fs::OpenOptionsExt};
    let temporary = tempfile::tempdir().unwrap();
    let attempt = temporary
        .path()
        .join("state/sessions/fixture/runs/kmm5wkwf/attempts/fixture");
    fs::create_dir_all(&attempt).unwrap();
    let manifest = attempt.join("manifest.json");
    let name = std::ffi::CString::new(manifest.as_os_str().as_encoded_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let cli = Cli {
        executable: PathBuf::new(),
        directory: temporary.path().to_owned(),
        state: temporary.path().join("state"),
    };
    // The writer opens only after the actual offline worker opens its reader.
    // Holding it without sending bytes keeps that read pending until cancelled.
    let writer = tokio::task::spawn_blocking(move || {
        HangGuard::new(
            "retained manifest reader readiness",
            Duration::from_secs(60),
        )
        .wait_for(|| {
            match fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&manifest)
            {
                Ok(file) => Some(file),
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => None,
                Err(error) => panic!("manifest readiness: {error}"),
            }
        })
        .unwrap()
    });
    let scenario = HangGuard::new("wedged offline diagnostic fixture", Duration::from_secs(60));
    let diagnostics = failure_capture::budget();
    let held_writer = {
        let capture = cli.capture_comparison_failure(
            &scenario,
            &diagnostics,
            "kmm5wkwf",
            "23456789",
            "repeat",
        );
        tokio::pin!(capture);
        tokio::select! {
            _ = &mut capture => panic!("comparison returned before its retained reader was ready"),
            writer = writer => writer.unwrap(),
        }
        // Cancel only after the actual offline reader has opened the FIFO.
    };
    tokio::task::spawn_blocking(move || {
        HangGuard::new("cancelled retained reader cleanup", Duration::from_secs(60))
            .wait_for(|| {
                let mut fd = libc::pollfd {
                    fd: held_writer.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                assert!(unsafe { libc::poll(&mut fd, 1, 0) } >= 0);
                // POLLERR on the FIFO writer positively reports that its only
                // reader closed, rather than inferring cleanup from absence.
                (fd.revents & libc::POLLERR != 0).then_some(())
            })
            .unwrap();
    })
    .await
    .unwrap();
    let summary = fs::read_to_string(temporary.path().join("failure/summary.txt")).unwrap();
    assert!(
        summary.contains("comparison kmm5wkwf / 23456789"),
        "{summary}"
    );
}
