//! One server session and the public API calls every oracle shares.
use crate::trace::Record;
use crate::{program, Outcome, SEED};
use futures_util::StreamExt;
use seedbox_api::{
    CheckpointState, ComparisonStatus, ControlCommand, ControlReceipt, ControlRequest,
    ControlState, EventDelivery, EventKind, ExecutionTermination, InputSource, LogDelivery,
    LogEntry, Phase, PlaybackRate, Simulation, SimulationId, TestVerdict,
};
use seedbox_client::Client;
use seedbox_engine_protocol::host_control::ControlError;
use seedbox_test::process::Guarded;
use serde_json::Value;
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// Poll server observations back to back until `poll` yields, within a
/// generous containment bound (never a pacing delay).
pub async fn within<T>(
    seconds: u64,
    what: impl FnOnce() -> String,
    poll: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(Duration::from_secs(seconds), poll)
        .await
        .map_err(|_| format!("{} within {seconds} s", what()))?
}

pub fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../harness/fixtures/determinism")
}

pub const REAL_TIME: PlaybackRate = PlaybackRate::Limited {
    numerator: std::num::NonZeroU32::MIN,
    denominator: std::num::NonZeroU32::MIN,
};

pub struct Session {
    pub cli: PathBuf,
    pub state: PathBuf,
    pub client: Client,
    /// Machine workers this session's submissions request; None leaves the
    /// count to the server's default.
    pub workers: Option<u32>,
    /// The shared resolved definition, as a saved simulation document.
    pub document: Value,
    /// Simulations this session created and has not yet deleted.
    created: std::sync::Mutex<Vec<SimulationId>>,
}

impl Session {
    pub fn new(cli: &Path, state: &Path, workers: Option<u32>) -> Self {
        Self {
            cli: cli.to_owned(),
            state: state.to_owned(),
            client: Client::connect_to(&state.join("server.sock")).unwrap(),
            workers,
            document: Value::Null,
            created: Default::default(),
        }
    }

    /// Run a CLI command against this session's server; stdout on success.
    pub fn cli(&self, args: &[&str]) -> Result<Vec<u8>, String> {
        let output = Command::new(&self.cli)
            .args(args)
            .env_remove("SEEDBOX_VMM")
            .env("SEEDBOX_STATE", &self.state)
            .guarded_output()
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

    /// Resolve the fixture once (submitted paused, then deleted), add the
    /// external inputs, and run that definition unobserved at unlimited rate.
    pub async fn baseline(&mut self) -> Result<SimulationId, String> {
        let stdout = self.cli(&[
            "run",
            fixture().to_str().ok_or("fixture path is not UTF-8")?,
            "--seed",
            &SEED.to_string(),
            "--paused",
            "--detach",
            "--json",
        ])?;
        let submitted: Simulation = serde_json::from_slice(&stdout).map_err(err)?;
        let id = submitted.metadata.id;
        let mut resource = within(1200, || format!("{id}: fixture not resolved"), async {
            loop {
                let resource = self.client.describe(&id).await.map_err(err)?;
                if matches!(resource.spec.inputs, InputSource::Resolved { .. }) {
                    return Ok(resource);
                }
                if resource.status.phase == Phase::Completed {
                    return Err(format!("fixture never resolved: {:?}", resource.status));
                }
            }
        })
        .await?;
        self.client.delete(&id).await.map_err(err)?;
        resource.spec.program = program();
        self.document = serde_json::to_value(&resource).map_err(err)?;
        let baseline = self.submit(&self.document, false).await?;
        self.complete(&baseline).await?;
        Ok(baseline)
    }

    /// Submit a saved document with this session's machine worker count.
    pub async fn submit(&self, document: &Value, paused: bool) -> Result<SimulationId, String> {
        self.submit_with(document, paused, self.workers).await
    }

    pub async fn submit_with(
        &self,
        document: &Value,
        paused: bool,
        workers: Option<u32>,
    ) -> Result<SimulationId, String> {
        let mut document = document.clone();
        if let Some(options) = document
            .as_object_mut()
            .and_then(|d| d.get_mut("options"))
            .and_then(Value::as_object_mut)
        {
            options.remove("machineWorkers");
        }
        if let Some(workers) = workers {
            document["options"]["machineWorkers"] = workers.into();
        }
        let id = self
            .client
            .submit_definition(&document, paused)
            .await
            .map_err(err)?
            .metadata
            .id;
        Ok(self.created(id))
    }

    /// Track a simulation for [`Self::release`].
    pub fn created(&self, id: SimulationId) -> SimulationId {
        self.created.lock().unwrap().push(id.clone());
        id
    }

    /// Delete every simulation this session created except `keep`. Each run
    /// retains its RAM checkpoints and outputs, and a whole session's would
    /// otherwise use tens of GiB.
    pub async fn release(&self, keep: &[&SimulationId]) -> Outcome {
        let created = std::mem::take(&mut *self.created.lock().unwrap());
        let (kept, released): (Vec<_>, Vec<_>) =
            created.into_iter().partition(|id| keep.contains(&id));
        *self.created.lock().unwrap() = kept;
        for id in released {
            match self.client.delete(&id).await {
                // Some oracles delete their own simulations.
                Ok(_)
                | Err(seedbox_client::Error::Rejected {
                    code: seedbox_api::ErrorCode::NotFound,
                    ..
                }) => {}
                Err(error) => return Err(format!("delete {id}: {error}")),
            }
        }
        Ok(())
    }

    /// Machine threads the server ran this simulation with.
    pub async fn machine_workers(&self, id: &SimulationId) -> Result<u32, String> {
        self.client
            .describe(id)
            .await
            .map_err(err)?
            .options
            .machine_workers
            .ok_or_else(|| format!("{id}: no effective machine worker count"))
    }

    /// Wait until the simulation's playback endpoint exists (it is executing).
    pub async fn live(&self, id: &SimulationId) -> Outcome {
        within(600, || format!("{id}: no live playback endpoint"), async {
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
            }
        })
        .await
    }

