//! Whole-run determinism oracles over the public server API.
//!
//! One resolved definition (harness/fixtures/determinism plus external inputs)
//! runs once as the baseline. Each oracle then runs the same definition under a
//! different host arrangement, or an edited copy of it, and checks the server's
//! comparison of witness, full state and fault history, or the unchanged prefix
//! of logs, events and faults. Any host leak into the simulated world fails it.
//! The last oracles cross a server session: runs exported from the first
//! session are imported into a second server with a fresh state directory,
//! where fresh resolutions must match them and imported checkpoints must branch.
//! The first session runs with 3 machine workers and the second at the
//! server default, one thread. A separate test runs references on
//! one thread, then reruns, replays and branches them with 2, 3 and 8 workers.
#![cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]

use futures_util::StreamExt;
use seedbox_api::{
    ComparisonStatus, ControlCommand, ControlReceipt, ControlRequest, ControlState, DeriveRequest,
    EventDelivery, EventKind, InputSource, LogDelivery, LogEntry, Phase, PlaybackRate,
    ProgramInput, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader},
    num::NonZeroU32,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const SEED: u64 = 4242;
const SECOND: u64 = 1_000_000_000;
type Outcome = Result<(), String>;

/// External inputs of the shared definition. `lossy` keeps seeded loss and
/// jitter active from 5 s to 15 s; the oracle partitions and heals at 7–8 s.
fn program() -> Vec<ProgramInput> {
    let input = |key: &str, at: u64, action: Value| ProgramInput {
        key: key.into(),
        at_ns: at,
        action,
        timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
    };
    vec![
        input(
            "lossy",
            5 * SECOND,
            json!({"action": "shape", "handle": "lossy", "from": ["worker"],
                   "to": ["server"], "latency_ns": 3_000_000, "jitter_ns": 2_000_000,
                   "loss_ppm": 100_000}),
        ),
        input(
            "slow",
            10 * SECOND,
            json!({"action": "throttle", "handle": "slow", "machine": "worker",
                   "speed_hz": 300_000_000u64}),
        ),
        input(
            "unshape",
            15 * SECOND,
            json!({"action": "heal", "handle": "lossy"}),
        ),
    ]
}

