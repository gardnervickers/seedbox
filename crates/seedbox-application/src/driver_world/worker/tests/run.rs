use super::super::super::transport::Control;
use super::*;
use std::os::unix::net::UnixDatagram;

#[test]
fn loop_processes_real_control_prepare_commit_retry_and_resumes_exactly_once() {
    for workers in [1, 4] {
        let (root, mut worker, _) = worker(workers);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        assert_eq!(
            playback(
                &mut worker,
                0,
                Command::SetRate {
                    rate: host::PlaybackRate::Paused
                }
            )
            .error,
            None
        );
        let endpoint = root.path().join("worker.sock");
        let mut control = Control::bind(&endpoint).unwrap();
        let peer = root.path().join("peer.sock");
        let client = UnixDatagram::bind(peer).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let prepare = request(
            1,
            Command::PrepareFault {
                request_id: "live".into(),
                timeout_ns: 5_000_000_000,
                action: seedbox_control::Action::ClockDrift {
                    handle: "caller".into(),
                    machine: "n1".into(),
                    rate_ppb: None,
                    max_rate_ppb: Some(100_000_000),
                },
            },
        );
        let commit = request(
            2,
            Command::CommitFault {
                request_id: "live".into(),
            },
        );
        for request in [
            prepare,
            commit.clone(),
            commit,
            request(
                3,
                Command::SetRate {
                    rate: host::PlaybackRate::Unlimited,
                },
            ),
        ] {
            client
                .send_to(&serde_json::to_vec(&request).unwrap(), &endpoint)
                .unwrap();
        }
        assert_eq!(
            worker.execute(30, &mut control).unwrap(),
            StopReason::SimulatedTimeLimit
        );
        let mut replies = vec![];
        for _ in 0..4 {
            let mut bytes = [0; host::MAX_DATAGRAM];
            let size = client.recv(&mut bytes).unwrap();
            replies.push(serde_json::from_slice::<Reply>(&bytes[..size]).unwrap());
        }
        assert!(replies.iter().all(|reply| reply.error.is_none()));
        assert_eq!(
            replies[1], replies[2],
            "exact commit retry must reuse its receipt"
        );
        assert_eq!(replies[0].intervention.as_ref().unwrap().plan.id, 1);
        assert_eq!(worker.state.world.input_program.next, 1);
        assert_eq!(worker.state.world.input_program.admissions.len(), 1);
        assert_eq!(worker.state.world.input_program.admissions[0].plan.id, 1);
        assert_eq!(worker.state.world.position.simulated_time_ns, 30);
        assert_eq!(
            worker.state.world.publication.session.applied_faults.len(),
            1
        );
    }
}

struct Idle {
    first: bool,
    calls: Arc<AtomicUsize>,
}
impl Machine for Idle {
    type Context = ();
    type Error = String;
    type Snapshot = bool;
    fn run(
        &mut self,
        context: &mut seedbox_machine::machine::RunContext<'_, ()>,
        _: Timestamp,
    ) -> std::result::Result<MachineStop, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        while context.inbox.take().is_some() {}
        if self.first {
            context
                .outbox
                .emit(Output {
                    at: Timestamp::from_nanos(context.clock.now()),
                    payload: Response::Ready,
                })
                .unwrap();
            self.first = false;
        }
        Ok(MachineStop::Blocked {
            reached: Timestamp::from_nanos(context.clock.now()),
            deadline: None,
        })
    }
    fn snapshot(&mut self) -> std::result::Result<bool, String> {
        Ok(self.first)
    }
}
#[test]
fn loop_advances_idle_world_to_service_owned_future_input_without_queuing_a_copy() {
    for workers in [1, 4] {
        let root = tempfile::tempdir().unwrap();
        let output = root.path().join("output");
        std::fs::create_dir(&output).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let spy = calls.clone();
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
                Ok(Idle {
                    first: true,
                    calls: spy.clone(),
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
            inventory(root.path()),
            output,
            BTreeMap::new(),
            constructed,
            None,
            |saved, writer| {
                serde_json::to_writer(writer, &saved)?;
                Ok(())
            },
        )
        .unwrap();
        worker.step(Timestamp::from_nanos(10)).unwrap();
        assert_eq!(worker.state.world.position.simulated_time_ns, 0);
        assert!(worker.state.world.readiness.latched);
        worker.state.world.input_program.inputs =
            vec![seedbox_engine_protocol::input_program::TimedInput {
                key: "only-in-service-suffix".into(),
                at_ns: 20,
                timeout_ns: 5_000_000_000,
                action: seedbox_control::Action::ClockDrift {
                    handle: "idle-drift".into(),
                    machine: "n1".into(),
                    rate_ppb: Some(100),
                    max_rate_ppb: None,
                },
            }];
        let checkpoint = worker
            .driver
            .checkpoint_map(|_| Ok::<_, String>(driver::SnapshotReference::new([0; 32], 0)))
            .unwrap();
        let mut encoded = vec![];
        checkpoint.write_to(&mut encoded).unwrap();
        assert!(!encoded
            .windows(b"only-in-service-suffix".len())
            .any(|part| part == b"only-in-service-suffix"));
        let initial_calls = calls.load(Ordering::SeqCst);
        let mut control = Control::bind(&root.path().join("worker.sock")).unwrap();
        assert_eq!(
            worker.execute(100, &mut control).unwrap(),
            StopReason::Quiescent
        );
        assert_eq!(worker.state.world.position.simulated_time_ns, 20);
        assert_eq!(worker.state.world.input_program.next, 1);
        assert_eq!(worker.state.world.input_program.admissions[0].plan.id, 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            initial_calls,
            "idle clock advancement executed guest work"
        );
    }
}
