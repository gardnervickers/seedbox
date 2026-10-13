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

/// Whether the oracle's retained output already contains `marker`.
async fn oracle_printed(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
    marker: &[u8],
) -> Result<bool, String> {
    let mut output = Vec::new();
    for entry in retained_logs(client, id, "oracle").await? {
        if let Logged::Bytes { bytes, .. } = entry {
            output.extend(bytes);
        }
    }
    Ok(output.windows(marker.len()).any(|window| window == marker))
}

/// Hold a paused run before the effect of the fault that follows the oracle's
/// `marker` line. The oracle prints the line, then makes a non-fault call, so
/// the line is published before that fault. Earlier faults are passed one at a
/// time, pausing before each.
async fn hold_before_marked_fault(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
    marker: &[u8],
    deadline: Instant,
) -> Result<(), String> {
    use seedbox_api::{ControlCommand, PlaybackRate};
    while client.playback(id).await.is_err() {
        if Instant::now() > deadline {
            return Err("no live playback endpoint".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut passed = None;
    loop {
        acknowledged(
            client,
            id,
            ControlCommand::PauseBeforeNextFault {
                rate: PlaybackRate::Unlimited,
            },
        )
        .await?;
        // Held before the next fault effect: paused, moved past the last
        // fault passed, and no longer moving.
        let mut last = None;
        let held = loop {
            let observation = client.playback(id).await.map_err(|e| e.to_string())?;
            if observation.state.terminal {
                return Err("run completed before its marked capture phase".into());
            }
            let position = observation.state.position;
            if observation.state.rate == PlaybackRate::Paused
                && last == Some(position)
                && passed != Some(position)
            {
                break position;
            }
            last = Some(position);
            if Instant::now() > deadline {
                return Err("run did not pause before its marked phase".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        // The line precedes a full RPC round trip, so it is published by now;
        // allow for observation latency before passing this fault.
        for _ in 0..20 {
            if oracle_printed(client, id, marker).await? {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        passed = Some(held);
    }
}

/// Run until the fault that follows the oracle's `CAPTURE_NEXT` line, step
/// through it, and capture there. Each checkpoint workload prints that line,
/// then makes a non-fault call, then marks its capture phase with a fault:
/// usually a nominal-speed throttle, so the capture lands at the same
/// committed boundary on every run. checkpoint-pending-stop marks it with a
/// stop whose acknowledgement the guest holds back, so the capture lands while
/// that call is admitted and awaiting its effect.
async fn capture_at_marked_phase(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
) -> Result<(String, u64), String> {
    use seedbox_api::{CheckpointState, ControlCommand, ControlState, PlaybackRate};
    use seedbox_engine_protocol::host_control::ControlError;
    let deadline = Instant::now() + Duration::from_secs(600);
    hold_before_marked_fault(client, id, b"CAPTURE_NEXT\n", deadline).await?;
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

/// The simulation a `replay` reran, from its `MATCH:` status line.
fn replayed(output: &str) -> Result<String, String> {
    output
        .lines()
        .find_map(|line| line.strip_prefix("MATCH: "))
        .map(str::to_owned)
        .ok_or_else(|| format!("missing MATCH simulation ID: {output}"))
}

/// Delete a passing derived workload's simulations, newest first, so their
/// session files (machine snapshots, checkpoints) do not accumulate on the
/// shared server across workloads. Failing workloads keep theirs.
fn delete_simulations(
    cli: &Path,
    directory: &Path,
    state: &Path,
    ids: &[&str],
) -> Result<(), String> {
    for id in ids.iter().rev() {
        invoke(
            cli,
            directory,
            state,
            &format!("delete-{id}"),
            &["delete", "simulation", id],
        )?;
    }
    Ok(())
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
        // The branch restores the source's fault history and must continue it
        // exactly: the same IDs, outcomes and effect times, with no fault the
        // checkpoint held pending applied twice or lost.
        let source_faults = runtime
            .block_on(client.world(&id))
            .map_err(|e| e.to_string())?
            .faults;
        let branch_id: seedbox_api::SimulationId = branch
            .parse()
            .map_err(|e| format!("branch ID {branch}: {e}"))?;
        let branch_faults = runtime
            .block_on(client.world(&branch_id))
            .map_err(|e| e.to_string())?
            .faults;
        if branch_faults != source_faults {
            return Err(format!(
                "{name} {branch} fault history differs from source {id}: {branch_faults:?} != {source_faults:?}"
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
    let replay = replayed(&invoke(
        cli,
        directory,
        state,
        "replay-branch",
        &["replay", &branches[0], "--wall-time", "30m"],
    )?)?;
    delete_simulations(
        cli,
        directory,
        state,
        &[id.as_str(), &branches[0], &branches[1], &replay],
    )
}

/// Run a disk workload (durability.yaml, two-crash.yaml) under the torn-write
/// disk model, which only a resolved definition selects: derive it from a fresh
/// run's definition, switch the model and the oracle's policy, then require
/// PASS and a replay MATCH.
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
    let replay = replayed(&invoke(
        cli,
        directory,
        state,
        "replay-torn",
        &["replay", torn, "--wall-time", "30m"],
    )?)?;
    delete_simulations(
        cli,
        directory,
        state,
        &[simulation_id(&drop)?, torn, &replay],
    )
}

/// Hold the run before the stop that follows the oracle's `CRASH_NEXT` line,
/// step through its admission and let the guest run on for half a second, so
/// the application leader is dead and the supervisor is waiting on the reap the
/// stale-ack gatekeeper holds back for two seconds. Then crash the machine
/// interactively.
async fn crash_during_held_stop(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
) -> Result<u64, String> {
    use seedbox_api::{ControlCommand, ControlState, PlaybackRate};
    let deadline = Instant::now() + Duration::from_secs(600);
    hold_before_marked_fault(client, id, b"CRASH_NEXT\n", deadline).await?;
    let held = client
        .playback(id)
        .await
        .map_err(|e| e.to_string())?
        .state
        .position
        .simulated_time_ns;
    loop {
        acknowledged(client, id, ControlCommand::Step).await?;
        let state = loop {
            let observation = client.playback(id).await.map_err(|e| e.to_string())?;
            if !observation.state.stepping {
                break observation.state;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        if state.terminal {
            return Err("run completed before the held stop was crashed".into());
        }
        if state.position.simulated_time_ns >= held + 500_000_000 {
            break;
        }
        if Instant::now() > deadline {
            return Err("run did not reach the crash point".into());
        }
    }
    let crash = acknowledged(
        client,
        id,
        ControlCommand::InjectFault {
            action: seedbox_api::FaultAction::Crash {
                machine: "app".into(),
            },
        },
    )
    .await?;
    let at = match crash.state {
        ControlState::Acknowledged {
            error: None,
            status,
            ..
        } => status.position.simulated_time_ns,
        state => return Err(format!("crash injection failed: {state:?}")),
    };
    acknowledged(
        client,
        id,
        ControlCommand::SetRate {
            rate: PlaybackRate::Unlimited,
        },
    )
    .await?;
    Ok(at)
}

/// stale-ack.yaml with an interactive crash while the oracle's stop is
/// admitted but unacknowledged. The pending call must end unavailable, the
/// rebooted incarnation's own stop must be confirmed once, and a replay of the
/// interactive history must MATCH.
fn crashed_pending_stop(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    use seedbox_api::FaultStatus;
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
    let crashed_ns = runtime.block_on(crash_during_held_stop(&client, &id))?;
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
        return Err(format!("{id} did not pass: {:?}", finished.status));
    }
    let result = oracle_line(cli, directory, state, "result", id.as_str(), "RESULT ")?;
    if !result.ends_with(" mode=crashed") {
        return Err(format!(
            "{id}: the injected crash did not interrupt the stop: {result}"
        ));
    }
    let world = runtime
        .block_on(client.world(&id))
        .map_err(|e| e.to_string())?;
    let mut faults = world.faults.clone();
    faults.sort_by_key(|fault| fault.id);
    let rows: Vec<_> = faults
        .iter()
        .map(|f| (f.action.as_str(), f.status, f.applied_time_ns))
        .collect();
    let expected = [
        ("stop", FaultStatus::Unavailable),
        ("crash", FaultStatus::Applied),
        ("reboot", FaultStatus::Applied),
        ("stop", FaultStatus::Applied),
        ("restart", FaultStatus::Applied),
    ];
    if rows.len() != expected.len()
        || rows
            .iter()
            .zip(expected)
            .any(|(row, (action, status))| row.0 != action || row.1 != status)
    {
        return Err(format!("{id}: unexpected fault history {rows:?}"));
    }
    if rows[0].2.is_some() || rows[1].2 != Some(crashed_ns) {
        return Err(format!(
            "{id}: the stop must have no effect and the crash land at {crashed_ns} ns: {rows:?}"
        ));
    }
    let app = world
        .machines
        .iter()
        .find(|machine| machine.name == "app")
        .ok_or("no app machine")?;
    // Incarnations count from zero.
    if app.incarnation != 1 {
        return Err(format!(
            "{id}: app incarnation {} after one reboot",
            app.incarnation
        ));
    }
    let replay = replayed(&invoke(
        cli,
        directory,
        state,
        "replay",
        &["replay", id.as_str(), "--wall-time", "30m"],
    )?)?;
    delete_simulations(cli, directory, state, &[id.as_str(), &replay])
}

/// Simulations derived from a workload run rather than submitted from its file:
/// checkpoint branches of every `checkpoint-*` workload, stale-ack with an
/// interactive crash, and the disk workloads under torn writes. They share one
/// server and run one at a time.
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
    if let Err(error) = crashed_pending_stop(
        Path::new(&cli),
        &directory("stale-ack-crashed"),
        state,
        &workloads.join("stale-ack.yaml"),
    ) {
        failures.push(format!("stale-ack.yaml (crashed while held): {error}"));
    }
    for name in ["durability", "two-crash"] {
        let workload = workloads.join(format!("{name}.yaml"));
        if let Err(error) = torn_writes(
            Path::new(&cli),
            &directory(&format!("{name}-torn")),
            state,
            &workload,
        ) {
            failures.push(format!("{} (torn writes): {error}", workload.display()));
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

/// A machine's retained log history in delivery order. Worker gaps depend only
/// on execution; source gaps are observation loss between worker and reader.
#[derive(Clone, Debug, PartialEq)]
enum Logged {
    Bytes {
        incarnation: u32,
        time_ns: u64,
        stderr: bool,
        bytes: Vec<u8>,
    },
    WorkerGap {
        incarnation: u32,
        time_ns: u64,
        records: u64,
    },
    SourceGap {
        observations: u64,
    },
}

impl Logged {
    fn time_ns(&self) -> Option<u64> {
        match self {
            Self::Bytes { time_ns, .. } | Self::WorkerGap { time_ns, .. } => Some(*time_ns),
            Self::SourceGap { .. } => None,
        }
    }
}

async fn retained_logs(
    client: &seedbox_client::Client,
    id: &seedbox_api::SimulationId,
    machine: &str,
) -> Result<Vec<Logged>, String> {
    use futures_util::StreamExt;
    use seedbox_api::{LogDelivery, LogEntry, LogStream};
    let mut stream = client
        .logs_with(id, machine, false)
        .await
        .map_err(|e| e.to_string())?;
    let mut logged = Vec::new();
    while let Some(delivery) = stream.next().await {
        match delivery.map_err(|e| e.to_string())? {
            LogDelivery::Entry { entry, .. } => logged.push(match entry {
                LogEntry::Bytes {
                    incarnation,
                    simulated_time_ns,
                    stream,
                    bytes,
                    ..
                } => Logged::Bytes {
                    incarnation,
                    time_ns: simulated_time_ns,
                    stderr: stream == LogStream::Stderr,
                    bytes,
                },
                LogEntry::MachineGap {
                    incarnation,
                    simulated_time_ns,
                    records,
                    ..
                } => Logged::WorkerGap {
                    incarnation,
                    time_ns: simulated_time_ns,
                    records,
                },
                LogEntry::SourceGap { first, last } => Logged::SourceGap {
                    observations: last - first + 1,
                },
                LogEntry::SourceUnavailable => return Err("log source unavailable".into()),
            }),
            LogDelivery::Gap { first, last } => logged.push(Logged::SourceGap {
                observations: last - first + 1,
            }),
            LogDelivery::Closed => break,
        }
    }
    Ok(logged)
}

/// Logs are best effort, so compare two histories only where both are certain:
/// at a simulated time with no observation loss on either side of it, both runs
/// must retain the same entries in the same order. Returns the times compared.
fn same_retained_logs(source: &[Logged], replay: &[Logged]) -> Result<usize, String> {
    use std::collections::{BTreeMap, BTreeSet};
    type Times<'a> = BTreeMap<u64, Vec<&'a Logged>>;
    fn index(logged: &[Logged]) -> (Times<'_>, Vec<(u64, u64)>) {
        let mut times = Times::new();
        let mut uncertain = Vec::new();
        for (i, entry) in logged.iter().enumerate() {
            match entry.time_ns() {
                Some(time) => times.entry(time).or_default().push(entry),
                None => {
                    let before = logged[..i].iter().rev().find_map(Logged::time_ns);
                    let after = logged[i + 1..].iter().find_map(Logged::time_ns);
                    uncertain.push((before.unwrap_or(0), after.unwrap_or(u64::MAX)));
                }
            }
        }
        (times, uncertain)
    }
    let certain = |ranges: &[(u64, u64)], time: u64| {
        !ranges.iter().any(|&(from, to)| (from..=to).contains(&time))
    };
    let (a, a_uncertain) = index(source);
    let (b, b_uncertain) = index(replay);
    let mut compared = 0;
    for time in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        if certain(&a_uncertain, *time) && certain(&b_uncertain, *time) {
            if a.get(time) != b.get(time) {
                return Err(format!("retained output at {time} ns differs in replay"));
            }
            compared += 1;
        }
    }
    if compared == 0 {
        return Err("no simulated time was retained without loss by both runs".into());
    }
    Ok(compared)
}

/// The record format of harness/acceptance/driver/output_storm.py.
mod storm {
    const ANSI: [&[u8]; 4] = [b"\x1b[31m", b"\x1b[0m", b"\x1b[2K", b"\x1b[1;32m"];

    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    pub struct Header {
        pub boot: String,
        pub process: u32,
        pub thread: u32,
        pub sequence: u64,
        pub length: usize,
    }

    fn unit(boot: &str, process: u32, thread: u32) -> Vec<u8> {
        let mut seed = (u64::from_str_radix(boot, 16).unwrap_or(0)
            ^ (u64::from(process) << 20)
            ^ (u64::from(thread) << 16))
            & 0xFFFF_FFFF;
        let mut out = Vec::new();
        for i in 0..251 {
            seed = (seed * 1_103_515_245 + 12_345) & 0x7FFF_FFFF;
            let value = ((seed >> 16) & 0xFF) as u8;
            out.push(if i % 17 == 0 {
                0
            } else if value == 0x1e {
                0x1d
            } else {
                value
            });
            if i % 61 == 0 {
                out.extend_from_slice(ANSI[((seed >> 8) % 4) as usize]);
            }
        }
        out
    }

    pub fn body(header: &Header) -> Vec<u8> {
        let unit = unit(&header.boot, header.process, header.thread);
        let k = (header.sequence % unit.len() as u64) as usize;
        unit[k..]
            .iter()
            .chain(&unit[..k])
            .copied()
            .cycle()
            .take(header.length)
            .collect()
    }

    /// The header at the start of `data` and its length; `None` if `data`
    /// does not start with a complete header.
    pub fn header(data: &[u8]) -> Option<(Header, usize)> {
        let end = data.iter().position(|&b| b == 0x1f)?;
        let text = std::str::from_utf8(data.get(..end)?.strip_prefix(b"\x1eREC ")?).ok()?;
        let mut fields = text.split(' ');
        let mut field = |name: &str| fields.next()?.strip_prefix(name).map(str::to_owned);
        let boot = field("b=")?;
        let header = Header {
            process: field("p=")?.parse().ok()?,
            thread: field("t=")?.parse().ok()?,
            sequence: field("s=")?.parse().ok()?,
            length: field("n=")?.parse().ok()?,
            boot,
        };
        (header.boot.len() == 8 && fields.next().is_none()).then_some((header, end + 1))
    }
}

/// Check one machine's storm output against the app's own totals.
fn check_storm(logged: &[Logged], totals: &serde_json::Value) -> Result<String, String> {
    use std::collections::{BTreeMap, BTreeSet};
    // Each stream splits into segments at every gap; bytes keep their incarnation.
    let mut segments: [Vec<Vec<(u32, u8)>>; 2] = [vec![Vec::new()], vec![Vec::new()]];
    let (mut worker_gaps, mut source_gaps) = (0, 0);
    let mut logged_bytes = [0u64; 2];
    for entry in logged {
        match entry {
            Logged::Bytes {
                incarnation,
                stderr,
                bytes,
                ..
            } => {
                let stream = usize::from(*stderr);
                logged_bytes[stream] += bytes.len() as u64;
                let segment = segments[stream].last_mut().unwrap();
                segment.extend(bytes.iter().map(|&b| (*incarnation, b)));
            }
            Logged::WorkerGap { records, .. }
            | Logged::SourceGap {
                observations: records,
            } => {
                if matches!(entry, Logged::WorkerGap { .. }) {
                    worker_gaps += records;
                } else {
                    source_gaps += records;
                }
                for stream in &mut segments {
                    stream.push(Vec::new());
                }
            }
        }
    }
    let mut records = Vec::new();
    for (stream, segments) in segments.iter().enumerate() {
        let name = ["stdout", "stderr"][stream];
        for (index, segment) in segments.iter().enumerate() {
            let (after_gap, before_gap) = (index > 0, index + 1 < segments.len());
            let data: Vec<u8> = segment.iter().map(|&(_, b)| b).collect();
            let mut position = data.iter().position(|&b| b == 0x1e).unwrap_or(data.len());
            if position > 0 && !after_gap {
                return Err(format!("{name}: {position} bytes before the first record"));
            }
            while position < data.len() {
                let Some((header, length)) = storm::header(&data[position..]) else {
                    if before_gap && !data[position + 1..].contains(&0x1f) {
                        break; // A header cut by a gap.
                    }
                    return Err(format!("{name}: no record header at byte {position}"));
                };
                let start = position + length;
                let end = (start + header.length).min(data.len());
                let expected = storm::body(&header);
                if data[start..end] != expected[..end - start] {
                    return Err(format!("{name}: record {header:?} is torn or interleaved"));
                }
                if start + header.length > data.len() {
                    if !before_gap {
                        return Err(format!("{name}: record {header:?} is incomplete"));
                    }
                    break;
                }
                let incarnations: BTreeSet<u32> =
                    segment[position..end].iter().map(|&(i, _)| i).collect();
                if incarnations.len() != 1 {
                    return Err(format!("{name}: record {header:?} spans incarnations"));
                }
                records.push((stream, *incarnations.first().unwrap(), header));
                position = end;
            }
        }
    }
    let storms = totals.as_array().ok_or("TOTALS is not a list")?;
    let mut written = [0u64; 2];
    let mut expected_records = BTreeMap::new();
    for storm in storms {
        let key = (
            storm["boot"].as_str().ok_or("boot")?.to_owned(),
            storm["process"].as_u64().ok_or("process")? as u32,
        );
        for (stream, name) in ["stdout", "stderr"].iter().enumerate() {
            written[stream] += storm["bytes"][name].as_u64().ok_or("bytes")?;
            expected_records.insert(
                (key.clone(), stream),
                storm["records"][name].as_u64().ok_or("records")?,
            );
        }
    }
    // Incarnations attribute boots one to one, in boot order.
    let mut boots = BTreeMap::new();
    let mut found = BTreeMap::new();
    let mut sequences: BTreeMap<_, Vec<u64>> = BTreeMap::new();
    for (stream, incarnation, header) in &records {
        let key = (header.boot.clone(), header.process);
        if !expected_records.contains_key(&(key.clone(), *stream)) {
            return Err(format!("record {header:?} from no reported storm"));
        }
        if *boots.entry(header.boot.clone()).or_insert(*incarnation) != *incarnation {
            return Err(format!("boot {} logged in two incarnations", header.boot));
        }
        *found.entry((key.clone(), *stream)).or_insert(0u64) += 1;
        sequences
            .entry((key, header.thread, *stream))
            .or_default()
            .push(header.sequence);
    }
    let incarnations: BTreeSet<_> = boots.values().collect();
    if incarnations.len() != boots.len() || boots.len() != 2 {
        return Err(format!("boots and incarnations do not match: {boots:?}"));
    }
    for ((key, thread, stream), sequence) in &sequences {
        if sequence.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(format!(
                "{key:?} thread {thread} stream {stream}: records out of order"
            ));
        }
    }
    let storms_seen: BTreeSet<_> = found.keys().map(|(key, _)| key).collect();
    if storms_seen.len() != storms.len() {
        return Err(format!(
            "records of only {} storms retained",
            storms_seen.len()
        ));
    }
    // Each lost record or observation carried at most one 2048-byte chunk.
    let gap_chunks = worker_gaps + source_gaps;
    for stream in 0..2 {
        let missing = written[stream]
            .checked_sub(logged_bytes[stream])
            .ok_or_else(|| format!("stream {stream}: more bytes logged than written"))?;
        let bound = gap_chunks * 2048;
        if missing > bound {
            return Err(format!(
                "stream {stream}: {missing} bytes missing but gaps cover at most {bound}"
            ));
        }
    }
    if gap_chunks == 0 && found != expected_records {
        return Err(format!(
            "records without gaps {found:?} != {expected_records:?}"
        ));
    }
    Ok(format!(
        "records={} worker_gap_records={worker_gaps} source_gap_observations={source_gaps} \
         logged={logged_bytes:?} written={written:?}",
        records.len()
    ))
}

/// Check what `logs` retained of an exited leader and its orphaned writer, and
/// that the simulation ended at the oracle's exit, before the writer finished.
fn check_leader_exit(world: &seedbox_api::WorldState, logged: &[Logged]) -> Result<String, String> {
    use seedbox_api::MachineCondition;
    let machine = |name: &str| {
        world
            .machines
            .iter()
            .find(|m| m.name == name)
            .ok_or_else(|| format!("no machine {name}"))
    };
    let (app, oracle) = (machine("app")?, machine("oracle")?);
    if app.condition != MachineCondition::Exited || app.exit_code != Some(0) {
        return Err(format!(
            "app is {:?} with {:?}",
            app.condition, app.exit_code
        ));
    }
    let exited = app.condition_since_ns.ok_or("app exit has no time")?;
    if oracle.condition != MachineCondition::Exited
        || oracle.condition_since_ns != Some(world.simulated_time_ns)
    {
        return Err(format!(
            "simulation ended at {} ns, not at the oracle's exit {:?}",
            world.simulated_time_ns, oracle.condition_since_ns
        ));
    }
    // The writer prints for 10 s after it starts, about 1 s before the leader exits.
    if world.simulated_time_ns >= exited + 9_000_000_000 {
        return Err("simulation waited for the orphaned writer".into());
    }
    let mut text = Vec::new();
    for entry in logged {
        match entry {
            Logged::Bytes {
                time_ns,
                stderr,
                bytes,
                ..
            } => {
                if *time_ns > exited {
                    return Err(format!(
                        "output logged at {time_ns} ns, after exit at {exited}"
                    ));
                }
                if *stderr {
                    return Err(format!(
                        "unexpected stderr {:?}",
                        String::from_utf8_lossy(bytes)
                    ));
                }
                text.extend_from_slice(bytes);
            }
            Logged::WorkerGap { .. } | Logged::SourceGap { .. } => {
                return Err("small output has a log gap".into())
            }
        }
    }
    let text = String::from_utf8(text).map_err(|e| e.to_string())?;
    let leader: Vec<_> = text.lines().filter(|l| l.starts_with("LEADER")).collect();
    let expected: Vec<String> = (0..5)
        .map(|n| format!("LEADER {n}"))
        .chain(["LEADER_EXIT".into()])
        .collect();
    if leader != expected {
        return Err(format!("leader output {leader:?}"));
    }
    let orphan: Vec<_> = text.lines().filter(|l| l.starts_with("ORPHAN ")).collect();
    for (n, line) in orphan.iter().enumerate() {
        if !line.starts_with(&format!("ORPHAN {n:03} ")) {
            return Err(format!("orphan line {n} is {line:?}"));
        }
    }
    // A bounded drain: the lines queued at the leader's exit, not 10 s of output.
    if orphan.is_empty() || orphan.len() >= 50 {
        return Err(format!("{} orphan lines retained", orphan.len()));
    }
    if text.lines().count() != leader.len() + orphan.len() {
        return Err(format!("unexpected app output {text:?}"));
    }
    Ok(format!("exited_ns={exited} orphan_lines={}", orphan.len()))
}

/// Run each `logs-*` workload, check what `logs` retained against the guest's
/// own account, then replay it and require the same retained entries.
fn retained_output(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let output = invoke(
        cli,
        directory,
        state,
        "run",
        &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let id: seedbox_api::SimulationId = simulation_id(&output)?
        .parse()
        .map_err(|e| format!("{e:?}"))?;
    let client = seedbox_client::Client::connect_to(&state.join("server.sock"))
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let logged = runtime.block_on(retained_logs(&client, &id, "app"))?;
    let summary = if workload.file_stem().is_some_and(|s| s == "logs-storm") {
        let totals = oracle_line(cli, directory, state, "totals", id.as_str(), "TOTALS ")?;
        let totals = serde_json::from_str(&totals["TOTALS ".len()..]).map_err(|e| e.to_string())?;
        check_storm(&logged, &totals)?
    } else {
        let world = runtime
            .block_on(client.world(&id))
            .map_err(|e| e.to_string())?;
        check_leader_exit(&world, &logged)?
    };
    eprintln!("retained output: {} {summary}", workload.display());
    let replay = invoke(
        cli,
        directory,
        state,
        "replay",
        &["replay", id.as_str(), "--wall-time", "30m"],
    )?;
    let replayed: seedbox_api::SimulationId = replay
        .lines()
        .find_map(|line| line.strip_prefix("MATCH: "))
        .ok_or_else(|| format!("replay did not match: {replay}"))?
        .parse()
        .map_err(|e| format!("{e:?}"))?;
    let again = runtime.block_on(retained_logs(&client, &replayed, "app"))?;
    let compared = same_retained_logs(&logged, &again)?;
    eprintln!("retained output: {compared} simulated times agree with replay {replayed}");
    Ok(())
}

/// Application output retention: a stdout/stderr storm across a restart and a
/// reboot, and a leader that exits while an orphaned writer continues.
#[test]
fn retained_output_is_accounted_attributed_and_replayed() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let reports =
        std::env::var_os("SEEDBOX_TEST_REPORT_DIR").expect("set a fresh SEEDBOX_TEST_REPORT_DIR");
    let workloads =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/acceptance/workloads");
    let mut selected: Vec<_> = fs::read_dir(&workloads)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|e| e == "yaml")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("logs-"))
        })
        .collect();
    selected.sort();
    assert_eq!(selected.len(), 2);
    let state_directory = tempfile::Builder::new()
        .prefix("seedbox-logs-")
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
    for workload in selected {
        let directory = Path::new(&reports)
            .join("retained-output")
            .join(workload.file_stem().unwrap());
        fs::create_dir_all(&directory).unwrap();
        if let Err(error) = retained_output(Path::new(&cli), &directory, state, &workload) {
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
