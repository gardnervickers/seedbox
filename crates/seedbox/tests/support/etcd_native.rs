//! Fast preparation smoke through the normal CLI/service, not bd-207 acceptance.
//! Canonical acceptance must run examples/etcd unchanged. No BackendProvider,
//! prepared guest manifest, substituted oracle or image. TCG stays unscheduled
//! until its real user route lands.
use super::etcd_project::*;
use seedbox_api::{InputSource, Simulation, SimulationComparison};
use seedbox_test_time::HangGuard;
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
};

type Result<T> = std::result::Result<T, String>;

struct Cli {
    executable: PathBuf,
    directory: PathBuf,
    state: PathBuf,
}
impl Cli {
    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        // Keep normal CLI runtime-path and engine resolution. Clear the known
        // legacy VMM override, as the existing application acceptance does.
        // The explicit selector and retained profile check prohibit a silent
        // legacy/PV fallback without discarding legitimate TCG asset paths.
        command
            .env_remove("SEEDBOX_VMM")
            .env("SEEDBOX_STATE", &self.state)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        command
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "test invokes an already-built release CLI; no executable is written while acceptance runs"
    )]
    async fn invoke(&self, guard: &HangGuard, label: &str, args: &[OsString]) -> Result<Vec<u8>> {
        eprintln!("etcd CLI {label}: {args:?}");
        let stdout = self.directory.join(format!("{label}.stdout"));
        let stderr = self.directory.join(format!("{label}.stderr"));
        let mut child = self
            .command()
            .args(args)
            .stdout(fs::File::create(&stdout).map_err(|e| e.to_string())?)
            .stderr(fs::File::create(&stderr).map_err(|e| e.to_string())?)
            .spawn()
            .map_err(|e| format!("spawn CLI {label}: {e}"))?;
        // test-time: allow(hang-guard) -- Bound a positive CLI reply; expiration is infrastructure failure, never an oracle verdict.
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

    async fn describe(&self, guard: &HangGuard, label: &str, id: &str) -> Result<Simulation> {
        let bytes = self
            .invoke(
                guard,
                label,
                &words(&["describe", "simulation", id, "--json"]),
            )
            .await?;
        serde_json::from_slice(&bytes).map_err(|e| format!("{label}: {e}"))
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
        self.invoke(guard, &format!("{label}-resume"), &words(&["resume", &id]))
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
                .get("data")
                .is_some_and(|volume| volume.target == "/data" && volume.size_bytes > 0)
        {
            return Err(format!("{name} did not retain its persistent /data disk"));
        }
    }
    Ok(())
}

#[allow(
    clippy::disallowed_methods,
    reason = "test starts an already-built release server; no executable is written concurrently"
)]
async fn start_server(cli: &Cli, guard: &HangGuard) -> Result<Child> {
    let mut child = cli
        .command()
        .args(["serve", "--startup-handshake", "--full-state"])
        .stdout(Stdio::piped())
        .stderr(fs::File::create(cli.directory.join("server.stderr")).map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut ready = String::new();
    let mut reader = BufReader::new(child.stdout.take().ok_or("missing startup pipe")?);
    // test-time: allow(hang-guard) -- Bound the positive server startup handshake, with no sleep or timed absence used for ordering.
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
    let compared: SimulationComparison = serde_json::from_slice(
        &cli.invoke(
            guard,
            "compare-repeat",
            &words(&["compare", &baseline_id, &repeated_id, "--json"]),
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    if compared.baseline != baseline.metadata.id
        || compared.simulation != repeated.metadata.id
        || baseline.metadata.id == repeated.metadata.id
    {
        return Err(
            "repeat comparison did not compare two independently submitted simulations".into(),
        );
    }
    require_match(&compared)?;
    // Reports are already retained; bound memory by releasing completed native
    // worlds rather than accumulating five four-machine executions.
    cli.invoke(
        guard,
        "delete-repeat",
        &words(&["delete", "simulation", &repeated_id]),
    )
    .await?;
    // No new backend selection on replay: the retained runtime must own it.
    let replay: SimulationComparison = serde_json::from_slice(
        &cli.invoke(
            guard,
            "replay",
            &words(&["replay", &baseline_id, "--json", "--wall-time", "20m"]),
        )
        .await?,
    )
    .map_err(|e| e.to_string())?;
    if replay.baseline != baseline.metadata.id
        || replay.simulation == baseline.metadata.id
        || replay.simulation == repeated.metadata.id
    {
        return Err("replay did not create/compare a fresh simulation from the original".into());
    }
    require_match(&replay)?;
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
        &words(&["delete", "simulation", replay.simulation.as_str()]),
    )
    .await?;
    cli.invoke(
        guard,
        "delete-baseline",
        &words(&["delete", "simulation", &baseline_id]),
    )
    .await?;
    for seed in EXTRA_SEEDS {
        let completed = cli
            .fresh(guard, backend, project, seed, &format!("seed-{seed}"))
            .await?;
        cli.invoke(
            guard,
            &format!("delete-seed-{seed}"),
            &words(&["delete", "simulation", completed.metadata.id.as_str()]),
        )
        .await?;
    }
    // Diagnostics only; never assert a host speed or compare timings as evidence.
    eprintln!(
        "etcd {} seeds123(repeat+replay)/124/125 PASS; witness/full-state MATCH",
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
        let shutdown = cli
            .invoke(&cleanup, "shutdown", &words(&["shutdown"]))
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
        // test-time: allow(hang-guard) -- Bound positive exit of our owned server after graceful shutdown, including failure cleanup.
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
