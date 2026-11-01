//! Simulations derived from an acceptance workload run rather than submitted
//! from its file: checkpoint branches at a marked phase, an interactive crash
//! while a stop is held, torn-write disk models, and retained-output checks.
//! Every helper runs against the owned server of the calling test.
use super::application::{compare, invoke, invoke_stdout, oracle_line, replayed, simulation_id};
use seedbox_api::{
    CheckpointState, ControlCommand, ControlReceipt, ControlRequest, ControlState, FaultStatus,
    PlaybackRate, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use seedbox_engine_protocol::host_control::ControlError;
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

/// A client of the owned server and a runtime to drive it from a sync test.
pub struct Session {
    pub client: Client,
    pub runtime: tokio::runtime::Runtime,
}
impl Session {
    pub fn new(state: &Path) -> Result<Self, String> {
        Ok(Self {
            client: Client::connect_to(&state.join("server.sock")).map_err(|e| e.to_string())?,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())?,
        })
    }
    pub fn world(&self, id: &SimulationId) -> Result<seedbox_api::WorldState, String> {
        self.runtime
            .block_on(self.client.world(id))
            .map_err(|e| e.to_string())
    }
    pub fn describe(&self, id: &SimulationId) -> Result<Simulation, String> {
        self.runtime
            .block_on(self.client.describe(id))
            .map_err(|e| e.to_string())
    }
}

/// The CLI's `--json` resource for a submitted simulation.
pub fn submitted(output: &str) -> Result<Simulation, String> {
    serde_json::from_str(output).map_err(|error| format!("simulation JSON: {error}: {output}"))
}

fn parse_id(id: &str) -> Result<SimulationId, String> {
    id.parse().map_err(|e| format!("simulation ID {id}: {e:?}"))
}