    /// Submit a control and wait for its acknowledgement at a boundary; a
    /// checkpoint also waits for its deferred capture.
    pub async fn control(
        &self,
        id: &SimulationId,
        command: ControlCommand,
    ) -> Result<ControlReceipt, String> {
        let request = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command,
        };
        self.client
            .request_control::<ControlReceipt>(id, &request)
            .await
            .map_err(err)?;
        self.acknowledged(id, &request.request_id).await
    }

    /// Poll a receipt until the worker acknowledged it without error (and
    /// captured it, for a checkpoint).
    pub async fn acknowledged(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<ControlReceipt, String> {
        within(
            120,
            || format!("{id}: {request_id} not acknowledged"),
            async {
                loop {
                    let receipt: ControlReceipt = self
                        .client
                        .control_receipt(id, request_id)
                        .await
                        .map_err(err)?;
                    match &receipt.state {
                        ControlState::Acknowledged {
                            error: None,
                            checkpoint,
                            ..
                        } if checkpoint
                            .as_ref()
                            .is_none_or(|c| c.state == CheckpointState::Captured) =>
                        {
                            return Ok(receipt)
                        }
                        ControlState::Accepted | ControlState::Acknowledged { error: None, .. } => {
                        }
                        _ => return Err(format!("{id}: {request_id} failed: {receipt:?}")),
                    }
                }
            },
        )
        .await
    }

    pub async fn rate(&self, id: &SimulationId, rate: PlaybackRate) -> Outcome {
        self.control(id, ControlCommand::SetRate { rate })
            .await
            .map(drop)
    }

    /// Play the rest of the run at unlimited rate. The run may end before
    /// the worker sees the change, which is fine: nothing is left to speed up.
    pub async fn finish(&self, id: &SimulationId) -> Outcome {
        let request = ControlRequest {
            request_id: Client::request_id().map_err(err)?,
            command: ControlCommand::SetRate {
                rate: PlaybackRate::Unlimited,
            },
        };
        self.client
            .request_control::<ControlReceipt>(id, &request)
            .await
            .map_err(err)?;
        within(
            120,
            || format!("{id}: unlimited rate not acknowledged"),
            async {
                loop {
                    let receipt: ControlReceipt = self
                        .client
                        .control_receipt(id, &request.request_id)
                        .await
                        .map_err(err)?;
                    match receipt.state {
                        ControlState::Acknowledged {
                            error: None | Some(ControlError::Completed),
                            ..
                        }
                        | ControlState::Interrupted => return Ok(()),
                        ControlState::Accepted => {}
                        state => return Err(format!("{id}: unlimited rate: {state:?}")),
                    }
                }
            },
        )
        .await
    }

    pub async fn inject(&self, id: &SimulationId, action: Value) -> Result<ControlReceipt, String> {
        let action = serde_json::from_value(action).map_err(err)?;
        self.control(id, ControlCommand::InjectFault { action })
            .await
    }

    /// Pause, capture a checkpoint, and return its name and capture time.
    pub async fn checkpoint(&self, id: &SimulationId) -> Result<(String, u64), String> {
        self.rate(id, PlaybackRate::Paused).await?;
        let receipt = self.control(id, ControlCommand::Checkpoint).await?;
        match receipt.state {
            ControlState::Acknowledged {
                checkpoint: Some(status),
                ..
            } => Ok((status.request_id, status.position.simulated_time_ns)),
            state => Err(format!("{id}: checkpoint not captured: {state:?}")),
        }
    }

    /// Wait until the committed position reaches `at_ns`; false if the run ended first.
    pub async fn reach(&self, id: &SimulationId, at_ns: u64) -> Result<bool, String> {
        within(1200, || format!("{id}: did not reach {at_ns} ns"), async {
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
            }
        })
        .await
    }

    pub async fn terminal(&self, id: &SimulationId) -> bool {
        match self.client.playback(id).await {
            Ok(observation) => observation.state.terminal,
            Err(_) => self
                .client
                .describe(id)
                .await
                .map_or(true, |s| s.status.phase == Phase::Completed),
        }
    }

    /// Wait for completion with a passing oracle verdict.
    pub async fn complete(&self, id: &SimulationId) -> Result<Simulation, String> {
        within(1800, || format!("{id}: did not complete"), async {
            loop {
                let current = self.client.describe(id).await.map_err(err)?;
                if current.status.phase == Phase::Completed {
                    // A passing oracle is not enough: an execution error after
                    // its exit (missing evidence, say) leaves nothing to compare.
                    if current.status.verdict() != TestVerdict::Passed
                        || current.status.termination != Some(ExecutionTermination::OracleExited)
                    {
                        return Err(format!("{id} did not pass: {:?}", current.status));
                    }
                    return Ok(current);
                }
            }
        })
        .await
    }

    /// The server's comparison: witness, full state, outcome and fault
    /// history.
    pub async fn same_results(&self, baseline: &SimulationId, other: &SimulationId) -> Outcome {
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
    pub async fn logs(&self, id: &SimulationId) -> Result<Vec<Record>, String> {
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

    /// Retained engine events with a simulated time; server lifecycle,
    /// playback controls and preparation are host observations and are left out.
    pub async fn events(&self, id: &SimulationId) -> Result<Vec<(u64, Value)>, String> {
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
                            | EventKind::OutOfMemoryKill { .. }
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

    /// Resolve and run a project afresh with an explicit seed, at the CLI's
    /// default machine worker count.
    pub async fn run_project(&self, project: &Path) -> Result<SimulationId, String> {
        let stdout = self.cli(&[
            "run",
            project.to_str().ok_or("project path is not UTF-8")?,
            "--seed",
            &SEED.to_string(),
            "--detach",
            "--json",
        ])?;
        let submitted: Simulation = serde_json::from_slice(&stdout).map_err(err)?;
        let id = self.created(submitted.metadata.id);
        self.complete(&id).await?;
        Ok(id)
    }

    /// Save one captured checkpoint, with every binary input, as a checkpoint file.
    pub async fn export_checkpoint(
        &self,
        id: &SimulationId,
        checkpoint: &str,
        bundle: &Path,
    ) -> Outcome {
        let mut file = tokio::fs::File::create_new(bundle).await.map_err(err)?;
        self.client
            .export_checkpoint_bundle(
                id,
                checkpoint,
                &mut file,
                seedbox_api::BUNDLE_MAX_BYTES,
                || false,
            )
            .await
            .map_err(|e| format!("export {id} checkpoint {checkpoint}: {e}"))?;
        file.sync_all().await.map_err(err)
    }

    /// Restore a checkpoint file's inputs as a simulation document to submit.
    pub async fn import_checkpoint(&self, bundle: &Path) -> Result<Value, String> {
        let file = tokio::fs::File::open(bundle).await.map_err(err)?;
        let definition = self
            .client
            .import_checkpoint_bundle(file, seedbox_api::BUNDLE_MAX_BYTES, || false)
            .await
            .map_err(|e| format!("import {}: {e}", bundle.display()))?;
        let spec = seedbox_api::SimulationSpec::resolved(definition)?;
        Ok(serde_json::json!({ "spec": spec }))
    }

    /// Branch from a checkpoint as a new simulation document.
    pub async fn checkpoint_document(
        &self,
        id: &SimulationId,
        checkpoint: &str,
    ) -> Result<Value, String> {
        let resource = self
            .client
            .checkpoint_definition(id, checkpoint)
            .await
            .map_err(|e| format!("{id}: checkpoint {checkpoint}: {e}"))?;
        serde_json::to_value(&resource).map_err(err)
    }

    /// A completed run's recorded definition (external controls included).
    pub async fn recorded_document(&self, id: &SimulationId) -> Result<Value, String> {
        serde_json::to_value(self.client.describe(id).await.map_err(err)?).map_err(err)
    }
}

/// The fixture owns its server; `--full-state` makes comparisons include full state.
pub struct Server(Child);
impl Server {
    pub fn start(cli: &Path, state: &Path) -> Result<Self, String> {
        let mut command = Command::new(cli);
        command.args([
            "server",
            "start",
            "--foreground",
            "--startup-handshake",
            "--full-state",
        ]);
        let mut server = Self(
            command
                .env_remove("SEEDBOX_VMM")
                .env("SEEDBOX_STATE", state)
                .stdout(Stdio::piped())
                .guarded_spawn()
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
        let ready: Value = serde_json::from_str(&ready).map_err(|e| format!("{e}: {ready}"))?;
        if ready["status"] != "ready" {
            return Err(format!("server failed to start: {ready}"));
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
        let pid = self.0.id();
        std::thread::scope(|scope| {
            let (done, completed) = std::sync::mpsc::channel();
            scope.spawn(move || {
                // Observe exit without reaping, so the PID stays owned until
                // this thread decides whether SIGKILL is needed.
                let result = loop {
                    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
                    // SAFETY: info is writable and pid names our unreaped child.
                    let rc = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            pid,
                            info.as_mut_ptr(),
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if rc == 0 {
                        break Ok(());
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break Err(error);
                    }
                };
                let _ = done.send(result);
            });
            if !matches!(completed.recv_timeout(Duration::from_secs(30)), Ok(Ok(()))) {
                let _ = self.0.kill();
            }
        });
        let _ = self.0.wait();
    }
}
