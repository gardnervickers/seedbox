//! Real worker capture/restore of an admitted hold, including its authored
//! cursor. Observation records verify effects but never reconstruct state.
use super::*;
use seedbox_driver::driver::{FaultRecord, PlaybackRate};
use seedbox_engine_protocol::{
    fault_status::FaultStatus,
    observation::{Kind, Observation},
};
use std::os::unix::net::UnixDatagram;

fn limits() -> driver::DecodeLimits {
    driver::DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 128 << 20,
        collection_items: 100_000,
        total_items: 500_000,
        string_bytes: 4096,
        blob_bytes: 16 << 20,
        depth: 128,
        archive_entries: 100_000,
        archive_path_bytes: 1 << 20,
        archive_expanded_bytes: 16 << 20,
    }
}
struct Observed {
    socket: UnixDatagram,
    records: Vec<Observation>,
}
impl Observed {
    fn attach(worker: &mut Worker<Mock>, path: &Path) -> Self {
        let socket = UnixDatagram::bind(path).unwrap();
        socket.set_nonblocking(true).unwrap();
        worker.publisher = Publisher::connect(Some(path)).unwrap();
        Self {
            socket,
            records: vec![],
        }
    }
    fn drain(&mut self) {
        // All sends happen synchronously on this test thread before drain.
        // No wall-clock sleep, polling deadline or competing receiver exists.
        loop {
            let mut buffer = vec![0; 65_536];
            match self.socket.recv(&mut buffer) {
                Ok(n) => {
                    let record: Observation = serde_json::from_slice(&buffer[..n]).unwrap();
                    assert_eq!(
                        record.sequence,
                        self.records.len() as u64 + 1,
                        "observation lost"
                    );
                    self.records.push(record);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => panic!("observation receive: {e}"),
            }
        }
    }
    fn effects(&self) -> Vec<u32> {
        self.records
            .iter()
            .filter_map(|record| match &record.kind {
                Kind::FaultChanged {
                    id,
                    status: FaultStatus::Applied,
                    ..
                } => Some(*id),
                _ => None,
            })
            .collect()
    }
}
fn states(worker: &mut Worker<Mock>) -> Vec<Data> {
    worker
        .driver
        .with_barrier(|b| Ok(b.machines().map(|(_, m)| m.data.clone()).collect()))
        .unwrap()
}
fn drive(worker: &mut Worker<Mock>, observed: &mut Observed, upto: u64) {
    for _ in 0..128 {
        let status = worker.playback().unwrap();
        let at = status.position.at.as_nanos();
        if status.before_fault.is_some() {
            if status.rate == PlaybackRate::Paused && !status.stepping {
                return;
            }
            worker.step(Timestamp::from_nanos(at)).unwrap();
        } else if status.rate == PlaybackRate::Paused && !status.stepping || at >= upto {
            return;
        } else if worker
            .state
            .world
            .input_program
            .inputs
            .get(worker.state.world.input_program.next)
            .is_some_and(|input| input.at_ns == at)
        {
            assert!(worker.authored_next().unwrap());
        } else {
            let next = worker
                .state
                .world
                .input_program
                .inputs
                .get(worker.state.world.input_program.next)
                .map_or(upto, |input| input.at_ns.min(upto));
            worker
                .step_toward_external(Timestamp::from_nanos(next))
                .unwrap();
        }
        observed.drain();
    }
    panic!("mock failed to reach its requested deterministic boundary");
}
fn restore(envelope: &wire::Checkpoint, workers: usize, output: &Path) -> Worker<Mock> {
    let decoded = checkpoint::open(envelope, limits())
        .unwrap()
        .try_map_snapshots(|file| {
            serde_json::from_reader(file.reader())
                .map(Saved)
                .map_err(crate::Error::from)
        })
        .unwrap();
    let names = decoded
        .machine_names()
        .map(|(id, name)| (id, name.unwrap().to_owned()))
        .collect();
    let mut driver = Driver::<Mock>::from_checkpoint(
        &decoded,
        workers,
        || Ok(()),
        |saved: &Saved| {
            Ok(Mock {
                data: saved.0.clone(),
                runs: Arc::new(AtomicUsize::new(0)),
            })
        },
        |_| Err("no future factory needed".into()),
    )
    .unwrap();
    assert!(
        driver.drain_observations().events.is_empty(),
        "no observed effect may be stored in the capture"
    );
    Worker::new(
        driver,
        envelope.state.clone(),
        envelope.inventory.clone(),
        output.to_owned(),
        names,
        Arc::default(),
        None,
        |saved, writer| {
            serde_json::to_writer(writer, &saved.0)?;
            Ok(())
        },
    )
    .unwrap()
}
fn crash() -> seedbox_engine_protocol::control::Action {
    seedbox_engine_protocol::control::Action::Crash {
        machine: "n1".into(),
    }
}
fn partition() -> seedbox_engine_protocol::control::Action {
    seedbox_engine_protocol::control::Action::Partition {
        handle: "partition".into(),
        left: vec!["oracle".into()],
        right: vec!["n1".into()],
    }
}

#[test]
fn held_crash_partition_and_same_time_order_survive_binary_capture_one_and_four_workers() {
    for source_workers in [1, 4] {
        for actions in [
            vec![crash()],
            vec![partition()],
            vec![crash(), partition()],
            vec![partition(), crash()],
        ] {
            let count = actions.len();
            let (root, mut source, _) = worker_with_network(source_workers, true);
            source.step(Timestamp::from_nanos(10)).unwrap();
            assert!(source.state.world.readiness.latched);
            source.state.world.input_program.inputs = actions
                .into_iter()
                .enumerate()
                .map(
                    |(index, action)| seedbox_engine_protocol::input_program::TimedInput {
                        key: format!("effect-{index}"),
                        at_ns: 10,
                        timeout_ns: 100,
                        action,
                    },
                )
                .collect();
            let mut source_observed =
                Observed::attach(&mut source, &root.path().join("source.sock"));
            assert_eq!(
                playback(
                    &mut source,
                    1,
                    Command::PauseBeforeNextFault {
                        rate: host::PlaybackRate::Unlimited
                    }
                )
                .error,
                None
            );
            assert!(source.status(2).unwrap().before_next_fault);
            assert!(source.authored_next().unwrap());
            source_observed.drain();
            let held = source.playback().unwrap();
            let first: FaultRecord = held.before_fault.clone().expect("real held fault");
            assert_eq!(held.rate, PlaybackRate::Paused);
            assert!(!held.stepping);
            assert_eq!(
                source.state.world.input_program.next, 1,
                "admitted input must not replay"
            );
            assert!(source_observed.effects().is_empty());
            assert_eq!(
                playback(
                    &mut source,
                    2,
                    Command::Checkpoint {
                        request_id: "held".into()
                    }
                )
                .error,
                None
            );
            let captured = source.capture_pending().unwrap().unwrap();
            source_observed.drain();
            assert_eq!(captured.state, host::CheckpointState::Captured);
            assert_eq!(source.playback().unwrap(), held);
            assert!(
                source_observed.effects().is_empty(),
                "capture must not emit an effect"
            );
            assert!(source
                .state
                .world
                .publication
                .session
                .applied_faults
                .is_empty());
            let envelope = checkpoint::read(&source.output.join("checkpoint-held.json")).unwrap();
            assert!(envelope
                .state
                .world
                .publication
                .session
                .applied_faults
                .is_empty());
            assert_eq!(envelope.state.world.input_program.next, 1);
            assert_eq!(
                playback(
                    &mut source,
                    3,
                    Command::SetRate {
                        rate: host::PlaybackRate::Unlimited
                    }
                )
                .error,
                None
            );
            drive(&mut source, &mut source_observed, 30);
            let expected_effects = source_observed.effects();
            assert_eq!(expected_effects.len(), count);
            assert_eq!(expected_effects[0], first.fault.get());
            let admitted: Vec<_> = source
                .state
                .world
                .input_program
                .admissions
                .iter()
                .map(|row| row.plan.id)
                .collect();
            assert_eq!(
                expected_effects, admitted,
                "simultaneous effects must keep admission order"
            );
            let expected = source.state.world.publication.clone();
            let expected_states = states(&mut source);
            for workers in [1, 4] {
                for step in [false, true] {
                    let mut branch = restore(&envelope, workers, root.path());
                    assert_eq!(branch.state.world.publication.session, Default::default());
                    let mut observed = Observed::attach(
                        &mut branch,
                        &root.path().join(format!("branch-{workers}-{step}.sock")),
                    );
                    assert_eq!(
                        branch.playback().unwrap(),
                        held,
                        "restore may not run at unlimited speed"
                    );
                    assert!(matches!(
                        branch.step(Timestamp::from_nanos(30)).unwrap(),
                        StepOutcome::BeforeFault { .. }
                    ));
                    observed.drain();
                    assert!(observed.effects().is_empty());
                    assert_eq!(branch.playback().unwrap().position, held.position);
                    let command = if step {
                        Command::Step
                    } else {
                        Command::SetRate {
                            rate: host::PlaybackRate::Unlimited,
                        }
                    };
                    assert_eq!(playback(&mut branch, 10, command).error, None);
                    drive(&mut branch, &mut observed, 30);
                    if step {
                        let paused = branch.playback().unwrap();
                        assert_eq!(paused.rate, PlaybackRate::Paused);
                        assert!(!paused.stepping && paused.before_fault.is_none());
                        assert!(paused.position.completed_quanta > held.position.completed_quanta);
                        assert_eq!(observed.effects(), expected_effects);
                        let before = paused.position;
                        branch.step(Timestamp::from_nanos(30)).unwrap();
                        observed.drain();
                        assert_eq!(branch.playback().unwrap().position, before);
                        assert_eq!(
                            playback(
                                &mut branch,
                                11,
                                Command::SetRate {
                                    rate: host::PlaybackRate::Unlimited
                                }
                            )
                            .error,
                            None
                        );
                        drive(&mut branch, &mut observed, 30);
                    }
                    assert_eq!(
                        observed.effects(),
                        expected_effects,
                        "effect identity/order/count changed"
                    );
                    assert_eq!(
                        branch.state.world.input_program,
                        source.state.world.input_program
                    );
                    assert_eq!(
                        branch.state.world.publication.oracle_exit,
                        expected.oracle_exit
                    );
                    assert_eq!(
                        branch.state.world.publication.session.applied_faults,
                        expected.session.applied_faults
                    );
                    assert_eq!(states(&mut branch), expected_states);
                }
            }
        }
    }
}

#[test]
fn checkpoint_refusals_identify_actual_state_without_unsupported() {
    let (_root, mut worker, _) = worker(1);
    let capture = |id| Command::Checkpoint {
        request_id: format!("capture-{id}"),
    };
    assert_eq!(
        playback(&mut worker, 1, capture(1)).error,
        Some(ControlError::NotPaused)
    );
    assert!(worker.capture_pending().unwrap().is_none());
    playback(
        &mut worker,
        2,
        Command::SetRate {
            rate: host::PlaybackRate::Paused,
        },
    );
    assert_eq!(
        playback(&mut worker, 3, capture(3)).error,
        Some(ControlError::NotReady)
    );
    assert!(worker.capture_pending().unwrap().is_none());
    playback(
        &mut worker,
        4,
        Command::SetRate {
            rate: host::PlaybackRate::Unlimited,
        },
    );
    worker.step(Timestamp::from_nanos(10)).unwrap();
    playback(
        &mut worker,
        5,
        Command::SetRate {
            rate: host::PlaybackRate::Paused,
        },
    );
    assert_eq!(
        playback(
            &mut worker,
            6,
            Command::Checkpoint {
                request_id: "../bad".into()
            }
        )
        .error,
        Some(ControlError::InvalidCheckpoint)
    );
    assert!(worker.capture_pending().unwrap().is_none());
}