#[test]
fn whole_run_determinism_oracles() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    // macOS limits Unix socket paths to 104 bytes.
    let directory = tempfile::Builder::new()
        .prefix("seedbox-oracles-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = directory.path().join("state");
    let mut server = Some(Server::start(Path::new(&cli), &state, Some(3)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let failures = runtime.block_on(async {
        let mut oracles = Oracles::new(PathBuf::from(&cli), &state).await;
        let mut failures = Vec::new();
        let baseline = match oracles.baseline().await {
            Ok(baseline) => baseline,
            Err(error) => return vec![format!("baseline: {error}")],
        };
        eprintln!("determinism: baseline {baseline}");
        for (name, result) in [
            (
                "PerfSettingInvariance",
                oracles.perf_settings(&baseline).await,
            ),
            (
                "ConcurrentSimulationIsolation",
                oracles.concurrent_isolation(&baseline).await,
            ),
            (
                "CheckpointTransparencyAndBranchIdentity",
                oracles.checkpoint_transparency(&baseline).await,
            ),
            (
                "EditedTimelinePrefix",
                oracles.edited_timeline_prefix(&baseline).await,
            ),
            ("PreviewPurity", oracles.preview_purity(&baseline).await),
            (
                "AcceptedFaultSurvivesDisconnect",
                oracles.accepted_fault_survives_disconnect(&baseline).await,
            ),
        ] {
            match result {
                Ok(()) => eprintln!("determinism: {name}: PASS"),
                Err(error) => {
                    eprintln!("determinism: {name}: FAIL: {error}");
                    failures.push(format!("{name}: {error}"));
                }
            }
        }
        // Cross-session oracles: export from this session, then shut it down.
        // Exports refuse symlinked parents such as macOS /tmp.
        let root = directory.path().canonicalize().unwrap();
        let carried = match oracles.carry(&root).await {
            Ok(carried) => carried,
            Err(error) => {
                failures.push(format!("cross-session export: {error}"));
                return failures;
            }
        };
        drop(server.take());
        // The server default, one thread, against the first session's pool of 3.
        let second = directory.path().join("state2");
        server = Some(Server::start(Path::new(&cli), &second, None));
        let fresh = Oracles::new(PathBuf::from(&cli), &second).await;
        for (name, result) in [
            (
                "TwinFreshRunsCrossSession",
                fresh.twin_fresh_runs(&carried).await,
            ),
            (
                "ImportedCheckpointExecutes",
                fresh.imported_checkpoint(&carried).await,
            ),
        ] {
            match result {
                Ok(()) => eprintln!("determinism: {name}: PASS"),
                Err(error) => {
                    eprintln!("determinism: {name}: FAIL: {error}");
                    failures.push(format!("{name}: {error}"));
                }
            }
        }
        failures
    });
    drop(server);
    if !failures.is_empty() {
        let retained = directory.keep();
        panic!(
            "{}\nretained server state: {}",
            failures.join("\n"),
            retained.display()
        );
    }
}

/// Machine worker count is a server performance setting. References run on
/// one thread; sessions with 2, 3 and 8 workers (more than the three
/// machines) must reproduce every one of them exactly.
#[test]
fn machine_worker_count_never_changes_results() {
    let cli = std::env::var_os("SEEDBOX_TEST_CLI")
        .expect("harness/check.sh supplies the current release CLI");
    let directory = tempfile::Builder::new()
        .prefix("seedbox-workers-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = directory.path().join("state");
    let mut server = Some(Server::start(Path::new(&cli), &state, Some(1)));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let failures = runtime.block_on(async {
        let mut oracles = Oracles::new(PathBuf::from(&cli), &state).await;
        let baseline = match oracles.baseline().await {
            Ok(baseline) => baseline,
            Err(error) => return vec![format!("baseline: {error}")],
        };
        // Exports refuse symlinked parents such as macOS /tmp.
        let root = directory.path().canonicalize().unwrap();
        let references = match oracles.worker_references(&baseline, &root).await {
            Ok(references) => references,
            Err(error) => return vec![format!("references: {error}")],
        };
        let mut failures = Vec::new();
        let sessions = directory.path().join("state2");
        for workers in [2, 3, 8] {
            drop(server.take());
            server = Some(Server::start(Path::new(&cli), &sessions, Some(workers)));
            let session = Oracles::new(PathBuf::from(&cli), &sessions).await;
            let name = format!("MachineWorkerInvariance({workers} workers)");
            match session.worker_invariance(workers as u64, &references).await {
                Ok(()) => eprintln!("determinism: {name}: PASS"),
                Err(error) => {
                    eprintln!("determinism: {name}: FAIL: {error}");
                    failures.push(format!("{name}: {error}"));
                }
            }
        }
        failures
    });
    drop(server);
    if !failures.is_empty() {
        let retained = directory.keep();
        panic!(
            "{}\nretained server state: {}",
            failures.join("\n"),
            retained.display()
        );
    }
}

struct Oracles {
    cli: PathBuf,
    state: PathBuf,
    client: Client,
    /// The shared resolved definition, as a saved simulation document.
    document: Value,
}

impl Oracles {
    async fn new(cli: PathBuf, state: &Path) -> Self {
        let client = Client::connect_to(&state.join("server.sock")).unwrap();
        Self {
            cli,
            state: state.to_owned(),
            client,
            document: Value::Null,
        }
    }

    /// Resolve the fixture once (submitted paused, then deleted), add the
    /// external inputs, and run that definition unobserved at unlimited rate.
    async fn baseline(&mut self) -> Result<SimulationId, String> {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/fixtures/determinism");
        let output = Command::new(&self.cli)
            .arg("run")
            .arg(&fixture)
            .args([
                "--seed",
                &SEED.to_string(),
                "--paused",
                "--detach",
                "--json",
            ])
            .env("SEEDBOX_STATE", &self.state)
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "submit fixture: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let submitted: Simulation =
            serde_json::from_slice(&output.stdout).map_err(|error| error.to_string())?;
        let id = submitted.metadata.id;
        let deadline = Instant::now() + Duration::from_secs(1200);
        let mut resource = loop {
            let resource = self.client.describe(&id).await.map_err(err)?;
            if matches!(resource.spec.inputs, InputSource::Resolved { .. }) {
                break resource;
            }
            if resource.status.phase == Phase::Completed || Instant::now() > deadline {
                return Err(format!("fixture never resolved: {:?}", resource.status));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        self.client.delete(&id).await.map_err(err)?;
        resource.spec.program = program();
        self.document = serde_json::to_value(&resource).map_err(err)?;
        let baseline = self.submit(&self.document, false).await?;
        self.complete(&baseline).await?;
        Ok(baseline)
    }

    async fn submit(&self, document: &Value, paused: bool) -> Result<SimulationId, String> {
        Ok(self
            .client
            .submit_definition(document, paused)
            .await
            .map_err(err)?
            .metadata
            .id)
    }

    /// Wait until the simulation's playback endpoint exists (it is executing).
    async fn live(&self, id: &SimulationId) -> Outcome {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            match self.client.playback(id).await {
                Ok(_) => return Ok(()),
                Err(seedbox_client::Error::Rejected {
                    code: seedbox_api::ErrorCode::Unavailable,
                    ..
                }) => {}
                Err(error) => return Err(format!("{id}: playback: {error}")),
            }
            let current = self.client.describe(id).await.map_err(err)?;
            if current.status.phase == Phase::Completed {
                return Err(format!("{id} completed before it was observed live"));
            }
            if Instant::now() > deadline {
                return Err(format!("{id}: no live playback endpoint"));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Submit a control and wait for its acknowledgement at a boundary.
    async fn control(
        &self,
        id: &SimulationId,
        command: ControlCommand,
    ) -> Result<ControlReceipt, String> {
        let request = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command,
        };
        let mut receipt = self
            .client
            .request_control(id, &request)
            .await
            .map_err(err)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match &receipt.state {
                ControlState::Acknowledged { error: None, .. } => return Ok(receipt),
                ControlState::Accepted => {}
                state => return Err(format!("{id}: {:?} failed: {state:?}", request.command)),
            }
            if Instant::now() > deadline {
                return Err(format!("{id}: {:?} not acknowledged", request.command));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
            receipt = self
                .client
                .control_receipt(id, &request.request_id)
                .await
                .map_err(err)?;
        }
    }

    async fn rate(&self, id: &SimulationId, rate: PlaybackRate) -> Outcome {
        self.control(id, ControlCommand::SetRate { rate })
            .await
            .map(drop)
    }

    /// Wait until the committed position reaches `at_ns`; false if the run ended first.
    async fn reach(&self, id: &SimulationId, at_ns: u64) -> Result<bool, String> {
        let deadline = Instant::now() + Duration::from_secs(1200);
        loop {
            match self.client.playback(id).await {
                Ok(observation) if observation.state.position.simulated_time_ns >= at_ns => {
                    return Ok(true);
                }
                Ok(observation) if observation.state.terminal => return Ok(false),
                Ok(_) => {}
                Err(_)
                    if self.client.describe(id).await.map_err(err)?.status.phase
                        == Phase::Completed =>
                {
                    return Ok(false);
                }
                Err(error) => return Err(format!("{id}: playback: {error}")),
            }
            if Instant::now() > deadline {
                return Err(format!("{id}: never reached {at_ns} ns"));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Wait for completion with a passing oracle verdict.
    async fn complete(&self, id: &SimulationId) -> Result<Simulation, String> {
        let deadline = Instant::now() + Duration::from_secs(1800);
        loop {
            let current = self.client.describe(id).await.map_err(err)?;
            if current.status.phase == Phase::Completed {
                if current.status.verdict() != TestVerdict::Passed {
                    return Err(format!("{id} did not pass: {:?}", current.status));
                }
                return Ok(current);
            }
            if Instant::now() > deadline {
                return Err(format!("{id}: did not complete"));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The server's comparison: witness, full state, outcome and fault history.
    async fn same_results(&self, baseline: &SimulationId, other: &SimulationId) -> Outcome {
        let comparison = self.client.compare(baseline, other).await.map_err(err)?;
        if comparison.status != ComparisonStatus::Match
            || !comparison.outcome_matches
            || comparison.witness_matches != Some(true)
            || comparison.full_state_matches != Some(true)
            || !comparison.fault_history_matches
        {
            return Err(format!("{baseline} vs {other}: {comparison:?}"));
        }
        Ok(())
    }

    /// Retained application output in delivery order.
    async fn logs(&self, id: &SimulationId) -> Result<Vec<Record>, String> {
        let mut stream = self.client.all_logs_with(id, false).await.map_err(err)?;
        let mut records = Vec::new();
        while let Some(delivery) = stream.next().await {
            match delivery.map_err(err)? {
                LogDelivery::Entry {
                    entry:
                        LogEntry::Bytes {
                            machine,
                            incarnation,
                            simulated_time_ns,
                            stream,
                            bytes,
                        },
                    ..
                } => records.push(Record {
                    machine,
                    incarnation,
                    time: simulated_time_ns,
                    stream: format!("{stream:?}"),
                    bytes,
                }),
                LogDelivery::Closed => break,
                other => return Err(format!("{id}: incomplete log history: {other:?}")),
            }
        }
        Ok(records)
    }

    /// Retained engine events with a simulated time; server lifecycle and
    /// playback controls are host observations and are left out.
    async fn events(&self, id: &SimulationId) -> Result<Vec<(u64, Value)>, String> {
        let mut stream = self.client.events_with(id, false).await.map_err(err)?;
        let mut events = Vec::new();
        while let Some(delivery) = stream.next().await {
            match delivery.map_err(err)? {
                EventDelivery::Event { observation, .. } => {
                    let engine = matches!(
                        observation.event,
                        EventKind::FaultChanged { .. }
                            | EventKind::MachineLifecycle { .. }
                            | EventKind::MachineReady { .. }
                            | EventKind::MachineExited { .. }
                            | EventKind::OverrideRejected { .. }
                            | EventKind::OracleExited { .. }
                    );
                    if let (true, Some(time)) = (engine, observation.simulated_time_ns) {
                        events.push((time, serde_json::to_value(&observation.event).map_err(err)?));
                    }
                }
                EventDelivery::Closed => break,
                EventDelivery::Gap { first, last } => {
                    return Err(format!("{id}: event gap {first}..{last}"));
                }
            }
        }
        Ok(events)
    }

    // ---- 2. PerfSettingInvariance ------------------------------------------------

    /// Playback rate, pauses and observers are host settings: a slowed run with
    /// fast followers, and a run paused twice with stalled and disconnecting
    /// subscribers, must both reproduce the unobserved unlimited baseline.
    async fn perf_settings(&self, baseline: &SimulationId) -> Outcome {
        // This session's server runs every simulation on 3 machine threads.
        let used = self.machine_workers(baseline).await?;
        if used != 3 {
            return Err(format!("{baseline}: {used} machine workers, not 3"));
        }
        // Slow playback for the first simulated seconds, with fast followers.
        let slow = self.submit(&self.document, true).await?;
        self.live(&slow).await?;
        let mut followers = Vec::new();
        let logs = self.client.all_logs(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(logs.for_each(|_| async {})));
        let events = self.client.events(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(events.for_each(|_| async {})));
        let frames = self.client.frames(&slow).await.map_err(err)?;
        followers.push(tokio::spawn(frames.for_each(|_| async {})));
        let tenth = PlaybackRate::Limited {
            numerator: NonZeroU32::MIN,
            denominator: NonZeroU32::new(10).unwrap(),
        };
        self.rate(&slow, tenth).await?;
        if !self.reach(&slow, 3 * SECOND).await? {
            return Err(format!("{slow} ended before 3 s"));
        }
        self.rate(&slow, PlaybackRate::Unlimited).await?;
        self.complete(&slow).await?;
        for follower in followers {
            follower.abort();
        }
        self.same_results(baseline, &slow)
            .await
            .map_err(|e| format!("rate 1/10 with fast followers: {e}"))?;

        // Paused twice, with stalled and disconnecting subscribers.
        let paused = self.submit(&self.document, true).await?;
        self.live(&paused).await?;
        let stalled = (
            self.client.all_logs(&paused).await.map_err(err)?,
            self.client.logs(&paused, "server").await.map_err(err)?,
            self.client.events(&paused).await.map_err(err)?,
            self.client.frames(&paused).await.map_err(err)?,
        );
        let quitter = self.client.frames(&paused).await.map_err(err)?;
        let quitter = tokio::spawn(quitter.take(20).for_each(|_| async {}));
        let logs_quitter = self.client.all_logs(&paused).await.map_err(err)?;
        let logs_quitter = tokio::spawn(logs_quitter.take(5).for_each(|_| async {}));
        self.rate(&paused, PlaybackRate::Unlimited).await?;
        for at in [6, 12] {
            if !self.reach(&paused, at * SECOND).await? {
                return Err(format!("{paused} ended before {at} s"));
            }
            self.rate(&paused, PlaybackRate::Paused).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
            self.rate(&paused, PlaybackRate::Unlimited).await?;
        }
        self.complete(&paused).await?;
        drop(stalled);
        quitter.abort();
        logs_quitter.abort();
        self.same_results(baseline, &paused)
            .await
            .map_err(|e| format!("paused twice with stalled subscribers: {e}"))
    }

    // ---- 3. ConcurrentSimulationIsolation ---------------------------------------

    /// A runs beside B, built from the same assets and seed, while B is resumed,
    /// faulted, paused, stopped and deleted. A must equal the solo baseline, and
    /// nothing done to B may appear in A.
    async fn concurrent_isolation(&self, baseline: &SimulationId) -> Outcome {
        let b = self.submit(&self.document, true).await?;
        self.live(&b).await?;
        // A plays in real time while B is controlled, then at unlimited rate.
        let a = self.submit(&self.document, true).await?;
        self.live(&a).await?;
        let real_time = PlaybackRate::Limited {
            numerator: NonZeroU32::MIN,
            denominator: NonZeroU32::MIN,
        };
        self.rate(&a, real_time).await?;
        let mut requests = Vec::new();
        let mut overlapped = true;
        self.rate(&b, PlaybackRate::Unlimited).await?;
        self.reach(&b, 2 * SECOND).await?;
        let fault = self
            .control(
                &b,
                ControlCommand::InjectFault {
                    action: serde_json::from_value(json!({"action": "partition",
                        "handle": "b-only", "left": ["worker"], "right": ["server", "oracle"]}))
                    .map_err(err)?,
                },
            )
            .await?;
        requests.push(fault.request.request_id.clone());
        for rate in [
            PlaybackRate::Paused,
            PlaybackRate::Unlimited,
            PlaybackRate::Paused,
        ] {
            let receipt = self.control(&b, ControlCommand::SetRate { rate }).await?;
            requests.push(receipt.request.request_id);
            overlapped &= !self.terminal(&a).await;
        }
        let checkpoint = self.control(&b, ControlCommand::Checkpoint).await?;
        requests.push(checkpoint.request.request_id);
        let b_world = self.client.world(&b).await.map_err(err)?;
        if !b_world
            .faults
            .iter()
            .any(|f| f.plan.as_ref().is_some_and(|p| p["handle"] == "b-only"))
        {
            return Err(format!("{b}: injected fault missing from its own world"));
        }
        overlapped &= !self.terminal(&a).await;
        self.client.stop(&b).await.map_err(err)?;
        self.client.delete(&b).await.map_err(err)?;
        overlapped &= !self.terminal(&a).await;
        if !overlapped {
            return Err(format!(
                "{a} finished before B's controls; lengthen the fixture"
            ));
        }
        self.rate(&a, PlaybackRate::Unlimited).await?;
        self.complete(&a).await?;
        self.same_results(baseline, &a).await?;
        for request in &requests {
            if self.client.control_receipt(&a, request).await.is_ok() {
                return Err(format!("{a} reports B's control {request}"));
            }
        }
        let a_world = self.client.world(&a).await.map_err(err)?;
        let base_world = self.client.world(baseline).await.map_err(err)?;
        if fault_identity(&a_world.faults) != fault_identity(&base_world.faults) {
            return Err(format!(
                "{a} faults differ from the baseline: {:?}",
                a_world.faults
            ));
        }
        if !self.client.checkpoints(&a).await.map_err(err)?.is_empty() {
            return Err(format!("{a} lists B's checkpoint"));
        }
        let events = serde_json::to_string(&self.events(&a).await?).map_err(err)?;
        if events.contains(b.as_str()) || events.contains("b-only") {
            return Err(format!("{a} events mention B"));
        }
        if self.logs(&a).await? != self.logs(baseline).await? {
            return Err(format!("{a} logs differ from the baseline"));
        }
        Ok(())
    }

    async fn terminal(&self, id: &SimulationId) -> bool {
        match self.client.playback(id).await {
            Ok(observation) => observation.state.terminal,
            Err(_) => self
                .client
                .describe(id)
                .await
                .map_or(true, |s| s.status.phase == Phase::Completed),
        }
    }

    // ---- 4. CheckpointTransparencyAndBranchIdentity -----------------------------

    /// Capture at each oracle marker (pause, checkpoint, resume); the captured run
    /// must equal the baseline. Two branches from one checkpoint must match each
    /// other, and their output and later faults must equal the captured run's
    /// suffix.
    async fn checkpoint_transparency(&self, baseline: &SimulationId) -> Outcome {
        let captured = self.submit(&self.document, true).await?;
        self.live(&captured).await?;
        let mut marks = self.client.logs(&captured, "oracle").await.map_err(err)?;
        // Real-time playback leaves the blocked child in place when the host pauses.
        let real_time = PlaybackRate::Limited {
            numerator: NonZeroU32::MIN,
            denominator: NonZeroU32::MIN,
        };
        self.rate(&captured, real_time).await?;
        let mut text = String::new();
        let mut checkpoints = Vec::new();
        let mut seen = 0;
        while let Some(delivery) = marks.next().await {
            match delivery.map_err(err)? {
                LogDelivery::Entry {
                    entry: LogEntry::Bytes { bytes, .. },
                    ..
                } => text.push_str(&String::from_utf8_lossy(&bytes)),
                LogDelivery::Closed => break,
                _ => {}
            }
            let count = text.matches("MARK ").count();
            while seen < count {
                seen += 1;
                self.rate(&captured, PlaybackRate::Paused).await?;
                let receipt = self.control(&captured, ControlCommand::Checkpoint).await?;
                checkpoints.push(receipt.request.request_id);
                self.rate(&captured, real_time).await?;
            }
            if text.contains("oracle worker digest") {
                break;
            }
        }
        drop(marks);
        if checkpoints.len() != 5 {
            return Err(format!(
                "{captured}: {} checkpoints for 5 markers",
                checkpoints.len()
            ));
        }
        self.rate(&captured, PlaybackRate::Unlimited).await?;
        self.complete(&captured).await?;
        self.same_results(baseline, &captured)
            .await
            .map_err(|e| format!("captured run: {e}"))?;
        let receipts = self.client.checkpoints(&captured).await.map_err(err)?;
        let captured_at: BTreeMap<String, u64> = receipts
            .iter()
            .filter_map(|receipt| match &receipt.state {
                ControlState::Acknowledged {
                    checkpoint: Some(status),
                    ..
                } if status.state
                    == seedbox_engine_protocol::host_control::CheckpointState::Captured =>
                {
                    Some((status.request_id.clone(), status.position.simulated_time_ns))
                }
                _ => None,
            })
            .collect();
        if checkpoints
            .iter()
            .any(|name| !captured_at.contains_key(name))
        {
            return Err(format!(
                "{captured}: not every checkpoint was captured: {receipts:?}"
            ));
        }
        // Between the healed partition and the pending throttle and heal inputs.
        let name = &checkpoints[2];
        let at = captured_at[name];
        let resource = self
            .client
            .checkpoint_definition(&captured, name)
            .await
            .map_err(err)?;
        let document = serde_json::to_value(&resource).map_err(err)?;
        let first = self.submit(&document, false).await?;
        let second = self.submit(&document, false).await?;
        self.complete(&first).await?;
        self.complete(&second).await?;
        self.same_results(&first, &second)
            .await
            .map_err(|e| format!("branches from {name}: {e}"))?;
        let suffix: Vec<_> = self
            .logs(&captured)
            .await?
            .into_iter()
            .filter(|r| r.time >= at)
            .collect();
        for branch in [&first, &second] {
            let output = self.logs(branch).await?;
            if by_machine(&output) != by_machine(&suffix) {
                return Err(format!(
                    "{branch} output differs from {captured} after {at} ns:\n{}",
                    first_difference(&by_machine(&suffix), &by_machine(&output))
                ));
            }
            let later = |faults: &[seedbox_api::FaultState]| -> Vec<Value> {
                fault_identity(faults)
                    .into_iter()
                    .filter(|f| f["appliedTimeNs"].as_u64().is_some_and(|t| t >= at))
                    .collect()
            };
            let branch_faults = self.client.world(branch).await.map_err(err)?.faults;
            let source_faults = self.client.world(&captured).await.map_err(err)?.faults;
            if later(&branch_faults) != later(&source_faults) {
                return Err(format!(
                    "{branch} faults after {at} ns differ: {:?} vs {:?}",
                    later(&branch_faults),
                    later(&source_faults)
                ));
            }
        }
        Ok(())
    }

    // ---- 5. EditedTimelinePrefix ------------------------------------------------

    /// Edited definitions (drop `slow`; move `unshape` earlier; add an input)
    /// must reproduce the baseline before their first changed input time, and
    /// the oracle's own actions must neither repeat nor disappear.
    async fn edited_timeline_prefix(&self, baseline: &SimulationId) -> Outcome {
        let source = self.client.describe(baseline).await.map_err(err)?;
        let dropped = self
            .client
            .derive(
                baseline,
                &DeriveRequest {
                    request_id: Client::request_id().map_err(err)?,
                    source_created_at_unix_ms: source.metadata.created_at_unix_ms,
                    included: vec!["program:lossy".into(), "program:unshape".into()],
                    start_paused: false,
                },
            )
            .await
            .map_err(err)?
            .metadata
            .id;
        let mut moved = source.clone();
        for input in &mut moved.spec.program {
            if input.key == "unshape" {
                input.at_ns = 12 * SECOND;
            }
        }
        moved.spec.program.sort_by_key(|input| input.at_ns);
        let mut added = source.clone();
        added.spec.program.push(ProgramInput {
            key: "server-slow".into(),
            at_ns: 12 * SECOND,
            action: json!({"action": "throttle", "handle": "server-slow", "machine": "server",
                           "speed_hz": 400_000_000u64}),
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        });
        added.spec.program.sort_by_key(|input| input.at_ns);
        let moved = self
            .submit(&serde_json::to_value(&moved).map_err(err)?, false)
            .await?;
        let added = self
            .submit(&serde_json::to_value(&added).map_err(err)?, false)
            .await?;
        let base = Trace::read(self, baseline).await?;
        for (edited, changed_at, present, absent) in [
            (&dropped, 10 * SECOND, "unshape", "slow"),
            (&moved, 12 * SECOND, "unshape", ""),
            (&added, 12 * SECOND, "server-slow", ""),
        ] {
            self.complete(edited).await?;
            // The edit must change the run, or the prefix check proves nothing.
            let comparison = self.client.compare(baseline, edited).await.map_err(err)?;
            if comparison.status == ComparisonStatus::Match {
                return Err(format!("{edited}: edit left the whole run unchanged"));
            }
            let trace = Trace::read(self, edited).await?;
            base.same_prefix(&trace, changed_at)
                .map_err(|e| format!("{edited} (first change at {changed_at} ns): {e}"))?;
            let keys: Vec<_> = trace
                .faults
                .iter()
                .filter_map(|f| f["inputKey"].as_str())
                .collect();
            if !keys.contains(&present) || keys.contains(&absent) {
                return Err(format!("{edited}: edit not applied; inputs {keys:?}"));
            }
            if trace.oracle_actions() != base.oracle_actions() {
                return Err(format!(
                    "{edited}: oracle actions {:?} vs baseline {:?}",
                    trace.oracle_actions(),
                    base.oracle_actions()
                ));
            }
        }
        Ok(())
    }

    // ---- 6. PreviewPurity -------------------------------------------------------

    /// Hundreds of fault previews and world/timeline reads while paused with
    /// seeded loss and jitter active must not change any later admission,
    /// packet decision or result.
    async fn preview_purity(&self, baseline: &SimulationId) -> Outcome {
        let id = self.submit(&self.document, true).await?;
        self.live(&id).await?;
        let real_time = PlaybackRate::Limited {
            numerator: NonZeroU32::MIN,
            denominator: NonZeroU32::MIN,
        };
        self.rate(&id, real_time).await?;
        if !self.reach(&id, 5 * SECOND + SECOND / 2).await? {
            return Err(format!("{id} ended before 5.5 s"));
        }
        self.rate(&id, PlaybackRate::Paused).await?;
        let world = self.client.world(&id).await.map_err(err)?;
        if !world
            .faults
            .iter()
            .any(|f| f.input_key.as_deref() == Some("lossy") && f.active)
        {
            return Err(format!("{id}: lossy shape not active at the pause"));
        }
        let connection = json!({"from": "worker", "to": "server", "src_addr": "10.0.0.9",
            "dst_addr": "10.0.0.8", "src_port": 40000, "dst_port": 7000, "incarnation": 99});
        let actions = [
            json!({"action": "shape", "handle": "p-shape", "from": ["worker"], "to": ["server"],
                   "latency_ns": 5_000_000, "jitter_ns": 1_000_000, "loss_ppm": 500_000}),
            json!({"action": "partition", "handle": "p-cut", "left": ["worker"],
                   "right": ["server", "oracle"]}),
            json!({"action": "heal", "handle": "lossy"}),
            json!({"action": "heal", "handle": "absent"}),
            json!({"action": "drop_connection", "handle": "p-drop", "connection": connection}),
            json!({"action": "clock_skew", "handle": "p-skew", "machine": "server",
                   "max_offset_ns": 5_000_000_000u64}),
            json!({"action": "clock_drift", "handle": "p-drift", "machine": "worker",
                   "max_rate_ppb": 100_000_000}),
            json!({"action": "throttle", "handle": "p-slow", "machine": "server",
                   "speed_hz": 100_000_000}),
            json!({"action": "crash", "machine": "worker"}),
        ];
        let mut previews = 0;
        let mut explained = BTreeMap::<String, usize>::new();
        for round in 0..40 {
            for action in &actions {
                // Rejections (an unknown handle, a stale connection) are previews too.
                if self.client.preview_fault(&id, action).await.is_ok() {
                    *explained.entry(action["action"].to_string()).or_default() += 1;
                }
                previews += 1;
            }
            if round % 4 == 0 {
                self.client.world(&id).await.map_err(err)?;
                self.client.timeline(&id).await.map_err(err)?;
                self.client.fault_types(&id).await.map_err(err)?;
            }
        }
        // Every kind except the stale connection must be explained.
        if explained.len() < 7 {
            return Err(format!("{id}: previews mostly rejected: {explained:?}"));
        }
        let after = self.client.world(&id).await.map_err(err)?;
        if after.revision != world.revision || after.faults != world.faults {
            return Err(format!(
                "{id}: {previews} previews changed the paused world"
            ));
        }
        self.rate(&id, PlaybackRate::Unlimited).await?;
        self.complete(&id).await?;
        self.same_results(baseline, &id).await?;
        let faults = fault_identity(&self.client.world(&id).await.map_err(err)?.faults);
        let base = fault_identity(&self.client.world(baseline).await.map_err(err)?.faults);
        if faults != base {
            return Err(format!(
                "{id}: later fault IDs differ: {faults:?} vs {base:?}"
            ));
        }
        Ok(())
    }
}

impl Oracles {
    /// Run a CLI command against this session's server; stdout on success.
    fn cli(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = Command::new(&self.cli)
            .args(args)
            .env("SEEDBOX_STATE", &self.state)
            .output()
            .map_err(|error| format!("{}: {error}", self.cli.display()))?;
        if !output.status.success() {
            return Err(format!(
                "seedbox {}: {}{}",
                args.join(" "),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        Ok(output.stdout)
    }

    /// Resolve and run a project afresh with an explicit seed.
    async fn run_project(&self, project: &Path) -> Result<SimulationId, String> {
        let stdout = self.cli(&[
            "run",
            project.to_str().ok_or("project path is not UTF-8")?,
            "--seed",
            &SEED.to_string(),
            "--detach",
            "--json",
        ])?;
        let submitted: Simulation = serde_json::from_slice(&stdout).map_err(err)?;
        self.complete(&submitted.metadata.id).await?;
        Ok(submitted.metadata.id)
    }

    fn export(&self, id: &SimulationId, bundle: &Path) -> Outcome {
        self.cli(&[
            "export",
            id.as_str(),
            "--output",
            bundle.to_str().ok_or("bundle path is not UTF-8")?,
            "--json",
        ])
        .map(drop)
    }

    fn import(&self, bundle: &Path) -> Result<SimulationId, String> {
        let stdout = self.cli(&[
            "import",
            bundle.to_str().ok_or("bundle path is not UTF-8")?,
            "--json",
        ])?;
        let imported: Simulation = serde_json::from_slice(&stdout).map_err(err)?;
        Ok(imported.metadata.id)
    }

    /// Send one control over a raw connection and return it unread.
    fn post_control(
        &self,
        id: &SimulationId,
        request: &ControlRequest,
    ) -> Result<std::os::unix::net::UnixStream, String> {
        use std::io::Write;
        let body = serde_json::to_vec(request).map_err(err)?;
        let mut stream =
            std::os::unix::net::UnixStream::connect(self.state.join("server.sock")).map_err(err)?;
        write!(
            stream,
            "POST /v1/simulations/{id}/controls HTTP/1.1\r\nHost: seedbox.local\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .map_err(err)?;
        stream.write_all(&body).map_err(err)?;
        stream.flush().map_err(err)?;
        Ok(stream)
    }

    /// Poll a receipt until the worker acknowledged it without error.
    async fn acknowledged(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<ControlReceipt, String> {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            let receipt = self
                .client
                .control_receipt(id, request_id)
                .await
                .map_err(err)?;
            match &receipt.state {
                ControlState::Acknowledged { error: None, .. } => return Ok(receipt),
                ControlState::Accepted if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                _ => return Err(format!("{id}: {request_id}: {receipt:?}")),
            }
        }
    }

    // ---- 11. AcceptedFaultSurvivesDisconnect --------------------------------------

    /// Keyed faults whose HTTP clients disconnect before reading the receipt,
    /// once right after writing (perhaps before acceptance) and once after
    /// acceptance: retrying each key must leave exactly one admission, the
    /// partition must drop packets, and the run must replay.
    async fn accepted_fault_survives_disconnect(&self, baseline: &SimulationId) -> Outcome {
        let id = self.submit(&self.document, true).await?;
        self.live(&id).await?;
        self.rate(&id, PlaybackRate::Unlimited).await?;
        if !self.reach(&id, 2 * SECOND).await? {
            return Err(format!("{id} ended before 2 s"));
        }
        let partition = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command: ControlCommand::InjectFault {
                action: serde_json::from_value(json!({"action": "partition",
                    "handle": "dropped-client", "left": ["worker"], "right": ["server"]}))
                .map_err(err)?,
            },
        };
        // Close right after writing: the server may never have accepted it, so
        // the client retries the same key, which admits it exactly once.
        drop(self.post_control(&id, &partition)?);
        tokio::time::sleep(Duration::from_millis(500)).await;
        let first = self
            .client
            .request_control(&id, &partition)
            .await
            .map_err(err)?;
        let receipt = self.acknowledged(&id, &partition.request_id).await?;
        // Retrying an acknowledged key returns that receipt and admits nothing new.
        let retried = self
            .client
            .request_control(&id, &partition)
            .await
            .map_err(err)?;
        if retried != receipt || first.request != receipt.request {
            return Err(format!(
                "{id}: retry changed the receipt: {retried:?} vs {receipt:?}"
            ));
        }
        let at = self.client.playback(&id).await.map_err(err)?.state.position;
        if !self.reach(&id, at.simulated_time_ns + SECOND).await? {
            return Err(format!("{id} ended while partitioned"));
        }
        // The heal's client disconnects after acceptance, before the response.
        let heal = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command: ControlCommand::InjectFault {
                action: serde_json::from_value(
                    json!({"action": "heal", "handle": "dropped-client"}),
                )
                .map_err(err)?,
            },
        };
        let connection = self.post_control(&id, &heal)?;
        let deadline = Instant::now() + Duration::from_secs(60);
        while self
            .client
            .control_receipt(&id, &heal.request_id)
            .await
            .is_err()
        {
            if Instant::now() > deadline {
                return Err(format!("{id}: heal never accepted on an open connection"));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(connection);
        self.acknowledged(&id, &heal.request_id).await?;
        let retried = self.client.request_control(&id, &heal).await.map_err(err)?;
        if !matches!(
            retried.state,
            ControlState::Acknowledged { error: None, .. }
        ) {
            return Err(format!("{id}: heal retry: {retried:?}"));
        }
        self.complete(&id).await?;
        let world = self.client.world(&id).await.map_err(err)?;
        let admitted: Vec<_> = world
            .faults
            .iter()
            .filter(|f| {
                f.action == "partition"
                    && f.plan
                        .as_ref()
                        .is_some_and(|p| p["handle"] == "dropped-client")
            })
            .collect();
        if admitted.len() != 1 {
            return Err(format!(
                "{id}: {} admissions of one keyed partition: {:?}",
                admitted.len(),
                world.faults
            ));
        }
        let interventions = self
            .client
            .timeline(&id)
            .await
            .map_err(err)?
            .entries
            .iter()
            .filter(|e| serde_json::to_string(e).is_ok_and(|t| t.contains("dropped-client")))
            .count();
        // The partition and its heal.
        if interventions != 2 {
            return Err(format!(
                "{id}: {interventions} timeline entries for the keyed fault"
            ));
        }
        let dropped = |world: &seedbox_api::WorldState| -> u64 {
            world
                .links
                .iter()
                .filter(|l| {
                    (l.from == "worker" && l.to == "server")
                        || (l.from == "server" && l.to == "worker")
                })
                .map(|l| l.counters.partition_dropped)
                .sum()
        };
        let base = self.client.world(baseline).await.map_err(err)?;
        if dropped(&world) <= dropped(&base) {
            return Err(format!(
                "{id}: the keyed partition dropped no packets ({} vs baseline {})",
                dropped(&world),
                dropped(&base)
            ));
        }
        // Exit status 0 is MATCH.
        self.cli(&["replay", id.as_str()]).map(drop)
    }

    // ---- 8 and 10: carried across the session boundary ---------------------------

    /// In the first session: fresh runs of the fixture and of an example with
    /// images pulled by tag, and a run with a checkpoint whose source is
    /// deleted after export. Everything is exported for the second session.
    async fn carry(&self, root: &Path) -> Result<Carried, String> {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/fixtures/determinism");
        let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/redis");
        let mut carried = Carried {
            fixture,
            example: example.clone(),
            pinned: root.join("redis-pinned"),
            bundles: root.join("bundles"),
            suffix: Vec::new(),
            checkpoint: String::new(),
        };
        std::fs::create_dir_all(&carried.bundles).map_err(err)?;
        let twin = self.run_project(&carried.fixture).await?;
        self.export(&twin, &carried.bundles.join("fixture.tar"))?;
        let tagged = self.run_project(&example).await?;
        self.export(&tagged, &carried.bundles.join("example.tar"))?;
        // The same example with its pulled image named by a registry digest.
        let engine = bollard::Docker::connect_with_defaults().map_err(err)?;
        let digest = engine
            .inspect_image("docker.io/library/redis:7.2")
            .await
            .map_err(err)?
            .repo_digests
            .unwrap_or_default()
            .into_iter()
            .find(|reference| reference.starts_with("docker.io/library/redis@sha256:"))
            .ok_or("redis:7.2 has no registry digest")?;
        copy_tree(&example, &carried.pinned).map_err(err)?;
        let project = carried.pinned.join("seedbox.yaml");
        let text = std::fs::read_to_string(&project).map_err(err)?;
        let pinned = text.replace("docker.io/library/redis:7.2", &digest);
        if pinned == text {
            return Err("examples/redis no longer names redis:7.2".into());
        }
        std::fs::write(&project, pinned).map_err(err)?;

        // A checkpoint with open TCP streams, volume data and guest entropy in use.
        let source = self.submit(&self.document, true).await?;
        self.live(&source).await?;
        self.rate(&source, PlaybackRate::Unlimited).await?;
        if !self.reach(&source, 9 * SECOND).await? {
            return Err(format!("{source} ended before 9 s"));
        }
        self.rate(&source, PlaybackRate::Paused).await?;
        let receipt = self.control(&source, ControlCommand::Checkpoint).await?;
        carried.checkpoint = receipt.request.request_id;
        self.rate(&source, PlaybackRate::Unlimited).await?;
        self.complete(&source).await?;
        let at = self
            .client
            .checkpoint(&source, &carried.checkpoint)
            .await
            .map_err(err)
            .and_then(|receipt| match receipt.state {
                ControlState::Acknowledged {
                    checkpoint: Some(status),
                    ..
                } if status.state
                    == seedbox_engine_protocol::host_control::CheckpointState::Captured =>
                {
                    Ok(status.position.simulated_time_ns)
                }
                state => Err(format!("{source}: checkpoint not captured: {state:?}")),
            })?;
        carried.suffix = self
            .logs(&source)
            .await?
            .into_iter()
            .filter(|r| r.time >= at)
            .collect();
        self.export(&source, &carried.bundles.join("checkpoint.tar"))?;
        self.client.delete(&source).await.map_err(err)?;
        Ok(carried)
    }

    // ---- 8. TwinFreshRunsCrossSession -------------------------------------------

    /// In a new session with an empty cache, the fixture and the example
    /// (pinned by digest) resolve, prepare and run again; each must match its
    /// imported first-session run.
    async fn twin_fresh_runs(&self, carried: &Carried) -> Outcome {
        for (bundle, project) in [
            ("fixture.tar", &carried.fixture),
            ("example.tar", &carried.pinned),
        ] {
            let imported = self.import(&carried.bundles.join(bundle))?;
            let fresh = self.run_project(project).await?;
            self.same_results(&imported, &fresh)
                .await
                .map_err(|e| format!("{bundle} vs a fresh run of {}: {e}", project.display()))?;
            if self.machine_workers(&fresh).await? != 1 {
                return Err(format!(
                    "{fresh}: the default did not run on one machine thread"
                ));
            }
        }
        Ok(())
    }

    // ---- 10. ImportedCheckpointExecutes -----------------------------------------

    /// A checkpoint exported from a deleted source, imported into a session
    /// that never saw it, must branch twice to equal results and continue
    /// with the source's output.
    async fn imported_checkpoint(&self, carried: &Carried) -> Outcome {
        let imported = self.import(&carried.bundles.join("checkpoint.tar"))?;
        let definition = self
            .client
            .checkpoint_definition(&imported, &carried.checkpoint)
            .await
            .map_err(err)?;
        let document = serde_json::to_value(&definition).map_err(err)?;
        let first = self.submit(&document, false).await?;
        let second = self.submit(&document, false).await?;
        self.complete(&first).await?;
        self.complete(&second).await?;
        self.same_results(&first, &second).await?;
        for branch in [&first, &second] {
            let output = self.logs(branch).await?;
            if by_machine(&output) != by_machine(&carried.suffix) {
                return Err(format!(
                    "{branch} output differs from the deleted source after its checkpoint:\n{}",
                    first_difference(&by_machine(&carried.suffix), &by_machine(&output))
                ));
            }
        }
        Ok(())
    }
}

// ---- 12. MachineWorkerInvariance ------------------------------------------------

/// Single-thread reference runs carried to the worker-count sessions.
struct WorkerReferences {
    bundles: PathBuf,
    /// Bundle file name, and everything its run's observers saw.
    runs: Vec<(&'static str, Trace)>,
    /// The checkpoint in `interactive.tar` that `branch.tar` started from.
    checkpoint: String,
}

/// The shared definition with machines of very different speeds (so rounds
/// finish at very different host times), seeded per-call jitter, and whole
/// machine pause/resume, process pause/resume and crash/reboot beside the
/// shared program's loss, throttle and heal, and the oracle's partition.
fn lifecycle_document(document: &Value) -> Result<Value, String> {
    let mut resource: Simulation = serde_json::from_value(document.clone()).map_err(err)?;
    let input = |key: &str, at: u64, action: Value| ProgramInput {
        key: key.into(),
        at_ns: at,
        action,
        timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
    };
    resource.spec.program.extend([
        input(
            "pause-server",
            2 * SECOND + SECOND / 2,
            json!({"action": "pause", "machine": "server"}),
        ),
        input(
            "resume-server",
            3 * SECOND,
            json!({"action": "resume", "machine": "server"}),
        ),
        input(
            "freeze-server",
            3 * SECOND + SECOND / 2,
            json!({"action": "pause_process", "machine": "server"}),
        ),
        input(
            "thaw-server",
            3 * SECOND + 4 * SECOND / 5,
            json!({"action": "resume_process", "machine": "server"}),
        ),
        input(
            "crash-worker",
            4 * SECOND,
            json!({"action": "crash", "machine": "worker"}),
        ),
        input(
            "reboot-worker",
            4 * SECOND + 3 * SECOND / 5,
            json!({"action": "reboot", "machine": "worker"}),
        ),
    ]);
    resource.spec.program.sort_by_key(|input| input.at_ns);
    let mut document = serde_json::to_value(&resource).map_err(err)?;
    for (path, cpu) in [
        (
            "/spec/machines/server/cpu",
            json!({"speed": "4GHz", "jitter": "25%"}),
        ),
        (
            "/spec/machines/worker/cpu",
            json!({"speed": "250MHz", "jitter": "10%"}),
        ),
        ("/spec/oracle/cpu", json!({"speed": "1GHz", "jitter": "5%"})),
    ] {
        *document
            .pointer_mut(path)
            .ok_or_else(|| format!("resolved definition has no {path}"))? = cpu;
    }
    Ok(document)
}

impl Oracles {
    /// In the single-thread session: the baseline, the lifecycle variant, an
    /// interactively controlled and checkpointed lifecycle run, and a branch
    /// from that checkpoint, each exported with everything observers saw.
    async fn worker_references(
        &self,
        baseline: &SimulationId,
        root: &Path,
    ) -> Result<WorkerReferences, String> {
        let bundles = root.join("worker-bundles");
        std::fs::create_dir_all(&bundles).map_err(err)?;
        let lifecycle = lifecycle_document(&self.document)?;
        let varied = self.submit(&lifecycle, false).await?;
        self.complete(&varied).await?;
        let comparison = self.client.compare(baseline, &varied).await.map_err(err)?;
        if comparison.status == ComparisonStatus::Match {
            return Err(format!(
                "{varied}: the lifecycle variant left the run unchanged"
            ));
        }
        let world = self.client.world(&varied).await.map_err(err)?;
        for action in [
            "pause",
            "resume",
            "pause_process",
            "resume_process",
            "crash",
            "reboot",
        ] {
            if !world.faults.iter().any(|f| f.action == action) {
                return Err(format!("{varied}: no {action} applied: {:?}", world.faults));
            }
        }

        // Live controls at host-chosen boundaries, retained for replay.
        let interactive = self.submit(&lifecycle, true).await?;
        self.live(&interactive).await?;
        self.rate(&interactive, PlaybackRate::Unlimited).await?;
        if !self.reach(&interactive, 2 * SECOND).await? {
            return Err(format!("{interactive} ended before 2 s"));
        }
        self.control(
            &interactive,
            ControlCommand::InjectFault {
                action: serde_json::from_value(json!({"action": "partition",
                    "handle": "live-cut", "left": ["oracle"], "right": ["server"]}))
                .map_err(err)?,
            },
        )
        .await?;
        if !self.reach(&interactive, 5 * SECOND).await? {
            return Err(format!("{interactive} ended before 5 s"));
        }
        self.control(
            &interactive,
            ControlCommand::InjectFault {
                action: serde_json::from_value(json!({"action": "heal", "handle": "live-cut"}))
                    .map_err(err)?,
            },
        )
        .await?;
        if !self.reach(&interactive, 9 * SECOND).await? {
            return Err(format!("{interactive} ended before 9 s"));
        }
        self.rate(&interactive, PlaybackRate::Paused).await?;
        let checkpoint = self
            .control(&interactive, ControlCommand::Checkpoint)
            .await?
            .request
            .request_id;
        self.rate(&interactive, PlaybackRate::Unlimited).await?;
        self.complete(&interactive).await?;
        let resource = self
            .client
            .checkpoint_definition(&interactive, &checkpoint)
            .await
            .map_err(err)?;
        let branch = self
            .submit(&serde_json::to_value(&resource).map_err(err)?, false)
            .await?;
        self.complete(&branch).await?;
        // The branch restores the rebooted worker's later incarnation and
        // must continue the captured run's output exactly.
        let at = match self
            .client
            .checkpoint(&interactive, &checkpoint)
            .await
            .map_err(err)?
            .state
        {
            ControlState::Acknowledged {
                checkpoint: Some(status),
                ..
            } if status.state
                == seedbox_engine_protocol::host_control::CheckpointState::Captured =>
            {
                status.position.simulated_time_ns
            }
            state => return Err(format!("{interactive}: checkpoint not captured: {state:?}")),
        };
        let suffix: Vec<_> = self
            .logs(&interactive)
            .await?
            .into_iter()
            .filter(|r| r.time >= at)
            .collect();
        let output = self.logs(&branch).await?;
        if by_machine(&output) != by_machine(&suffix) {
            return Err(format!(
                "{branch} output differs from {interactive} after {at} ns:\n{}",
                first_difference(&by_machine(&suffix), &by_machine(&output))
            ));
        }

        // Live admission and checkpoint capture publish host-timed events
        // that a rerun of the retained timeline does not, so the controlled
        // run's observations are those of its single-thread rerun.
        let document = serde_json::to_value(self.client.describe(&interactive).await.map_err(err)?)
            .map_err(err)?;
        let replayed = self.submit(&document, false).await?;
        self.complete(&replayed).await?;
        self.same_results(&interactive, &replayed)
            .await
            .map_err(|e| format!("single-thread replay of the controlled run: {e}"))?;

        let mut runs = Vec::new();
        for (bundle, id, observed) in [
            ("baseline.tar", baseline, baseline),
            ("lifecycle.tar", &varied, &varied),
            ("interactive.tar", &interactive, &replayed),
            ("branch.tar", &branch, &branch),
        ] {
            for id in [id, observed] {
                if self.machine_workers(id).await? != 1 {
                    return Err(format!("{id}: the reference did not run on one thread"));
                }
            }
            self.export(id, &bundles.join(bundle))?;
            runs.push((bundle, Trace::read(self, observed).await?));
        }
        Ok(WorkerReferences {
            bundles,
            runs,
            checkpoint,
        })
    }

    /// Machine threads the engine reported for a completed run.
    async fn machine_workers(&self, id: &SimulationId) -> Result<u64, String> {
        let report: Value =
            serde_json::from_slice(&self.cli(&["diagnostics", id.as_str(), "vmm.json"])?)
                .map_err(err)?;
        report["scheduler"]["machine_workers"]
            .as_u64()
            .ok_or_else(|| format!("{id}: engine report has no machine_workers"))
    }

    /// With `workers` machine threads: rerunning each imported reference
    /// (including its retained live controls) and branching from its imported
    /// checkpoint must reproduce outcome, witness, full state and fault
    /// history, and exactly the logs, events, faults and timeline that the
    /// single-thread references produced.
    async fn worker_invariance(&self, workers: u64, references: &WorkerReferences) -> Outcome {
        let mut imported = BTreeMap::new();
        for (bundle, _) in &references.runs {
            imported.insert(*bundle, self.import(&references.bundles.join(bundle))?);
        }
        for (bundle, trace) in &references.runs {
            let source = &imported[bundle];
            let document = if *bundle == "branch.tar" {
                let resource = self
                    .client
                    .checkpoint_definition(&imported["interactive.tar"], &references.checkpoint)
                    .await
                    .map_err(err)?;
                serde_json::to_value(&resource).map_err(err)?
            } else {
                serde_json::to_value(self.client.describe(source).await.map_err(err)?)
                    .map_err(err)?
            };
            let rerun = &self.submit(&document, false).await?;
            self.complete(rerun).await?;
            self.same_results(source, rerun)
                .await
                .map_err(|e| format!("{bundle}: {e}"))?;
            trace
                .same(&Trace::read(self, rerun).await?)
                .map_err(|e| format!("{bundle} vs {rerun}: {e}"))?;
            let used = self.machine_workers(rerun).await?;
            if used != workers {
                return Err(format!("{rerun}: ran on {used} machine workers"));
            }
        }
        // The CLI's replay reruns the controlled run and compares (exit 0 is MATCH).
        self.cli(&["replay", imported["interactive.tar"].as_str()])
            .map(drop)
    }
}

/// What the second session needs from the first.
struct Carried {
    fixture: PathBuf,
    #[allow(dead_code)]
    example: PathBuf,
    /// A copy of the example whose pulled image is named by digest.
    pinned: PathBuf,
    bundles: PathBuf,
    /// The checkpointed source's output from its capture time on.
    suffix: Vec<Record>,
    checkpoint: String,
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Record {
    machine: String,
    incarnation: u32,
    time: u64,
    stream: String,
    bytes: Vec<u8>,
}

/// Each machine stream's bytes; record boundaries are an observation detail.
fn by_machine(records: &[Record]) -> BTreeMap<(String, u32, String), Vec<u8>> {
    let mut streams = BTreeMap::<_, Vec<u8>>::new();
    for r in records {
        streams
            .entry((r.machine.clone(), r.incarnation, r.stream.clone()))
            .or_default()
            .extend_from_slice(&r.bytes);
    }
    streams
}

fn first_difference(
    expected: &BTreeMap<(String, u32, String), Vec<u8>>,
    actual: &BTreeMap<(String, u32, String), Vec<u8>>,
) -> String {
    for key in expected.keys().chain(actual.keys()) {
        let (a, b) = (
            expected.get(key).cloned().unwrap_or_default(),
            actual.get(key).cloned().unwrap_or_default(),
        );
        if a != b {
            let at = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
            let show = |v: &[u8]| {
                String::from_utf8_lossy(&v[at.saturating_sub(80)..(at + 160).min(v.len())])
                    .into_owned()
            };
            return format!(
                "{key:?} at byte {at} (lengths {} and {}):\nexpected: {}\nactual:   {}",
                a.len(),
                b.len(),
                show(&a),
                show(&b)
            );
        }
    }
    "no difference".into()
}

/// Fault identity without mutable status: ID, input key, action, plan and time.
fn fault_identity(faults: &[seedbox_api::FaultState]) -> Vec<Value> {
    faults
        .iter()
        .map(|f| {
            json!({"id": f.id, "inputKey": f.input_key, "action": f.action, "plan": f.plan,
                   "appliedTimeNs": f.applied_time_ns})
        })
        .collect()
}

/// Everything about one run that a prefix comparison inspects.
struct Trace {
    logs: Vec<Record>,
    events: Vec<(u64, Value)>,
    faults: Vec<Value>,
    timeline: Vec<seedbox_api::TimelineEntry>,
}

impl Trace {
    async fn read(oracles: &Oracles, id: &SimulationId) -> Result<Self, String> {
        Ok(Self {
            logs: oracles.logs(id).await?,
            events: oracles.events(id).await?,
            faults: fault_identity(&oracles.client.world(id).await.map_err(err)?.faults),
            timeline: oracles.client.timeline(id).await.map_err(err)?.entries,
        })
    }

    fn oracle_actions(&self) -> Vec<Value> {
        self.faults
            .iter()
            .filter(|f| f["inputKey"].is_null())
            .cloned()
            .collect()
    }

    /// Everything observed is identical, including log record boundaries.
    fn same(&self, other: &Self) -> Outcome {
        self.same_prefix(other, u64::MAX)?;
        if self.logs != other.logs {
            return Err("log records differ in their boundaries or times".into());
        }
        Ok(())
    }

    fn same_prefix(&self, other: &Self, before: u64) -> Outcome {
        let logs = |t: &Self| {
            by_machine(
                &t.logs
                    .iter()
                    .filter(|r| r.time < before)
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        };
        if logs(self) != logs(other) {
            return Err(format!(
                "output differs:\n{}",
                first_difference(&logs(self), &logs(other))
            ));
        }
        let events = |t: &Self| -> Vec<(u64, Value)> {
            t.events
                .iter()
                .filter(|(time, _)| *time < before)
                .cloned()
                .collect()
        };
        let (mine, theirs) = (events(self), events(other));
        if mine != theirs {
            let at = mine.iter().zip(&theirs).take_while(|(a, b)| a == b).count();
            return Err(format!(
                "events differ at index {at} of {} and {}: {:?} vs {:?}",
                mine.len(),
                theirs.len(),
                mine.get(at),
                theirs.get(at)
            ));
        }
        let faults = |t: &Self| -> Vec<Value> {
            t.faults
                .iter()
                .filter(|f| {
                    f["appliedTimeNs"]
                        .as_u64()
                        .is_some_and(|time| time < before)
                })
                .cloned()
                .collect()
        };
        if faults(self) != faults(other) {
            return Err(format!(
                "faults differ: {:?} vs {:?}",
                faults(self),
                faults(other)
            ));
        }
        let timeline = |t: &Self| -> Vec<seedbox_api::TimelineEntry> {
            t.timeline
                .iter()
                .filter(|e| e.at_ns < before)
                .cloned()
                .collect()
        };
        if timeline(self) != timeline(other) {
            return Err(format!(
                "timeline differs: {:?} vs {:?}",
                timeline(self),
                timeline(other)
            ));
        }
        Ok(())
    }
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// The fixture owns its server; `--full-state` makes comparisons include full
/// state. `workers` sets `--machine-workers`; None keeps the server default.
struct Server(Child);
impl Server {
    fn start(binary: &Path, state: &Path, workers: Option<usize>) -> Self {
        let mut command = Command::new(binary);
        command.args(["serve", "--startup-handshake", "--full-state"]);
        if let Some(workers) = workers {
            command.args(["--machine-workers", &workers.to_string()]);
        }
        let mut server = Self(
            command
                .env("SEEDBOX_STATE", state)
                .stdout(Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let mut ready = String::new();
        BufReader::new(server.0.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        let ready: Value = serde_json::from_str(&ready).unwrap();
        assert_eq!(ready["status"], "ready");
        server
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
