//! Discover native workloads; the public CLI and guest checks own each verdict.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
use std::{
    fs,
    io::{BufRead, BufReader},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn invoke(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    args: &[&str],
) -> Result<String, String> {
    let logfile = directory.join(format!("{name}.log"));
    let log = fs::File::create(&logfile).map_err(|error| error.to_string())?;
    eprintln!("application: {} / {name}", directory.display());
    let mut command = Command::new(cli);
    command
        .args(args)
        .env("SEEDBOX_STATE", state)
        .stdout(log.try_clone().map_err(|error| error.to_string())?)
        .stderr(log);
    let status = command
        .status()
        .map_err(|error| format!("{}: {error}", cli.display()))?;
    if !status.success() {
        // CI keeps only the console, so show the end of the failing log there.
        let text = fs::read(&logfile).map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
        if let Ok(text) = &text {
            let lines: Vec<_> = text.lines().collect();
            let tail = &lines[lines.len().saturating_sub(80)..];
            eprintln!(
                "--- {name} failed; last {} lines of {}:",
                tail.len(),
                logfile.display()
            );
            for line in tail {
                eprintln!("{line}");
            }
            eprintln!("--- end of {name}");
        }
        return Err(format!("{name}: {status}; see {}", logfile.display()));
    }
    Ok(
        String::from_utf8_lossy(&fs::read(&logfile).map_err(|error| error.to_string())?)
            .into_owned(),
    )
}

fn simulation_id(output: &str) -> Result<&str, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("PASS: "))
        .ok_or_else(|| format!("missing PASS simulation ID: {output}"))
}

fn require_full_state_match(output: &str) -> Result<(), String> {
    let comparison: seedbox_api::SimulationComparison =
        serde_json::from_str(output).map_err(|error| format!("comparison: {error}"))?;
    if comparison.full_state_matches != Some(true) {
        return Err(format!("missing full-state MATCH: {output}"));
    }
    Ok(())
}

// Capture both sides while the session is still alive. The CI artifact collector
// bounds these files; shutdown would otherwise remove the evidence we need.
fn compare(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    baseline: &str,
    repeated: &str,
) -> Result<(), String> {
    let result = invoke(
        cli,
        directory,
        state,
        name,
        &["compare", baseline, repeated, "--json"],
    )
    .and_then(|output| require_full_state_match(&output));
    if result.is_err() {
        for (side, id) in [("baseline", baseline), ("repeated", repeated)] {
            for (kind, args) in [
                ("definition", vec!["describe", "simulation", id, "--json"]),
                ("world", vec!["get", "world", id, "--json"]),
                ("timeline", vec!["get", "timeline", id, "--json"]),
                ("logs", vec!["logs", id, "--tail", "100"]),
            ] {
                if let Err(error) = invoke(
                    cli,
                    directory,
                    state,
                    &format!("{name}-{side}-{kind}"),
                    &args,
                ) {
                    eprintln!("comparison diagnostic: {error}");
                }
            }
        }
    }
    result
}

#[test]
fn every_application_runs_twice_and_reruns_through_the_server() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workloads =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut workloads: Vec<_> = fs::read_dir(workloads)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
        })
        .collect();
    workloads.sort();
    assert!(!workloads.is_empty());
    // macOS limits Unix socket paths to 104 bytes. The checkout/report path
    // can exceed that before appending the socket name.
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-apps-")
        .tempdir_in("/tmp")
        .unwrap();
    // Let the daemon create its own state directory with private permissions.
    let state_path = state_directory.path().join("state");
    let state = state_path.as_path();
    let server = match Server::start(Path::new(&cli), state) {
        Ok(server) => server,
        Err(error) => {
            let retained = state_directory.keep();
            panic!("{error}; retained server state: {}", retained.display());
        }
    };
    let mut failures = Vec::new();
    for workload in workloads {
        let directory = Path::new(&reports)
            .join("applications")
            .join(workload.file_stem().unwrap());
        fs::create_dir_all(&directory).unwrap();
        let check = || -> Result<(), String> {
            let first_output = invoke(
                Path::new(&cli),
                &directory,
                state,
                "first",
                &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
            )?;
            let first = simulation_id(&first_output)?;
            let described = invoke(
                Path::new(&cli),
                &directory,
                state,
                "describe-first",
                &["describe", "simulation", first, "--json"],
            )?;
            let resource: seedbox_api::Simulation = serde_json::from_str(&described)
                .map_err(|error| format!("describe first: {error}"))?;
            let seed = resource
                .spec
                .seed
                .ok_or("first simulation has no seed")?
                .to_string();
            let second_output = invoke(
                Path::new(&cli),
                &directory,
                state,
                "second",
                &[
                    "run",
                    workload.to_str().unwrap(),
                    "--seed",
                    &seed,
                    "--wall-time",
                    "30m",
                ],
            )?;
            let second = simulation_id(&second_output)?;
            compare(Path::new(&cli), &directory, state, "compare", first, second)?;
            let repeated = invoke(
                Path::new(&cli),
                &directory,
                state,
                "rerun",
                &["rerun", first, "--wall-time", "30m"],
            )?;
            compare(
                Path::new(&cli),
                &directory,
                state,
                "compare-rerun",
                first,
                simulation_id(&repeated)?,
            )?;
            Ok(())
        };
        if let Err(error) = check() {
            failures.push(format!("{}: {error}", workload.display()));
        }
    }
    drop(server);
    if !failures.is_empty() {
        let retained = state_directory.keep();
        panic!(
            "{}\nretained server state: {}",
            failures.join("\n"),
            retained.display()
        );
    }
}

