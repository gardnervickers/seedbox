mod public_actions;
mod run;
mod state_results;
use super::*;
use seedbox_driver::driver::{Action, NetworkConfig, Target};
use seedbox_engine_protocol::driver_world as wire;
use seedbox_machine::{
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    machine::StopReason as MachineStop,
    pv::{Output, Request as Input, Stream},
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[derive(Clone, serde::Serialize)]
struct Data {
    name: String,
    at: u64,
    first: bool,
    dead: bool,
    crashes: u64,
    random: [u8; 8],
}
struct Mock {
    data: Data,
    runs: Arc<AtomicUsize>,
}
struct Saved(Data);
impl Machine for Mock {
    type Context = ();
    type Error = String;
    type Snapshot = Saved;
    fn run(
        &mut self,
        c: &mut seedbox_machine::machine::RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<MachineStop, String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Input::Crash => {
                    self.data.dead = true;
                    self.data.crashes += 1
                }
                Input::Reboot => {
                    self.data.dead = false;
                    self.data.first = true
                }
                _ => {}
            }
        }
        self.data.at = c.clock.now();
        if self.data.dead {
            return Ok(MachineStop::Exited {
                reached: Timestamp::from_nanos(self.data.at),
                reason: ExitReason::Crashed,
            });
        }
        if self.data.first {
            c.outbox
                .emit(Output {
                    at: Timestamp::from_nanos(c.clock.now()),
                    payload: Response::Ready,
                })
                .unwrap();
            self.data.first = false;
        }
        if upto.as_nanos() > c.clock.now() {
            c.rng.fill_bytes(&mut self.data.random);
            c.outbox
                .emit(Output {
                    at: upto,
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: self.data.random.to_vec(),
                    },
                })
                .unwrap();
        }
        self.data.at = upto.as_nanos();
        Ok(MachineStop::BoundaryReached)
    }
    fn snapshot(&mut self) -> std::result::Result<Saved, String> {
        Ok(Saved(self.data.clone()))
    }
}
fn definition(name: &str) -> MachineDefinition {
    MachineDefinition {
        name: name.into(),
        resources: Resources {
            memory_bytes: 128 << 20,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar::Builder::new(Vec::new()).into_inner().unwrap())
                .unwrap(),
            launch: Launch {
                argv: vec!["/application".into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}
fn inventory(root: &Path) -> Inventory {
    let artifact = |name: &str| {
        let path = root.join(name);
        std::fs::write(&path, name).unwrap();
        wire::Artifact {
            sha256: ContentDigest::of(name.as_bytes()).hex().into(),
            length: name.len() as u64,
            artifact: path.to_str().unwrap().into(),
        }
    };
    Inventory::Pv {
        policy: None,
        executable: artifact("worker"),
        kernel: artifact("kernel"),
        init: artifact("init"),
        command_line: artifact("command-line"),
        progress: artifact("progress"),
        runtime: artifact("runtime"),
        formatter: None,
        environments: Default::default(),
        boot_archives: Default::default(),
        blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
        epoch_seconds: 1234,
    }
}
fn worker(workers: usize) -> (tempfile::TempDir, Worker<Mock>, Arc<AtomicUsize>) {
    let root = tempfile::tempdir().unwrap();
    let inventory = inventory(root.path());
    let output = root.path().join("output");
    std::fs::create_dir(&output).unwrap();
    let runs = Arc::new(AtomicUsize::new(0));
    let spy = runs.clone();
    let constructed: Constructed = Arc::default();
    let ledger = constructed.clone();
    let driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        42,
        || Ok(()),
        definition("oracle"),
        vec![definition("n1")],
        vec![],
        NetworkConfig::default(),
        move |definition| {
            ledger.lock().unwrap().push_back(definition.name.clone());
            Ok(Mock {
                data: Data {
                    name: definition.name.clone(),
                    at: 0,
                    first: true,
                    dead: false,
                    crashes: 0,
                    random: [0; 8],
                },
                runs: spy.clone(),
            })
        },
    )
    .unwrap();
    let state = wire::State {
        world: wire::World {
            seed: 42,
            machine_names: vec![],
            input_program: Default::default(),
            intervention_next: 0,
            readiness: wire::Readiness {
                initial: vec!["n1".into(), "oracle".into()],
                seen: vec![],
                latched: false,
            },
            publication: Default::default(),
            position: Default::default(),
        },
    };
    let mut worker = Worker::new(
        driver,
        state,
        inventory,
        output,
        BTreeMap::new(),
        constructed,
        None,
        |saved, writer| {
            serde_json::to_writer(writer, &saved.0)?;
            Ok(())
        },
    )
    .unwrap();
    worker.flush().unwrap();
    (root, worker, runs)
}
fn request(id: u64, command: Command) -> Request {
    Request { id, command }
}
fn playback(worker: &mut Worker<Mock>, id: u64, command: Command) -> Reply {
    let Dispatch::Reply(reply) = worker.playback_request(request(id, command)).unwrap() else {
        panic!("expected playback reply")
    };
    reply
}
fn fingerprint(worker: &mut Worker<Mock>) -> Vec<u8> {
    let capture = worker
        .driver
        .checkpoint_map(|saved| {
            let bytes = serde_json::to_vec(&saved.0).unwrap();
            let mut digest = [0; 32];
            hex::decode_to_slice(ContentDigest::of(&bytes).hex(), &mut digest).unwrap();
            Ok::<_, String>(driver::SnapshotReference::new(digest, bytes.len() as u64))
        })
        .unwrap();
    let mut bytes = vec![];
    capture.write_to(&mut bytes).unwrap();
    bytes
}
fn crash_proposal(worker: &mut Worker<Mock>, key: &str) -> PreparedInput {
    worker
        .driver
        .with_barrier(|barrier| {
            barrier.prepare_input(
                InputKey::new(key).unwrap(),
                Action::Crash(Target::Name("n1".into())),
            )
        })
        .unwrap()
}

#[test]
fn authored_same_time_entries_drain_individually_and_rejection_consumes_no_fault_id() {
    for workers in [1, 4] {
        let (_root, mut worker, _) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        let actions = [
            (
                "crash",
                seedbox_control::Action::Crash {
                    machine: "n1".into(),
                },
                Action::Crash(Target::Name("n1".into())),
            ),
            (
                "reboot",
                seedbox_control::Action::Reboot {
                    machine: "n1".into(),
                },
                Action::Reboot(Target::Name("n1".into())),
            ),
            (
                "invalid-resume",
                seedbox_control::Action::Resume {
                    machine: "n1".into(),
                },
                Action::Resume(Target::Name("n1".into())),
            ),
            (
                "pause",
                seedbox_control::Action::Pause {
                    machine: "n1".into(),
                },
                Action::Pause(Target::Name("n1".into())),
            ),
        ];
        worker.state.world.input_program.inputs = actions
            .iter()
            .map(
                |(key, action, _)| seedbox_engine_protocol::input_program::TimedInput {
                    key: (*key).into(),
                    at_ns: 10,
                    timeout_ns: 10,
                    action: action.clone(),
                },
            )
            .collect();
        for (index, (_, _, action)) in actions.into_iter().enumerate() {
            assert!(worker.authored(action, None).unwrap());
            assert_eq!(worker.state.world.input_program.next, index + 1);
            assert_eq!(
                worker.playback().unwrap().position.at,
                Timestamp::from_nanos(10)
            );
        }
        let program = &worker.state.world.input_program;
        assert_eq!(
            program
                .admissions
                .iter()
                .map(|row| row.plan.id)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert_eq!(
            program
                .admissions
                .iter()
                .map(|row| row.key.as_str())
                .collect::<Vec<_>>(),
            ["crash", "reboot", "pause"]
        );
        assert_eq!(program.rejections.len(), 1);
        assert_eq!(program.rejections[0].key, "invalid-resume");
        program.validate_at(10).unwrap();
        let machine = *worker
            .names
            .iter()
            .find(|(_, name)| name.as_str() == "n1")
            .unwrap()
            .0;
        assert!(worker
            .driver
            .with_barrier(|barrier| barrier.paused(machine))
            .unwrap());
    }
}

#[test]
fn paused_real_driver_prepare_journal_commit_receipt_holds_effect_and_releases_once() {
    for workers in [1, 4] {
        let (root, mut worker, runs) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        assert!(worker.state.world.readiness.latched);
        assert_eq!(
            playback(
                &mut worker,
                1,
                Command::SetRate {
                    rate: host::PlaybackRate::Paused
                }
            )
            .error,
            None
        );
        let before = fingerprint(&mut worker);
        let calls = runs.load(Ordering::SeqCst);
        let state = worker.state.clone();
        let prepare = request(
            2,
            Command::PrepareFault {
                request_id: "crash".into(),
                action: seedbox_control::Action::Crash {
                    machine: "n1".into(),
                },
                timeout_ns: 100,
            },
        );
        let proposal = crash_proposal(&mut worker, "crash");
        let reply = worker.prepared(&prepare, proposal).unwrap();
        let record = reply.intervention.unwrap();
        assert_eq!(fingerprint(&mut worker), before);
        assert_eq!(worker.state, state);
        assert_eq!(runs.load(Ordering::SeqCst), calls);
        let denied = playback(
            &mut worker,
            3,
            Command::Checkpoint {
                request_id: "too-early".into(),
            },
        );
        assert_eq!(denied.error, Some(ControlError::FaultPending));
        assert!(worker.capture_pending().unwrap().is_none());
        let journal_path = root.path().join("interventions.json");
        let mut journal =
            seedbox_artifacts::interventions::Journal::initialize(&journal_path).unwrap();
        journal.append(record.clone()).unwrap();
        assert_eq!(
            seedbox_artifacts::interventions::read(&journal_path)
                .unwrap()
                .entries
                .as_slice(),
            std::slice::from_ref(&record)
        );
        let committed = worker
            .commit(&request(
                4,
                Command::CommitFault {
                    request_id: "crash".into(),
                },
            ))
            .unwrap();
        assert_eq!(committed.intervention, Some(record));
        assert_eq!(committed.error, None);
        assert_eq!(committed.rate, host::PlaybackRate::Paused);
        assert!(!committed.stepping);
        assert_eq!(runs.load(Ordering::SeqCst), calls);
        assert_eq!(worker.state.world.input_program.next, 1);
        assert_eq!(worker.state.world.intervention_next, 1);
        assert_eq!(
            playback(
                &mut worker,
                5,
                Command::Checkpoint {
                    request_id: "held".into()
                }
            )
            .error,
            Some(ControlError::Unsupported)
        );
        playback(
            &mut worker,
            6,
            Command::SetRate {
                rate: host::PlaybackRate::Unlimited,
            },
        );
        worker.step(Timestamp::from_nanos(10)).unwrap();
        let crashes = worker
            .driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(_, machine)| machine.data.crashes)
                    .sum::<u64>())
            })
            .unwrap();
        assert_eq!(crashes, 1);
        worker.step(Timestamp::from_nanos(20)).unwrap();
        assert!(
            worker.state.world.readiness.latched,
            "crash must not clear ever-ready latch"
        );
        assert_eq!(worker.state.world.publication.applied_faults.len(), 1);
        assert_eq!(
            worker
                .report(StopReason::SimulatedTimeLimit, None, None, None)
                .unwrap()
                .faults
                .len(),
            1
        );
    }
}
#[test]
fn failed_real_journal_never_commits_and_prepared_scope_blocks_execution() {
    let (root, mut worker, runs) = worker(1);
    worker.step(Timestamp::from_nanos(10)).unwrap();
    playback(
        &mut worker,
        1,
        Command::SetRate {
            rate: host::PlaybackRate::Paused,
        },
    );
    let before = fingerprint(&mut worker);
    let calls = runs.load(Ordering::SeqCst);
    let path = root.path().join("interventions.json");
    let mut journal = seedbox_artifacts::interventions::Journal::open(&path).unwrap();
    let prepare = request(
        2,
        Command::PrepareFault {
            request_id: "crash".into(),
            action: seedbox_control::Action::Crash {
                machine: "n1".into(),
            },
            timeout_ns: 100,
        },
    );
    let proposal = crash_proposal(&mut worker, "crash");
    let reply = worker.prepared(&prepare, proposal).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(journal.append(reply.intervention.unwrap()).is_err());
    assert!(worker
        .step(Timestamp::from_nanos(20))
        .unwrap_err()
        .to_string()
        .contains("cannot overtake prepared"));
    assert_eq!(fingerprint(&mut worker), before);
    assert_eq!(runs.load(Ordering::SeqCst), calls);
    assert_eq!(worker.state.world.input_program.next, 0);
}
#[test]
fn checkpoint_receipt_follows_durable_manifest_and_is_not_recaptured() {
    let (_root, mut worker, _) = worker(1);
    worker.step(Timestamp::from_nanos(10)).unwrap();
    playback(
        &mut worker,
        1,
        Command::SetRate {
            rate: host::PlaybackRate::Paused,
        },
    );
    let requested = playback(
        &mut worker,
        2,
        Command::Checkpoint {
            request_id: "saved".into(),
        },
    );
    assert_eq!(
        requested.checkpoint.unwrap().state,
        host::CheckpointState::Requested
    );
    let captured = worker.capture_pending().unwrap().unwrap();
    assert_eq!(captured.state, host::CheckpointState::Captured);
    let path = worker.output.join("checkpoint-saved.json");
    let envelope = checkpoint::read(&path).unwrap();
    assert_eq!(envelope.state, worker.state);
    let original = std::fs::read(&path).unwrap();
    assert!(worker.capture_pending().unwrap().is_none());
    assert_eq!(std::fs::read(path).unwrap(), original);
    let repeat = playback(
        &mut worker,
        3,
        Command::Checkpoint {
            request_id: "saved".into(),
        },
    );
    assert_eq!(repeat.checkpoint.unwrap(), captured);
}
#[test]
fn event_chain_ignores_absent_observation_socket_and_worker_count() {
    let mut results = vec![];
    for workers in [1, 4] {
        let (_root, mut worker, _) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        worker.step(Timestamp::from_nanos(20)).unwrap();
        let report = worker
            .report(StopReason::SimulatedTimeLimit, None, None, None)
            .unwrap();
        assert!(
            report.full_state.is_none(),
            "exit witness is never RAM equality"
        );
        assert!(
            report.witness.is_none(),
            "event chain alone is not the final execution witness"
        );
        results.push((
            worker.state.world.publication.witness,
            worker.state.world.publication.witness_records,
        ));
    }
    assert_eq!(results[0], results[1]);
}