/// Start `workload` paused and detached; returns its ID.
fn start_paused(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<SimulationId, String> {
    let source = submitted(&invoke_stdout(
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
    Ok(source.metadata.id)
}

/// Wait for completion and require PASS.
fn passed(
    cli: &Path,
    directory: &Path,
    state: &Path,
    session: &Session,
    name: &str,
    id: &SimulationId,
) -> Result<(), String> {
    invoke(
        cli,
        directory,
        state,
        &format!("{name}-wait"),
        &["wait", id.as_str(), "--for", "completed"],
    )?;
    let finished = session.describe(id)?;
    if finished.status.verdict() != TestVerdict::Passed {
        return Err(format!("{name} {id} did not pass: {:?}", finished.status));
    }
    Ok(())
}

/// Send one playback command and wait for the worker's acknowledgement.
pub async fn acknowledged(
    client: &Client,
    id: &SimulationId,
    command: ControlCommand,
) -> Result<ControlReceipt, String> {
    let request = ControlRequest {
        request_id: Client::request_id().map_err(|e| e.to_string())?,
        command,
    };
    let mut receipt: ControlReceipt = client
        .request_control(id, &request)
        .await
        .map_err(|e| e.to_string())?;
    let deadline = Instant::now() + Duration::from_secs(120);
    while receipt.state == ControlState::Accepted {
        if Instant::now() > deadline {
            return Err(format!("{:?} was not acknowledged", request.command));
        }
        receipt = client
            .control_receipt(id, &request.request_id)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(receipt)
}

/// A machine's retained log history in delivery order. Worker gaps depend only
/// on execution; source gaps are observation loss between worker and reader.
#[derive(Clone, Debug, PartialEq)]
pub enum Logged {
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

pub async fn retained_logs(
    client: &Client,
    id: &SimulationId,
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

/// Whether the oracle's retained output already contains `marker`.
async fn oracle_printed(client: &Client, id: &SimulationId, marker: &[u8]) -> Result<bool, String> {
    let mut output = Vec::new();
    for entry in retained_logs(client, id, "oracle").await? {
        if let Logged::Bytes { bytes, .. } = entry {
            output.extend(bytes);
        }
    }
    Ok(output.windows(marker.len()).any(|window| window == marker))
}

/// Whether a simulation has finished executing: it has no worker control
/// endpoint left to hold or capture.
async fn ended(client: &Client, id: &SimulationId) -> bool {
    client.describe(id).await.is_ok_and(|simulation| {
        matches!(
            simulation.status.phase,
            seedbox_api::Phase::Finalizing | seedbox_api::Phase::Completed
        )
    })
}

/// Hold a paused run before the effect of the fault that follows the oracle's
/// `marker` line. The oracle prints the line, then makes a non-fault call, so
/// the line is committed in an earlier round than that fault's admission.
/// Earlier faults are passed one at a time, pausing before each.
///
/// Whether a held fault is the marked one does not depend on host speed or
/// observation latency. The worker publishes each round's logs before the
/// fault changes that round admits, on one ordered observation stream, and the
/// server folds both in that order. So once the server's world shows the held
/// fault admitted (scheduled, not yet applied), the marker is retained if and
/// only if it preceded that fault. Each checkpoint oracle makes one fault call
/// at a time, so the held fault is the only scheduled one not already passed.
async fn hold_before_marked_fault(
    client: &Client,
    id: &SimulationId,
    marker: &[u8],
    deadline: Instant,
) -> Result<(), String> {
    let missing = || {
        format!(
            "run completed before the fault after its {:?} oracle line",
            String::from_utf8_lossy(marker).trim_end()
        )
    };
    while client.playback(id).await.is_err() {
        if ended(client, id).await {
            return Err(missing());
        }
        if Instant::now() > deadline {
            return Err("no live playback endpoint".into());
        }
    }
    let mut passed = None;
    let mut held_faults = std::collections::BTreeSet::new();
    loop {
        let request = acknowledged(
            client,
            id,
            ControlCommand::PauseBeforeNextFault {
                rate: PlaybackRate::Unlimited,
            },
        )
        .await;
        let anchor = match request {
            Ok(ControlReceipt {
                state: ControlState::Acknowledged { status, .. },
                ..
            }) => status.position,
            Ok(receipt) => return Err(format!("pause request failed: {receipt:?}")),
            Err(error) => {
                return Err(if ended(client, id).await {
                    missing()
                } else {
                    error
                })
            }
        };
        // Held before the next fault effect: paused with the one-shot pause
        // consumed, in a playback sample taken after the request (one that
        // still showed the pause armed, or a later boundary than its ack).
        let mut current = false;
        let held = loop {
            let observation = match client.playback(id).await {
                Ok(observation) => observation,
                Err(_) if ended(client, id).await => return Err(missing()),
                Err(error) => return Err(error.to_string()),
            };
            if observation.state.terminal {
                return Err(missing());
            }
            let state = observation.state;
            current |=
                state.before_next_fault || state.position.boundary_index > anchor.boundary_index;
            if current
                && state.rate == PlaybackRate::Paused
                && !state.before_next_fault
                && passed != Some(state.position)
            {
                break state.position;
            }
            if Instant::now() > deadline {
                return Err("run did not pause before its marked phase".into());
            }
        };
        // Wait for the held fault's admission to reach the server; every log
        // the worker published before it has been retained by then.
        let fault = loop {
            let world = client.world(id).await.map_err(|e| e.to_string())?;
            if let Some(fault) = world.faults.iter().find(|f| {
                matches!(f.status, FaultStatus::Unrequested | FaultStatus::Requested)
                    && !held_faults.contains(&f.id)
            }) {
                break fault.id;
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "held at {held:?} without an observed scheduled fault: {:?}",
                    world.faults
                ));
            }
        };
        held_faults.insert(fault);
        if oracle_printed(client, id, marker).await? {
            return Ok(());
        }
        passed = Some(held);
    }
}

/// Step until playback reports the admitted round committed.
async fn step(client: &Client, id: &SimulationId) -> Result<seedbox_api::PlaybackStatus, String> {
    acknowledged(client, id, ControlCommand::Step).await?;
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let observation = client.playback(id).await.map_err(|e| e.to_string())?;
        if !observation.state.stepping {
            return Ok(observation.state);
        }
        if Instant::now() > deadline {
            return Err("an admitted step did not commit".into());
        }
    }
}

/// Run until the fault that follows the oracle's `CAPTURE_NEXT` line, step
/// through it, and capture there. Each checkpoint workload prints that line,
/// then makes a non-fault call, then marks its capture phase with a fault:
/// usually a factor-1 throttle, so the capture lands at the same committed
/// boundary on every run. checkpoint-pending-stop marks it with a stop whose
/// acknowledgement the guest holds back, so the capture lands while that call
/// is admitted and awaiting its effect.
async fn capture_at_marked_phase(
    client: &Client,
    id: &SimulationId,
) -> Result<(String, u64), String> {
    let deadline = Instant::now() + Duration::from_secs(600);
    hold_before_marked_fault(client, id, b"CAPTURE_NEXT\n", deadline).await?;
    for _ in 0..16 {
        step(client, id).await?;
        let receipt = acknowledged(client, id, ControlCommand::Checkpoint).await?;
        match receipt.state {
            // Still before the effect, or I/O cost outstanding: take another round.
            ControlState::Acknowledged {
                error:
                    Some(
                        ControlError::FaultPending
                        | ControlError::InvalidCheckpoint
                        | ControlError::NotReady
                        | ControlError::Unsupported,
                    ),
                ..
            } => continue,
            ControlState::Acknowledged { error: None, .. } => {}
            state => return Err(format!("checkpoint request failed: {state:?}")),
        }
        let name = receipt.request.request_id;
        loop {
            let receipts = client.checkpoints(id).await.map_err(|e| e.to_string())?;
            let captured = receipts.iter().find_map(|receipt| match &receipt.state {
                ControlState::Acknowledged {
                    checkpoint: Some(checkpoint),
                    ..
                } if checkpoint.request_id == name
                    && checkpoint.state == CheckpointState::Captured =>
                {
                    Some(checkpoint.position.simulated_time_ns)
                }
                _ => None,
            });
            if let Some(at) = captured {
                acknowledged(
                    client,
                    id,
                    ControlCommand::SetRate {
                        rate: PlaybackRate::Unlimited,
                    },
                )
                .await?;
                return Ok((name, at));
            }
            if Instant::now() > deadline {
                return Err(format!("checkpoint {name} was not captured: {receipts:?}"));
            }
            // A pending capture needs a boundary with no outstanding I/O cost.
            step(client, id).await?;
        }
    }
    Err("no checkpoint could be requested after the marked fault".into())
}

/// Branch a captured checkpoint into a new simulation and wait for its PASS.
fn branch(
    cli: &Path,
    directory: &Path,
    state: &Path,
    session: &Session,
    id: &SimulationId,
    checkpoint: &str,
    name: &str,
) -> Result<SimulationId, String> {
    let branched = session.runtime.block_on(async {
        let definition = session
            .client
            .checkpoint_definition(id, checkpoint)
            .await
            .map_err(|e| format!("{name}: checkpoint definition: {e}"))?;
        let document = serde_json::to_value(&definition).map_err(|e| e.to_string())?;
        session
            .client
            .submit_definition(&document, false)
            .await
            .map_err(|e| format!("{name}: submit: {e}"))
    })?;
    let branch = branched.metadata.id;
    fs::write(directory.join(format!("{name}.id")), branch.as_str()).map_err(|e| e.to_string())?;
    passed(cli, directory, state, session, name, &branch)?;
    Ok(branch)
}

/// Capture a `checkpoint-*` workload at its marked phase, finish the source
/// run, then branch twice. The branches must pass, match each other, replay,
/// continue the source's fault history, and print the same continuation
/// evidence as the source after the capture point.
pub fn checkpoint_branches(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let session = Session::new(state)?;
    let id = start_paused(cli, directory, state, workload)?;
    let (checkpoint, captured_ns) = session
        .runtime
        .block_on(capture_at_marked_phase(&session.client, &id))?;
    passed(cli, directory, state, &session, "source", &id)?;
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
    // A branch keeps input authority, not observation history: its world lists
    // the faults still to take effect at the capture, under their source IDs.
    let source_faults: Vec<_> = session
        .world(&id)?
        .faults
        .into_iter()
        .filter(|fault| fault.applied_time_ns.is_none_or(|at| at > captured_ns))
        .collect();
    let mut branches = Vec::new();
    for name in ["branch-a", "branch-b"] {
        let branch = branch(cli, directory, state, &session, &id, &checkpoint, name)?;
        let evidence = oracle_line(
            cli,
            directory,
            state,
            &format!("{name}-evidence"),
            branch.as_str(),
            "CONTINUATION ",
        )?;
        if evidence != expected {
            return Err(format!(
                "{name} {branch} continuation differs from source {id}: {evidence} != {expected}"
            ));
        }
        // The branch must continue the source's fault history exactly: the
        // same IDs, outcomes and effect times after the capture, with no
        // fault the checkpoint held pending applied twice or lost.
        let branch_faults = session.world(&branch)?.faults;
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
        branches[0].as_str(),
        branches[1].as_str(),
    )?;
    replayed(&invoke(
        cli,
        directory,
        state,
        "replay-branch",
        &["replay", branches[0].as_str(), "--wall-time", "30m"],
    )?)?;
    Ok(())
}

/// Run a disk workload (durability.yaml, two-crash.yaml) under the torn-write
/// disk model, which only a resolved definition selects: take a fresh run's
/// definition, switch the model and the oracle's policy, then require PASS and
/// a replay MATCH.
pub fn torn_writes(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let drop = invoke(
        cli,
        directory,
        state,
        "drop",
        &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let described = invoke_stdout(
        cli,
        directory,
        state,
        "describe-drop",
        &[
            "show",
            "--view",
            "definition",
            simulation_id(&drop)?,
            "--json",
        ],
    )?;
    let mut definition: serde_json::Value =
        serde_json::from_str(&described).map_err(|error| format!("describe: {error}"))?;
    definition
        .as_object_mut()
        .ok_or("definition is not an object")?
        .remove("status");
    let disk = &mut definition["spec"]["simulator"]["disk"];
    if disk["torn_writes"] != false {
        return Err(format!("unexpected disk model {disk}"));
    }
    disk["torn_writes"] = true.into();
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
    replayed(&invoke(
        cli,
        directory,
        state,
        "replay-torn",
        &["replay", torn, "--wall-time", "30m"],
    )?)?;
    Ok(())
}

/// Hold the run before the stop that follows the oracle's `CRASH_NEXT` line,
/// step through its admission and let the guest run on for half a second, so
/// the application leader is dead and the supervisor is waiting on the reap the
/// stale-ack gatekeeper holds back for two seconds. Then crash the machine
/// interactively.
async fn crash_during_held_stop(client: &Client, id: &SimulationId) -> Result<u64, String> {
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
        let state = step(client, id).await?;
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
pub fn crashed_pending_stop(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let session = Session::new(state)?;
    let id = start_paused(cli, directory, state, workload)?;
    let crashed_ns = session
        .runtime
        .block_on(crash_during_held_stop(&session.client, &id))?;
    passed(cli, directory, state, &session, "source", &id)?;
    let result = oracle_line(cli, directory, state, "result", id.as_str(), "RESULT ")?;
    if !result.ends_with(" mode=crashed") {
        return Err(format!(
            "{id}: the injected crash did not interrupt the stop: {result}"
        ));
    }
    let world = session.world(&id)?;
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
    // The held stop never takes effect: the crash settles it unavailable.
    if rows[0].2 != Some(crashed_ns) || rows[1].2 != Some(crashed_ns) {
        return Err(format!(
            "{id}: the crash must land at {crashed_ns} ns and settle the stop: {rows:?}"
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
    replayed(&invoke(
        cli,
        directory,
        state,
        "replay",
        &["replay", id.as_str(), "--wall-time", "30m"],
    )?)?;
    Ok(())
}

/// Logs are best effort, so compare two histories only where both are certain:
/// at a simulated time with no observation loss on either side of it, both runs
/// must retain the same entries in the same order. Returns the times compared.
pub fn same_retained_logs(source: &[Logged], replay: &[Logged]) -> Result<usize, String> {
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
pub fn check_storm(logged: &[Logged], totals: &serde_json::Value) -> Result<String, String> {
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
    // Each lost record or observation carried at most one 16 KiB chunk.
    let gap_chunks = worker_gaps + source_gaps;
    for stream in 0..2 {
        let missing = written[stream]
            .checked_sub(logged_bytes[stream])
            .ok_or_else(|| format!("stream {stream}: more bytes logged than written"))?;
        let bound = gap_chunks * 16 * 1024;
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
pub fn check_leader_exit(
    world: &seedbox_api::WorldState,
    logged: &[Logged],
) -> Result<String, String> {
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

/// Run a `logs-*` workload, check what `logs` retained against the guest's
/// own account, then replay it and require the same retained entries.
pub fn retained_output(
    cli: &Path,
    directory: &Path,
    state: &Path,
    workload: &Path,
) -> Result<(), String> {
    let session = Session::new(state)?;
    let output = invoke(
        cli,
        directory,
        state,
        "run",
        &["run", workload.to_str().unwrap(), "--wall-time", "30m"],
    )?;
    let id = parse_id(simulation_id(&output)?)?;
    let logged = session
        .runtime
        .block_on(retained_logs(&session.client, &id, "app"))?;
    let summary = if workload.file_stem().is_some_and(|s| s == "logs-storm") {
        let totals = oracle_line(cli, directory, state, "totals", id.as_str(), "TOTALS ")?;
        let totals = serde_json::from_str(&totals["TOTALS ".len()..]).map_err(|e| e.to_string())?;
        check_storm(&logged, &totals)?
    } else {
        check_leader_exit(&session.world(&id)?, &logged)?
    };
    eprintln!("retained output: {} {summary}", workload.display());
    let replay = invoke(
        cli,
        directory,
        state,
        "replay",
        &["replay", id.as_str(), "--wall-time", "30m"],
    )?;
    let replayed = parse_id(&replayed(&replay)?)?;
    let again = session
        .runtime
        .block_on(retained_logs(&session.client, &replayed, "app"))?;
    let compared = same_retained_logs(&logged, &again)?;
    eprintln!("retained output: {compared} simulated times agree with replay {replayed}");
    Ok(())
}