/// The CLI's `--json` resource for a submitted simulation.
fn submitted(output: &str) -> Result<seedbox_api::Simulation, String> {
    serde_json::from_str(output).map_err(|error| format!("simulation JSON: {error}: {output}"))
}

/// The one line of the oracle's output starting with `prefix`.
fn oracle_line(
    cli: &Path,
    directory: &Path,
    state: &Path,
    name: &str,
    id: &str,
    prefix: &str,
) -> Result<String, String> {
    let logs = invoke(cli, directory, state, name, &["logs", id, "oracle"])?;
    let mut lines = logs.lines().filter(|line| line.starts_with(prefix));
    match (lines.next(), lines.next()) {
        (Some(line), None) => Ok(line.to_owned()),
        _ => Err(format!("{id}: expected one {prefix:?} oracle line")),
    }
}

/// Send one playback command and wait for the worker's acknowledgement.
async fn acknowledged(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
    command: seedbox_api::ControlCommand,
) -> Result<seedbox_api::ControlReceipt, String> {
    let request = seedbox_api::ControlRequest {
        request_id: seedbox_client::Client::request_id().map_err(|e| e.to_string())?,
        command,
    };
    let mut receipt = client
        .request_control(id, &request)
        .await
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while receipt.state == seedbox_api::ControlState::Accepted {
        if Instant::now() > deadline {
            return Err(format!("{:?} was not acknowledged", request.command));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        receipt = client
            .control_receipt(id, &request.request_id)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(receipt)
}

/// Run until the oracle's first fault effect, step through it, and capture there.
/// Each checkpoint workload marks its capture phase with a nominal-speed throttle,
/// so the capture lands at the same committed boundary on every run.
async fn capture_at_marked_phase(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
) -> Result<(String, u64), String> {
    use seedbox_api::{CheckpointState, ControlCommand, ControlState, PlaybackRate};
    use seedbox_engine_protocol::host_control::ControlError;
    let deadline = Instant::now() + Duration::from_secs(600);
    while client.playback(id).await.is_err() {
        if Instant::now() > deadline {
            return Err("no live playback endpoint".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    acknowledged(
        client,
        id,
        ControlCommand::PauseBeforeNextFault {
            rate: PlaybackRate::Unlimited,
        },
    )
    .await?;
    // Held before the throttle effect: paused and no longer moving.
    let mut last = None;
    loop {
        let observation = client.playback(id).await.map_err(|e| e.to_string())?;
        if observation.state.terminal {
            return Err("run completed before its marked capture phase".into());
        }
        if observation.state.rate == PlaybackRate::Paused
            && last == Some(observation.state.position)
        {
            break;
        }
        last = Some(observation.state.position);
        if Instant::now() > deadline {
            return Err("run did not pause before its marked phase".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    for _ in 0..16 {
        acknowledged(client, id, ControlCommand::Step).await?;
        while client
            .playback(id)
            .await
            .map_err(|e| e.to_string())?
            .state
            .stepping
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let receipt = acknowledged(client, id, ControlCommand::Checkpoint).await?;
        match receipt.state {
            // Still before the effect, or I/O cost outstanding: take another round.
            ControlState::Acknowledged {
                error: Some(ControlError::Unsupported | ControlError::InvalidCheckpoint),
                ..
            } => continue,
            ControlState::Acknowledged { error: None, .. } => {}
            state => return Err(format!("checkpoint request failed: {state:?}")),
        }
        let name = receipt.request.request_id;
        loop {
            let receipt = client
                .checkpoint(id, &name)
                .await
                .map_err(|e| e.to_string())?;
            if let ControlState::Acknowledged {
                checkpoint: Some(checkpoint),
                ..
            } = &receipt.state
            {
                if checkpoint.state == CheckpointState::Captured {
                    acknowledged(
                        client,
                        id,
                        ControlCommand::SetRate {
                            rate: PlaybackRate::Unlimited,
                        },
                    )
                    .await?;
                    return Ok((name, checkpoint.position.simulated_time_ns));
                }
            }
            if Instant::now() > deadline {
                return Err(format!("checkpoint {name} was not captured"));
            }
            // A pending capture needs a boundary with no outstanding I/O cost.
            acknowledged(client, id, ControlCommand::Step).await?;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    Err("no checkpoint could be requested after the marked fault".into())
}

/// Capture each `checkpoint-*` workload at its marked phase, finish the source
/// run, then branch twice. The branches must pass, match each other, replay, and
/// print the same continuation evidence as the source after the capture point.
fn checkpoint_branches(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let source = submitted(&invoke(
        cli,
        directory,
        state,
        "source",
        &[
            "run",
            workload.to_str().unwrap(),
            "--paused",
            "--detach",
            "--json",
            "--wall-time",
            "30m",
        ],
    )?)?;
    let id = source.metadata.id.clone();
    let client = seedbox_client::Client::connect_to(&state.join("server.sock"))
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let (checkpoint, captured_ns) = runtime.block_on(capture_at_marked_phase(&client, &id))?;
    invoke(
        cli,
        directory,
        state,
        "source-wait",
        &["wait", id.as_str(), "--for", "completed"],
    )?;
    let finished = runtime
        .block_on(client.describe(&id))
        .map_err(|e| e.to_string())?;
    if finished.status.verdict() != seedbox_api::TestVerdict::Passed {
        return Err(format!("source {id} did not pass: {:?}", finished.status));
    }
    let marked = oracle_line(
        cli,
        directory,
        state,
        "source-marker",
        id.as_str(),
        "CAPTURE_PHASE ",
    )?;
    let marked_ns: u64 = marked
        .split_whitespace()
        .find_map(|field| field.strip_prefix("at_ns="))
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("malformed marker: {marked}"))?;
    eprintln!("checkpoint: {checkpoint} at {captured_ns} ns; marked phase at {marked_ns} ns");
    // The capture follows the marker's effect within one bounded step.
    if !(marked_ns..marked_ns + 1_000_000_000).contains(&captured_ns) {
        return Err(format!(
            "checkpoint at {captured_ns} ns is not at the marked phase ({marked_ns} ns)"
        ));
    }
    let expected = oracle_line(
        cli,
        directory,
        state,
        "source-evidence",
        id.as_str(),
        "CONTINUATION ",
    )?;
    let mut branches = Vec::new();
    for name in ["branch-a", "branch-b"] {
        let output = invoke(
            cli,
            directory,
            state,
            name,
            &["branch", id.as_str(), &checkpoint, "--wall-time", "30m"],
        )?;
        let branch = simulation_id(&output)?.to_owned();
        let evidence = oracle_line(
            cli,
            directory,
            state,
            &format!("{name}-evidence"),
            &branch,
            "CONTINUATION ",
        )?;
        if evidence != expected {
            return Err(format!(
                "{name} {branch} continuation differs from source {id}: {evidence} != {expected}"
            ));
        }
        branches.push(branch);
    }
    compare(
        cli,
        directory,
        state,
        "compare-branches",
        &branches[0],
        &branches[1],
    )?;
    invoke(
        cli,
        directory,
        state,
        "replay-branch",
        &["replay", &branches[0], "--wall-time", "30m"],
    )?;
    Ok(())
}

/// Run durability.yaml under the torn-write disk model, which only a resolved
/// definition selects: derive it from a fresh run's definition, switch the
/// model and the oracle's policy, then require PASS and a replay MATCH.
fn torn_writes(cli: &Path, directory: &Path, state: &Path, workload: &Path) -> Result<(), String> {
    let drop = invoke(
        cli,
        directory,
        state,
        "drop",
        &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let described = invoke(
        cli,
        directory,
        state,
        "describe-drop",
        &["describe", "simulation", simulation_id(&drop)?, "--json"],
    )?;
    let mut definition: serde_json::Value =
        serde_json::from_str(&described).map_err(|error| format!("describe: {error}"))?;
    definition
        .as_object_mut()
        .ok_or("definition is not an object")?
        .remove("status");
    definition["spec"]["simulator"]["disk"]["torn_writes"] = true.into();
    let argv = definition["spec"]["oracle"]["process"]["argv"]
        .as_array_mut()
        .ok_or("oracle argv missing")?;
    if argv.last().and_then(|policy| policy.as_str()) != Some("drop") {
        return Err(format!("unexpected oracle argv {argv:?}"));
    }
    *argv.last_mut().unwrap() = "torn".into();
    let path = directory.join("torn.json");
    fs::write(&path, definition.to_string()).map_err(|error| error.to_string())?;
    let torn = invoke(
        cli,
        directory,
        state,
        "torn",
        &["run", path.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let torn = simulation_id(&torn)?;
    let result = oracle_line(cli, directory, state, "torn-result", torn, "RESULT ")?;
    if !result.contains(" policy=torn ") {
        return Err(format!("torn run reported {result}"));
    }
    invoke(
        cli,
        directory,
        state,
        "replay-torn",
        &["replay", torn, "--wall-time", "30m"],
    )?;
    Ok(())
}

/// Simulations derived from a workload run rather than submitted from its file:
/// checkpoint branches of every `checkpoint-*` workload, and the durability
/// workload under torn writes. They share one server and run one at a time.
#[test]
fn derived_simulations_pass_and_replay() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workloads =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut checkpoints: Vec<_> = fs::read_dir(&workloads)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "yaml")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("checkpoint-"))
        })
        .collect();
    checkpoints.sort();
    assert!(!checkpoints.is_empty());
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-derived-")
        .tempdir_in("/tmp")
        .unwrap();
    let state_path = state_directory.path().join("state");
    let state = state_path.as_path();
    let server = match Server::start(Path::new(&cli), state) {
        Ok(server) => server,
        Err(error) => {
            let retained = state_directory.keep();
            panic!("{error}; retained server state: {}", retained.display());
        }
    };
    let mut failures = Vec::new();
    let directory = |name: &str| {
        let directory = Path::new(&reports).join("derived").join(name);
        fs::create_dir_all(&directory).unwrap();
        directory
    };
    for workload in checkpoints {
        let name = workload.file_stem().unwrap().to_str().unwrap();
        if let Err(error) = checkpoint_branches(Path::new(&cli), &directory(name), state, &workload)
        {
            failures.push(format!("{}: {error}", workload.display()));
        }
    }
    let durability = workloads.join("durability.yaml");
    if let Err(error) = torn_writes(
        Path::new(&cli),
        &directory("durability-torn"),
        state,
        &durability,
    ) {
        failures.push(format!("{} (torn writes): {error}", durability.display()));
    }
    drop(server);
    if !failures.is_empty() {
        let retained = state_directory.keep();
        panic!(
            "{}\nretained server state: {}",
            failures.join("\n"),
            retained.display()
        );
    }
}

/// The acceptance fixture owns its server process; retained reports outlive it.
struct Server(Child);
impl Server {
    fn start(cli: &Path, state: &Path) -> Result<Self, String> {
        let mut server = Self(
            Command::new(cli)
                .args(["serve", "--startup-handshake", "--full-state"])
                .env("SEEDBOX_STATE", state)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?,
        );
        let mut ready = String::new();
        BufReader::new(
            server
                .0
                .stdout
                .take()
                .ok_or("server startup pipe missing")?,
        )
        .read_line(&mut ready)
        .map_err(|e| e.to_string())?;
        let ready: serde_json::Value = serde_json::from_str(&ready).map_err(|e| e.to_string())?;
        if ready["status"] != "ready" {
            return Err("server failed to start".into());
        }
        Ok(server)
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_some() {
            return;
        }
        // SAFETY: this is the unreaped child owned by this fixture.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
