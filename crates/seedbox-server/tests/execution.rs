//! Public server lifecycle, observation and control transport with a Rust executor.
//! Every server, worker peer, content store and socket belongs to one test.
#![cfg(unix)]
use seedbox_api::{
    ControlCommand, ControlRequest, ControlState, EventKind, ExecutionTermination, Phase,
    PlaybackRate, Simulation, SimulationId, SimulationSpec, Stage, SubmitRequest, TestVerdict,
};
use seedbox_application::{
    execution::{
        CancellationToken, ExecutionBackend, ExecutionProfile, ExecutionReport, ExecutionRequest,
        PreparedImage, PreparedInputs,
    },
    image::Launch,
    store::ContentStore,
};
use seedbox_client::Client;
use seedbox_engine_protocol::{
    host_control::{BoundaryPosition, Command, Reply, Request},
    observation::{Kind, Observation},
};
use seedbox_server::{
    catalogue::Catalogue,
    execution::{BackendProvider, Backends, ExecutionService},
    manager::Manager,
    stream::Delivery,
};
use std::{
    collections::BTreeMap,
    fs,
    num::NonZeroUsize,
    os::unix::net::UnixDatagram,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

type Script = Arc<
    dyn for<'a> Fn(ExecutionRequest<'a>) -> seedbox_application::Result<ExecutionReport>
        + Send
        + Sync,
