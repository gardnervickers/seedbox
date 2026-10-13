//! Whole-run determinism oracles over the public server API.
//!
//! One resolved definition (harness/fixtures/determinism plus external inputs)
//! runs once as the baseline. Each oracle then runs the same definition under a
//! different host arrangement, or an edited copy of it, and checks the server's
//! comparison of witness, full state and fault history, or the unchanged prefix
//! of logs, events and faults. Any host leak into the simulated world fails it.
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
    let server = Server::start(Path::new(&cli), &state);
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
        if events(self) != events(other) {
            return Err(format!(
                "events differ: {:?} vs {:?}",
                events(self),
                events(other)
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

/// The fixture owns its server; `--full-state` makes comparisons include full state.
struct Server(Child);
impl Server {
    fn start(binary: &Path, state: &Path) -> Self {
        let mut server = Self(
            Command::new(binary)
                .args(["serve", "--startup-handshake", "--full-state"])
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
