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

    /// Retain terminal errors while the catalogue still exists. Observation
    /// failure is diagnostic only and never replaces the original case failure.
    async fn capture_failure_resources(&self, guard: &HangGuard) -> Result<()> {
        let bytes = self
            .invoke(
                guard,
                "failure-resources",
                &words(&["get", "simulations", "--json"]),
            )
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

    /// Copy only small retained execution metadata and per-page hashes before
    /// shutdown removes the session. Never retain native RAM or disk images.
    fn capture_failure_evidence(&self) -> Result<()> {
        const FILE_LIMIT: u64 = 16 * 1024 * 1024;
        const TOTAL_LIMIT: u64 = 64 * 1024 * 1024;
        let root = self.state.join("sessions");
        if !root.exists() {
            return Ok(());
        }
        let destination = self.directory.join("failure-evidence");
        let mut pending = vec![root.clone()];
        let mut total = 0u64;
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).map_err(|e| e.to_string())? {
                let entry = entry.map_err(|e| e.to_string())?;
                let kind = entry.file_type().map_err(|e| e.to_string())?;
                if kind.is_dir() {
                    pending.push(entry.path());
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let name = name.to_string_lossy();
                let page_hashes = name
                    .strip_prefix("state-")
                    .and_then(|name| name.strip_suffix(".pages"))
                    .is_some_and(|ordinal| {
                        !ordinal.is_empty() && ordinal.bytes().all(|b| b.is_ascii_digit())
                    });
                if !matches!(
                    name.as_ref(),
                    "driver-state.json"
                        | "state-report.json"
                        | "driver-policy.json"
                        | "manifest.json"
                        | "vmm.stderr"
                ) && !page_hashes
                {
                    continue;
                }
                let path = entry.path();
                let mut bytes = Vec::new();
                fs::File::open(&path)
                    .map_err(|e| e.to_string())?
                    .take(FILE_LIMIT + 1)
                    .read_to_end(&mut bytes)
                    .map_err(|e| e.to_string())?;
                if bytes.len() as u64 > FILE_LIMIT || total + bytes.len() as u64 > TOTAL_LIMIT {
                    eprintln!(
                        "omitting diagnostic {}: retained evidence size limit",
                        path.display()
                    );
                    continue;
                }
                let relative = path.strip_prefix(&root).map_err(|e| e.to_string())?;
                let target = destination.join(relative);
                fs::create_dir_all(target.parent().ok_or("missing evidence parent")?)
                    .map_err(|e| e.to_string())?;
                fs::write(&target, &bytes).map_err(|e| e.to_string())?;
                total += bytes.len() as u64;
                eprintln!("retained diagnostic {}", target.display());
            }
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
        .args(["serve", "--startup-handshake", "--full-state"])
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
    if std::env::var_os("SEEDBOX_DIAG_COMPARE_ONLY").is_some() {
        cli.capture_failure_evidence()?;
        return Ok(());
    }
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
        if result.is_err() {
            if let Err(error) = cli.capture_failure_evidence() {
                eprintln!("execution diagnostic capture failed (original error retained): {error}");
            }
            if let Err(error) = cli.capture_failure_resources(&cleanup).await {
                eprintln!("resource diagnostic capture failed (original error retained): {error}");
            }
        }
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