>;
struct Provider(Script);
struct Backend {
    profile: ExecutionProfile,
    script: Script,
}
impl BackendProvider for Provider {
    fn project(
        &self,
        _: &seedbox_application::config::Simulation,
        _: &ContentStore,
        _: &CancellationToken,
    ) -> seedbox_application::Result<Backends> {
        panic!("resolved inputs require no preparation")
    }
    fn retained(
        &self,
        inputs: &PreparedInputs,
        _: &ContentStore,
    ) -> seedbox_application::Result<Box<dyn ExecutionBackend>> {
        Ok(Box::new(Backend {
            profile: inputs.profile.clone(),
            script: self.0.clone(),
        }))
    }
}
impl ExecutionBackend for Backend {
    fn profile(&self) -> ExecutionProfile {
        self.profile.clone()
    }
    fn execute(
        &self,
        request: ExecutionRequest<'_>,
    ) -> seedbox_application::Result<ExecutionReport> {
        (self.script)(request)
    }
}
struct Server {
    directory: Arc<tempfile::TempDir>,
    manager: Arc<Manager>,
    client: Client,
    definition: seedbox_spec::definition::Definition,
    shutdown: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl Server {
    fn new(
        script: impl for<'a> Fn(ExecutionRequest<'a>) -> seedbox_application::Result<ExecutionReport>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        let directory = Arc::new(
            tempfile::Builder::new()
                .prefix("seedbox-server-test-")
                .tempdir_in("/tmp")
                .unwrap(),
        );
        let store = ContentStore::new(directory.path().join("store"));
        let definition = definition(directory.path(), &store);
        let manager = Arc::new(Manager::new(
            Catalogue::open(&directory.path().join("catalogue")).unwrap(),
        ));
        let execution = Arc::new(
            ExecutionService::new(
                manager.clone(),
                directory.path().into(),
                store,
                Arc::new(Provider(Arc::new(script))),
            )
            .unwrap(),
        );
        let socket = directory.path().join("api.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let shutdown = CancellationToken::default();
        let (owner, token, files) = (manager.clone(), shutdown.clone(), directory.clone());
        let task = tokio::spawn(async move {
            let _files = files; // Keep fixtures alive through cleanup even if an assertion panics.
            seedbox_server::serve(
                listener,
                owner,
                execution,
                NonZeroUsize::new(2).unwrap(),
                token,
            )
            .await
            .unwrap();
        });
        Self {
            directory,
            manager,
            client: Client::connect_to(&socket).unwrap(),
            definition,
            shutdown,
            task: Some(task),
        }
    }
    async fn submit(&self, seed: u64) -> SimulationId {
        let mut definition = self.definition.clone();
        definition.seed = seed;
        self.client
            .submit(&SubmitRequest {
                metadata: Default::default(),
                options: Default::default(),
                request_id: format!("run-{seed}"),
                spec: SimulationSpec::resolved(definition),
            })
            .await
            .unwrap()
            .metadata
            .id
    }
    async fn wait(&self, id: &SimulationId, predicate: impl Fn(&Simulation) -> bool) -> Simulation {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let resource = self.manager.describe(id).unwrap();
                if predicate(&resource) {
                    return resource;
                }
                assert_ne!(
                    resource.status.phase,
                    Phase::Completed,
                    "execution ended early: {resource:?}"
                );
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("server did not reach the expected state")
    }
    async fn receipt(&self, id: &SimulationId, request: ControlRequest) -> ControlState {
        self.client.request_control(id, &request).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let state = self
                    .client
                    .control_receipt(id, &request.request_id)
                    .await
                    .unwrap()
                    .state;
                if state != ControlState::Accepted {
                    return state;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn finish(mut self) {
        self.shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(8), self.task.take().unwrap())
            .await
            .unwrap()
            .unwrap();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}
fn definition(root: &Path, store: &ContentStore) -> seedbox_spec::definition::Definition {
    let rootfs = root.join("rootfs");
    fs::create_dir_all(rootfs.join("bin")).unwrap();
    fs::write(rootfs.join("bin/test"), b"application").unwrap();
    let init = root.join("init");
    fs::write(&init, b"init binary").unwrap();
    let launch = Launch {
        argv: vec!["/bin/test".into()],
        environment: Default::default(),
        working_dir: "/".into(),
        user: "0:0".into(),
        volumes: Vec::new(),
        disk_volumes: Vec::new(),
    };
    let recipe = seedbox_application::guest::launch_recipe(
        &launch,
        "10.0.0.1/24",
        "127.0.0.1 localhost\n",
        "oracle",
        &Default::default(),
        86400,
    )
    .unwrap();
    let archive = root.join("boot.cpio");
    seedbox_application::guest::build_initramfs(&rootfs, &init, &recipe, &archive).unwrap();
    let boot = store.put_file(&archive).unwrap();
    let profile = ExecutionProfile {
        backend: if cfg!(target_arch = "aarch64") {
            seedbox_application::execution::Backend::Hvf
        } else {
            seedbox_application::execution::Backend::Kvm
        },
        engine: store.put(b"engine").unwrap(),
        witness_mode: seedbox_application::execution::WitnessMode::Exits,
        semantic_parameters: seedbox_application::execution::SemanticParameters::new(1024),
    };
    let configuration = seedbox_application::config::parse_for_architecture(
        "name: test\nservices:\n  oracle:\n    image: example/oracle\n  other:\n    image: example/other\n",
        Path::new("/project/compose.yaml"), &Default::default(), if cfg!(target_arch = "aarch64") { seedbox_application::guest::Architecture::Arm64 } else { seedbox_application::guest::Architecture::Amd64 }).unwrap();
    let inputs = PreparedInputs {
        wall_clock_epoch: Some(86400),
        kernel: store.put(b"kernel").unwrap(),
        images: ["oracle", "other"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    PreparedImage {
                        manifest: boot.clone(),
                        config: boot.clone(),
                        layers: Vec::new(),
                        launch: launch.clone(),
                    },
                )
            })
            .collect(),
        placement: BTreeMap::from([
            ("oracle".into(), "oracle".into()),
            ("other".into(), "other".into()),
        ]),
        guests: BTreeMap::from([("oracle".into(), boot.clone()), ("other".into(), boot)]),
        profile,
        auxiliary: Default::default(),
        provenance: Default::default(),
    };
    seedbox_application::definition::resolve(&configuration, &inputs, 1, Vec::new()).unwrap()
}
fn error(message: &str) -> seedbox_application::Result<ExecutionReport> {
    Err(seedbox_application::Error::Execution(message.into()))
}
fn emit(
    request: &ExecutionRequest<'_>,
    observations: impl IntoIterator<Item = (u64, &'static str, u32, u64, Kind)>,
) {
    let socket = UnixDatagram::unbound().unwrap();
    seedbox_engine_protocol::observation::configure_socket(&socket).unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket.connect(request.observations.unwrap()).unwrap();
    for (sequence, machine, incarnation, simulated_time_ns, kind) in observations {
        socket
            .send(
                &serde_json::to_vec(&Observation {
                    sequence,
                    machine: machine.into(),
                    incarnation,
                    simulated_time_ns,
                    kind,
                })
                .unwrap(),
            )
            .unwrap();
    }
}
fn until(cancel: &CancellationToken, predicate: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(6);
    while !predicate() && !cancel.is_cancelled() {
        assert!(Instant::now() < deadline, "executor gate timed out");
        thread::sleep(Duration::from_millis(1));
    }
}
fn reply(id: u64) -> Reply {
    Reply {
        id,
        position: BoundaryPosition {
            boundary_index: 3,
            completed_rounds: 2,
            simulated_time_ns: 100,
        },
        terminal: false,
        application_ready: true,
        rate: PlaybackRate::Unlimited,
        achieved_rate: None,
        stepping: false,
        error: None,
        detail: None,
        intervention: None,
        checkpoint: None,
    }
}
struct Peer {
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
    _directory: tempfile::TempDir,
}
impl Peer {
    fn new(
        output: &Path,
        mut handler: impl FnMut(Request) -> Option<Reply> + Send + 'static,
    ) -> Self {
        Self::raw(output, move |request| {
            handler(request).map(|reply| serde_json::to_vec(&reply).unwrap())
        })
    }
    fn raw(
        output: &Path,
        mut handler: impl FnMut(Request) -> Option<Vec<u8>> + Send + 'static,
    ) -> Self {
        let directory = tempfile::Builder::new()
            .prefix("seedbox-worker-test-")
            .tempdir_in("/tmp")
            .unwrap();
        let endpoint = directory.path().join("socket");
        let socket = UnixDatagram::bind(&endpoint).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        fs::write(
            output.join("host-control"),
            endpoint.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let task = thread::spawn(move || {
            let mut bytes = [0; seedbox_engine_protocol::host_control::MAX_DATAGRAM + 1];
            while !done.load(Ordering::Acquire) {
                let (length, peer) = match socket.recv_from(&mut bytes) {
                    Ok(pair) => pair,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                        ) =>
                    {
                        continue
                    }
                    Err(e) => panic!("worker receive: {e}"),
                };
                let request: Request = serde_json::from_slice(&bytes[..length]).unwrap();
                if let Some(reply) = handler(request) {
                    let bytes = reply;
                    // A retired client can close while the reply is in flight.
                    if let Err(e) = socket.send_to(&bytes, peer.as_pathname().unwrap()) {
                        assert!(
                            matches!(
                                e.kind(),
                                std::io::ErrorKind::NotFound
                                    | std::io::ErrorKind::ConnectionRefused
                            ),
                            "{e}"
                        );
                    }
                }
            }
        });
        Self {
            stop,
            task: Some(task),
            _directory: directory,
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.task.take().unwrap().join();
        if !thread::panicking() {
            result.unwrap();
        }
    }
}

#[tokio::test]
async fn committed_logs_oracle_and_oversized_events_survive_finalization_and_restart() {
    for code in [0, 7] {
        let release = Arc::new(AtomicBool::new(false));
        let gate = release.clone();
        let server = Server::new(move |request| {
            emit(
                &request,
                [
                    (1, "oracle", 1, 42, Kind::LogGap { records: 1 }),
                    (2, "unknown", 9, 43, Kind::MachineReady),
                    (3, "other", 0, 44, Kind::MachineExited { code: 99 }),
                    (4, "oracle", 1, 45, Kind::MachineExited { code }),
                    (
                        5,
                        "oracle",
                        1,
                        46,
                        Kind::OverrideRejected {
                            input_key: "large".into(),
                            at_ns: 46,
                            reason: "x".repeat(9000),
                        },
                    ),
                    (6, "oracle", 1, 47, Kind::MachineReady),
                    (7, "", 0, 0, Kind::Closed),
                ],
            );
            until(request.cancellation, || gate.load(Ordering::Acquire));
            error(&"x".repeat(9000))
        });
        let id = server.submit(1).await;
        server
            .wait(&id, |r| {
                r.status.oracle_exit_code == Some(code)
                    && server.manager.world(&id).unwrap().simulated_time_ns >= 47
            })
            .await;
        let world = server.manager.world(&id).unwrap();
        assert_eq!(world.machines.len(), 2);
        assert!(!world.machines.iter().any(|m| m.name == "unknown"));
        assert_eq!(
            world
                .machines
                .iter()
                .find(|m| m.name == "oracle")
                .unwrap()
                .incarnation,
            1
        );
        assert!(!server.manager.cancellation(&id).unwrap().is_cancelled());
        release.store(true, Ordering::Release);
        let completed = server
            .wait(&id, |r| r.status.phase == Phase::Completed)
            .await;
        assert_eq!(
            completed.status.verdict(),
            if code == 0 {
                TestVerdict::Passed
            } else {
                TestVerdict::Failed
            }
        );
        assert!(matches!(
            completed.status.termination,
            Some(ExecutionTermination::Error { .. })
        ));
        let retained_world = server.manager.world(&id).unwrap();
        assert_eq!(retained_world.machines, world.machines);
        assert_eq!(retained_world.simulated_time_ns, world.simulated_time_ns);
        let mut events = server.manager.events(&id, false).unwrap();
        let mut observed = Vec::new();
        while let Delivery::Record(record) = events.recv().await {
            observed.push(record.payload.event.clone());
        }
        assert!(
            observed
                .iter()
                .filter(|event| **event == EventKind::SourceUnavailable)
                .count()
                >= 2
        );
        assert!(observed.contains(&EventKind::MachineReady {
            machine: "oracle".into(),
            incarnation: 1
        }));
        let directory = server.directory.clone();
        server.finish().await;
        let reopened = Manager::new(Catalogue::open(&directory.path().join("catalogue")).unwrap());
        assert_eq!(reopened.describe(&id).unwrap(), completed);
    }
}

#[tokio::test]
async fn committed_playback_closes_boot_stage_once_and_rejects_stale_samples() {
    let mut initial = reply(0);
    initial.application_ready = false;
    let sample = Arc::new(Mutex::new(initial));
    let selected = sample.clone();
    let connected = Arc::new(AtomicBool::new(false));
    let ready = connected.clone();
    let server = Server::new(move |request| {
        let selected = selected.clone();
        let _peer = Peer::new(request.output, move |request| {
            let mut value = selected.lock().unwrap().clone();
            value.id = request.id;
            Some(value)
        });
        ready.store(true, Ordering::Release);
        until(request.cancellation, || false);
        error("stopped")
    });
    let id = server.submit(1).await;
    server
        .wait(&id, |r| {
            r.status.stage == Some(Stage::Booting) && connected.load(Ordering::Acquire)
        })
        .await;
    for (key, position, ready) in [
        ("initial", 3, false),
        ("stale", 2, true),
        ("ready", 4, true),
        ("transition", 5, true),
        ("old", 3, false),
    ] {
        let mut observation = reply(0);
        observation.position.boundary_index = position;
        observation.position.completed_rounds = if position == 2 {
            1
        } else if position >= 4 {
            3
        } else {
            2
        };
        if position == 5 {
            observation.position.simulated_time_ns = 99;
        }
        observation.application_ready = ready;
        *sample.lock().unwrap() = observation;
        // A receipt is published only after the driver processes the sample.
        assert!(matches!(
            server
                .receipt(
                    &id,
                    ControlRequest {
                        request_id: key.into(),
                        command: ControlCommand::Step
                    }
                )
                .await,
            ControlState::Acknowledged { error: None, .. }
        ));
        let resource = server.manager.describe(&id).unwrap();
        assert_eq!(
            resource.status.stage,
            Some(if position <= 3 && key != "old" {
                Stage::Booting
            } else {
                Stage::Executing
            })
        );
        if key == "old" {
            assert_eq!(
                server
                    .client
                    .playback(&id)
                    .await
                    .unwrap()
                    .state
                    .position
                    .boundary_index,
                5
            );
        }
    }
    assert!(server.manager.describe(&id).unwrap().status.stage_timings[&Stage::Booting].complete);
    server.finish().await;
}

fn proposal(id: u64, key: &str, action: seedbox_control::Action, fault_id: u32) -> Reply {
    let mut reply = reply(id);
    reply.intervention = Some(seedbox_engine_protocol::intervention::Intervention {
        request_id: key.into(),
        position: reply.position,
        plan: seedbox_control::Plan {
            id: fault_id,
            at_ns: 100,
            deadline_ns: 100 + seedbox_control::PROCESS_EFFECT_TIMEOUT_NS,
            action,
        },
    });
    reply
}
#[tokio::test]
async fn fault_retention_precedes_commit_retries_and_checkpoint_freezes_its_prefix() {
    let finish = Arc::new(AtomicBool::new(false));
    let done = finish.clone();
    let attempts = Arc::new(Mutex::new(Vec::new()));
    let attempts_seen = attempts.clone();
    let run_path = Arc::new(Mutex::new(None));
    let published = run_path.clone();
    let server = Server::new(move |request| {
        let run = request.output.parent().unwrap().to_owned();
        *published.lock().unwrap() = Some(run.clone());
        let history = run.join("interventions.json");
        let attempts = attempts_seen.clone();
        let mut staged = None;
        let mut fault_id = 0;
        let _peer = Peer::new(request.output, move |request| {
            let mut response = reply(request.id);
            match &request.command {
                Command::PrepareFault {
                    request_id, action, ..
                } => {
                    fault_id += 1;
                    response = proposal(request.id, request_id, action.clone(), fault_id);
                    staged = response.intervention.clone();
                }
                Command::CommitFault { .. } => {
                    let retained = seedbox_artifacts::interventions::read(&history).unwrap();
                    assert_eq!(retained.entries.last(), staged.as_ref());
                    let mut attempts = attempts.lock().unwrap();
                    attempts.push(request.clone());
                    // Lose the first reply; retries must keep the exact wire ID and payload.
                    if attempts.len() == 1 {
                        return None;
                    }
                    response.intervention = staged.clone();
                }
                Command::Checkpoint { request_id } => {
                    response.checkpoint =
                        Some(seedbox_engine_protocol::host_control::CheckpointStatus {
                            request_id: request_id.clone(),
                            position: response.position,
                            state: seedbox_engine_protocol::host_control::CheckpointState::Captured,
                        });
                }
                _ => {}
            }
            Some(response)
        });
        until(request.cancellation, || done.load(Ordering::Acquire));
        error("finished")
    });
    let id = server.submit(1).await;
    server
        .wait(&id, |r| r.status.stage == Some(Stage::Executing))
        .await;
    for (key, command) in [
        (
            "cut",
            ControlCommand::InjectFault {
                action: seedbox_control::Action::Heal {
                    handle: "cut".into(),
                },
            },
        ),
        ("saved", ControlCommand::Checkpoint),
        (
            "later",
            ControlCommand::InjectFault {
                action: seedbox_control::Action::Heal {
                    handle: "later".into(),
                },
            },
        ),
    ] {
        assert!(matches!(
            server
                .receipt(
                    &id,
                    ControlRequest {
                        request_id: key.into(),
                        command
                    }
                )
                .await,
            ControlState::Acknowledged { error: None, .. }
        ));
    }
    {
        let retries = attempts.lock().unwrap();
        assert!(retries.len() >= 3);
        assert_eq!(retries[0], retries[1]);
    }
    let run = run_path.lock().unwrap().clone().unwrap();
    let frozen =
        seedbox_artifacts::interventions::read(&run.join("checkpoint-saved-interventions.json"))
            .unwrap();
    let final_history =
        seedbox_artifacts::interventions::read(&run.join("interventions.json")).unwrap();
    assert_eq!(frozen.entries, final_history.entries[..1]);
    assert_eq!(final_history.entries.len(), 2);
    finish.store(true, Ordering::Release);
    server
        .wait(&id, |r| r.status.phase == Phase::Completed)
        .await;
    assert!(server
        .client
        .request_control(
            &id,
            &ControlRequest {
                request_id: "stale".into(),
                command: ControlCommand::Checkpoint
            }
        )
        .await
        .is_err());
    let output = run
        .join("output")
        .join(seedbox_application::engine_adapter::MACHINE_OUTPUT_DIR);
    fs::create_dir_all(output.join(".checkpoint-machines-test")).unwrap();
    let arm = output.join(".arm-checkpoint-test");
    let x86 = output.join(".checkpoint-machines-test/0.snapshot");
    for path in [&arm, &x86, &output.join("keep")] {
        fs::write(path, b"data").unwrap();
    }
    let metadata = output.join("checkpoint-saved.json");
    fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({"state":{"world":{"machines":[
            {"checkpoint":{"artifact":arm}}, {"path":x86}
        ]}}}))
        .unwrap(),
    )
    .unwrap();
    server.client.delete_checkpoint(&id, "saved").await.unwrap();
    server.client.delete_checkpoint(&id, "saved").await.unwrap();
    assert!(!arm.exists() && !x86.exists() && !metadata.exists());
    assert!(!output.join(".checkpoint-machines-test").exists());
    assert_eq!(fs::read(output.join("keep")).unwrap(), b"data");
    assert!(!run.join("checkpoint-saved-interventions.json").exists());
    fs::write(
        &metadata,
        serde_json::to_vec(
            &serde_json::json!({"state":{"world":{"machines":[{"path":run.join("outside")} ]}}}),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(server.client.delete_checkpoint(&id, "saved").await.is_err());
    assert!(metadata.exists());
    server.client.delete(&id).await.unwrap();
    assert!(!run.exists());
    server.finish().await;
}

#[tokio::test]
async fn failed_journal_replacement_cancels_only_its_execution_and_never_commits() {
    let commits = Arc::new(AtomicBool::new(false));
    let observed = commits.clone();
    let server = Server::new(move |request| {
        let path = request.output.parent().unwrap().join("interventions.json");
        let broken = request.seed == 1;
        let observed = observed.clone();
        let _peer = Peer::new(request.output, move |request| match &request.command {
            Command::PrepareFault {
                request_id, action, ..
            } if broken => {
                fs::remove_file(&path).unwrap();
                fs::create_dir(&path).unwrap();
                Some(proposal(request.id, request_id, action.clone(), 1))
            }
            Command::CommitFault { .. } if broken => {
                observed.store(true, Ordering::Release);
                Some(reply(request.id))
            }
            _ => Some(reply(request.id)),
        });
        emit(
            &request,
            [
                (1, "oracle", 0, 10, Kind::MachineExited { code: 0 }),
                (2, "", 0, 0, Kind::Closed),
            ],
        );
        until(request.cancellation, || false);
        if broken {
            fs::remove_dir(request.output.parent().unwrap().join("interventions.json")).unwrap();
            fs::write(
                request.output.parent().unwrap().join("interventions.json"),
                seedbox_engine_protocol::intervention::History::default()
                    .encode()
                    .unwrap(),
            )
            .unwrap();
        }
        error("stopped")
    });
    let broken = server.submit(1).await;
    let healthy = server.submit(2).await;
    for id in [&broken, &healthy] {
        server
            .wait(id, |r| r.status.stage == Some(Stage::Executing))
            .await;
    }
    server
        .client
        .request_control(
            &broken,
            &ControlRequest {
                request_id: "cut".into(),
                command: ControlCommand::InjectFault {
                    action: seedbox_control::Action::Heal {
                        handle: "cut".into(),
                    },
                },
            },
        )
        .await
        .unwrap();
    let failed = server
        .wait(&broken, |r| r.status.phase == Phase::Completed)
        .await;
    assert_eq!(failed.status.verdict(), TestVerdict::Passed);
    assert!(
        matches!(failed.status.termination, Some(ExecutionTermination::Error { ref message }) if message.contains("worker control"))
    );
    assert!(!commits.load(Ordering::Acquire));
    assert!(!server
        .manager
        .cancellation(&healthy)
        .unwrap()
        .is_cancelled());
    assert!(matches!(
        server
            .receipt(
                &healthy,
                ControlRequest {
                    request_id: "step".into(),
                    command: ControlCommand::Step
                }
            )
            .await,
        ControlState::Acknowledged { error: None, .. }
    ));
    assert!(!server.shutdown.is_cancelled());
    server.finish().await;
}

#[tokio::test]
async fn observation_drain_preserves_binary_bytes_gaps_frames_and_uncertain_tail() {
    for closed in [false, true] {
        let frame = seedbox_api::Frame {
            from: "oracle".into(),
            to: "other".into(),
            sent_ns: 25,
            due_ns: Some(26),
            bytes: 60,
            fate: seedbox_api::FrameFate::Scheduled,
        };
        let transmitted = frame.clone();
        let send = Arc::new(AtomicBool::new(false));
        let sending = send.clone();
        let server = Server::new(move |request| {
            until(request.cancellation, || sending.load(Ordering::Acquire));
            let mut records = vec![
                (1, "oracle", 2, 10, Kind::MachineReady),
                (
                    2,
                    "oracle",
                    2,
                    15,
                    Kind::MachineLifecycle {
                        transition: seedbox_api::MachineTransition::Paused,
                    },
                ),
                (4, "oracle", 2, 20, Kind::MachineExited { code: 7 }),
                (
                    5,
                    "oracle",
                    2,
                    30,
                    Kind::FaultChanged {
                        id: 1,
                        action: "stop".into(),
                        machine: Some("oracle".into()),
                        status: seedbox_api::FaultStatus::Unavailable,
                        applied_time_ns: None,
                        plan: Some(Box::new(seedbox_api::FaultAction::Stop {
                            machine: "oracle".into(),
                        })),
                        input_key: None,
                    },
                ),
                (
                    6,
                    "oracle",
                    2,
                    30,
                    Kind::Frames {
                        frames: vec![transmitted.clone()],
                    },
                ),
                (
                    7,
                    "oracle",
                    2,
                    123,
                    Kind::Log(seedbox_engine_protocol::application_log::Chunk {
                        stream: seedbox_engine_protocol::application_log::Stream::Stderr,
                        bytes: vec![0, 255, 10],
                    }),
                ),
                (9, "oracle", 2, 123, Kind::LogGap { records: 17 }),
            ];
            if closed {
                records.push((10, "", 0, 0, Kind::Closed));
            }
            emit(&request, records);
            error("no final report")
        });
        let id = server.submit(1).await;
        server.wait(&id, |r| r.status.phase == Phase::Running).await;
        let mut frames = server.manager.frames(&id).unwrap();
        send.store(true, Ordering::Release);

        let completed = server
            .wait(&id, |r| r.status.phase == Phase::Completed)
            .await;
        assert_eq!(completed.status.oracle_exit_code, Some(7), "{completed:?}");
        let mut events = server.manager.events(&id, false).unwrap();
        let mut observed = Vec::new();
        while let Delivery::Record(record) = events.recv().await {
            observed.push(record.payload.event.clone());
        }
        assert!(observed.contains(&EventKind::SourceGap { first: 3, last: 3 }));
        assert!(observed.contains(&EventKind::SourceGap { first: 8, last: 8 }));
        assert_eq!(observed.contains(&EventKind::SourceUnavailable), !closed);
        assert!(observed.contains(&EventKind::FaultChanged {
            id: 1,
            action: "stop".into(),
            machine: Some("oracle".into()),
            status: seedbox_api::FaultStatus::Unavailable,
            applied_time_ns: None,
            plan: Some(Box::new(
                serde_json::to_value(seedbox_api::FaultAction::Stop {
                    machine: "oracle".into()
                })
                .unwrap()
            )),
            input_key: None
        }));
        let mut logs = server.manager.logs(&id, "oracle", false).unwrap();
        let mut entries = Vec::new();
        while let Delivery::Record(record) = logs.recv().await {
            entries.push(record.payload.clone());
        }
        assert!(entries.contains(&seedbox_api::LogEntry::Bytes {
            machine: "oracle".into(),
            incarnation: 2,
            simulated_time_ns: 123,
            stream: seedbox_api::LogStream::Stderr,
            bytes: vec![0, 255, 10]
        }));
        assert!(entries.contains(&seedbox_api::LogEntry::MachineGap {
            machine: "oracle".into(),
            incarnation: 2,
            simulated_time_ns: 123,
            records: 17
        }));
        let Delivery::Record(batch) = frames.recv().await else {
            panic!("frame batch missing")
        };
        assert_eq!(batch.payload.frames, [frame]);
        assert_eq!(batch.payload.simulated_time_ns, 30);
        assert!(matches!(frames.recv().await, Delivery::Closed));
        server.finish().await;
    }
}

#[tokio::test]
async fn malformed_control_reply_preserves_oracle_and_isolates_other_simulations() {
    let server = Server::new(move |request| {
        let broken = request.seed == 1;
        let _peer = Peer::raw(request.output, move |request| {
            Some(if broken {
                b"malformed reply".to_vec()
            } else {
                serde_json::to_vec(&reply(request.id)).unwrap()
            })
        });
        emit(
            &request,
            [
                (1, "oracle", 0, 10, Kind::MachineExited { code: 0 }),
                (2, "", 0, 0, Kind::Closed),
            ],
        );
        until(request.cancellation, || false);
        error("stopped")
    });
    let broken = server.submit(1).await;
    let healthy = server.submit(2).await;
    let failed = server
        .wait(&broken, |r| r.status.phase == Phase::Completed)
        .await;
    assert_eq!(failed.status.verdict(), TestVerdict::Passed);
    assert!(
        matches!(failed.status.termination, Some(ExecutionTermination::Error { ref message }) if message.contains("worker control"))
    );
    assert!(!server.shutdown.is_cancelled());
    server
        .wait(&healthy, |r| r.status.stage == Some(Stage::Executing))
        .await;
    assert!(matches!(
        server
            .receipt(
                &healthy,
                ControlRequest {
                    request_id: "step".into(),
                    command: ControlCommand::Step
                }
            )
            .await,
        ControlState::Acknowledged { error: None, .. }
    ));
    server.finish().await;
}

#[tokio::test]
async fn deletion_and_completion_drain_in_flight_control_before_retiring_files() {
    for delete in [false, true] {
        let finish = Arc::new(AtomicBool::new(false));
        let end = finish.clone();
        let pending = Arc::new(AtomicBool::new(false));
        let in_flight = pending.clone();
        let allow = Arc::new(AtomicBool::new(false));
        let acknowledged = allow.clone();
        let peer_owner = Arc::new(Mutex::new(None));
        let external_peer = peer_owner.clone();
        let attempt = Arc::new(Mutex::new(None));
        let published_attempt = attempt.clone();
        let server = Server::new(move |request| {
            let run = request.output.parent().unwrap().to_owned();
            *published_attempt.lock().unwrap() = Some(run.clone());
            let (pending, allow) = (in_flight.clone(), acknowledged.clone());
            *external_peer.lock().unwrap() = Some(Peer::new(request.output, move |request| {
                if request.command == Command::Step {
                    pending.store(true, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !allow.load(Ordering::Acquire) {
                        assert!(Instant::now() < deadline);
                        thread::sleep(Duration::from_millis(1));
                    }
                    fs::write(run.join("last-control-io"), b"finished").unwrap();
                }
                Some(reply(request.id))
            }));
            until(request.cancellation, || end.load(Ordering::Acquire));
            error("finished")
        });
        let id = server.submit(1).await;
        server
            .wait(&id, |r| r.status.stage == Some(Stage::Executing))
            .await;
        server
            .client
            .request_control(
                &id,
                &ControlRequest {
                    request_id: "step".into(),
                    command: ControlCommand::Step,
                },
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !pending.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        finish.store(true, Ordering::Release);
        let deletion = if delete {
            let client = server.client.clone();
            let id = id.clone();
            Some(tokio::spawn(async move {
                client.delete(&id).await.unwrap();
            }))
        } else {
            None
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_ne!(
            server.manager.describe(&id).unwrap().status.phase,
            Phase::Completed
        );
        let run = attempt.lock().unwrap().clone().unwrap();
        assert!(run.exists());
        allow.store(true, Ordering::Release);
        if let Some(deletion) = deletion {
            tokio::time::timeout(Duration::from_secs(3), deletion)
                .await
                .unwrap()
                .unwrap();
        } else {
            server
                .wait(&id, |r| r.status.phase == Phase::Completed)
                .await;
            assert_eq!(fs::read(run.join("last-control-io")).unwrap(), b"finished");
            assert!(server
                .client
                .request_control(
                    &id,
                    &ControlRequest {
                        request_id: "stale".into(),
                        command: ControlCommand::Checkpoint
                    }
                )
                .await
                .is_err());
            server.client.delete(&id).await.unwrap();
        }
        assert!(!run.exists());
        assert!(server.manager.describe(&id).is_err());
        drop(peer_owner.lock().unwrap().take());
        server.finish().await;
    }
}
