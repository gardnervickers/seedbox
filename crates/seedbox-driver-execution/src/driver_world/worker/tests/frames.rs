//! Host-only projection and policy/event-chain invariance; no native witness is
//! fabricated here. Native execution-witness comparisons belong to service tests.
use super::*;
use seedbox_driver::driver::{FrameFact, FrameFacts, FrameFate, NetworkSetup, Position};
use seedbox_engine_protocol::observation::{self, Kind, Observation};
use std::os::unix::net::UnixDatagram;

fn receive(socket: &UnixDatagram) -> Observation {
    let mut bytes = [0; observation::MAX_DATAGRAM_BYTES + 1];
    let n = socket.recv(&mut bytes).unwrap();
    assert!(n <= observation::MAX_DATAGRAM_BYTES);
    serde_json::from_slice(&bytes[..n]).unwrap()
}
#[test]
fn routed_frame_fates_and_overflow_map_to_observations_not_witness() {
    let (root, worker, _) = worker(1);
    let socket_path = root.path().join("frames.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    observation::configure_socket(&socket).unwrap();
    socket.set_nonblocking(true).unwrap();
    let mut publisher = Publisher::connect(Some(&socket_path)).unwrap();
    let mut state = worker.state.clone();
    let original = state.world.publication.clone();
    let ids: Vec<_> = worker.names.keys().copied().collect();
    let fates = [
        FrameFate::Scheduled,
        FrameFate::PartitionDropped,
        FrameFate::LossDropped,
        FrameFate::QueueDropped,
        FrameFate::ReceiverDropped,
    ];
    publisher
        .frames(
            &mut state,
            &worker.names,
            FrameFacts {
                facts: fates
                    .into_iter()
                    .map(|fate| FrameFact {
                        position: Position {
                            at: Timestamp::from_nanos(20),
                            boundary_index: 2,
                            completed_quanta: 2,
                        },
                        from: ids[0],
                        to: ids[1],
                        sent: Timestamp::from_nanos(10),
                        due: matches!(fate, FrameFate::Scheduled | FrameFate::ReceiverDropped)
                            .then_some(Timestamp::from_nanos(30)),
                        bytes: 64,
                        fate,
                    })
                    .collect(),
                omitted: 2,
            },
        )
        .unwrap();
    let observation = receive(&socket);
    assert_eq!(observation.sequence, 1);
    assert_eq!(observation.simulated_time_ns, 20);
    assert_eq!(observation.machine, "");
    let Kind::Frames { frames } = observation.kind else {
        panic!("expected frames")
    };
    assert_eq!(frames.len(), 5);
    for frame in &frames {
        assert_eq!(frame.from, worker.names[&ids[0]]);
        assert_eq!(frame.to, worker.names[&ids[1]]);
        assert_eq!(frame.sent_ns, 10);
        assert_eq!(frame.bytes, 64);
    }
    assert_eq!(
        frames.iter().map(|f| f.fate).collect::<Vec<_>>(),
        [
            observation::FrameFate::Scheduled,
            observation::FrameFate::PartitionDropped,
            observation::FrameFate::LossDropped,
            observation::FrameFate::QueueDropped,
            observation::FrameFate::ReceiverDropped,
        ]
    );
    assert_eq!(
        frames.iter().map(|f| f.due_ns).collect::<Vec<_>>(),
        [Some(30), None, None, None, Some(30)]
    );
    // The existing service receiver reports SourceGap {first:2,last:3}.
    // A watermark makes overflow visible even with no further machine output.
    let watermark = receive(&socket);
    assert_eq!(watermark.sequence, 4);
    assert_eq!(watermark.kind, Kind::Frames { frames: vec![] });
    publisher.close(&mut state).unwrap();
    let closed = receive(&socket);
    assert_eq!(closed.sequence, 5);
    assert_eq!(closed.kind, Kind::Closed);
    assert!(publisher.published.is_empty());
    assert_eq!(
        state.world.publication.session.next_driver_sequence,
        original.session.next_driver_sequence
    );
}

struct FrameMock(Mock);
impl Machine for FrameMock {
    type Context = ();
    type Error = String;
    type Snapshot = Saved;
    fn run(
        &mut self,
        c: &mut seedbox_machine::machine::RunContext<'_, ()>,
        upto: Timestamp,
    ) -> std::result::Result<MachineStop, String> {
        let send = self.0.data.first && self.0.data.name == "sender";
        let stopped = self.0.run(c, upto)?;
        if send {
            let mut bytes = vec![0; 64];
            bytes[..6].fill(255);
            bytes[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
            bytes[12..14].copy_from_slice(&[0x88, 0xb5]);
            for _ in 0..3 {
                c.outbox
                    .emit(Output {
                        at: upto,
                        payload: Response::TransmitFrame {
                            bytes: bytes.clone(),
                        },
                    })
                    .unwrap();
            }
        }
        Ok(stopped)
    }
    fn snapshot(&mut self) -> std::result::Result<Saved, String> {
        self.0.snapshot()
    }
}
fn frame_worker(
    workers: usize,
    observed: bool,
) -> (tempfile::TempDir, Worker<FrameMock>, UnixDatagram) {
    let (root, old, runs) = worker(1);
    let socket_path = root.path().join("network.sock");
    let socket = UnixDatagram::bind(&socket_path).unwrap();
    observation::configure_socket(&socket).unwrap();
    socket.set_nonblocking(true).unwrap();
    let definitions = ["sender", "receiver"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let mut d = definition(name);
            d.network = Some(
                seedbox_machine::definition::NetworkInterface::new(
                    seedbox_machine::definition::MacAddress::new([
                        2,
                        0,
                        0,
                        0,
                        0,
                        (index + 1) as u8,
                    ])
                    .unwrap(),
                    [10, 0, 0, (index + 1) as u8].into(),
                    24,
                )
                .unwrap(),
            );
            d
        })
        .collect();
    let constructed: Constructed = Arc::default();
    let ledger = constructed.clone();
    let driver = Driver::new_seeded_with_network(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        42,
        || Ok(()),
        definition("oracle"),
        definitions,
        vec![],
        NetworkSetup::from(
            NetworkConfig::new(Duration::from_nanos(10), Duration::ZERO, 0).unwrap(),
        ),
        move |definition| {
            ledger.lock().unwrap().push_back(definition.name.clone());
            Ok(FrameMock(Mock {
                data: Data {
                    name: definition.name.clone(),
                    at: 0,
                    first: true,
                    dead: false,
                    crashes: 0,
                    random: [0; 8],
                },
                runs: runs.clone(),
            }))
        },
    )
    .unwrap();
    let mut state = old.state.clone();
    state.world.machine_names.clear();
    state.world.publication = Default::default();
    state.world.readiness.initial = vec!["oracle".into(), "receiver".into(), "sender".into()];
    let mut w = Worker::new(
        driver,
        state,
        old.inventory.clone(),
        old.output.clone(),
        BTreeMap::new(),
        constructed,
        observed.then_some(socket_path.as_path()),
        |saved, out| {
            serde_json::to_writer(out, &saved.0)?;
            Ok(())
        },
    )
    .unwrap();
    w.flush().unwrap();
    (root, w, socket)
}
#[test]
fn routed_frame_draining_keeps_policy_and_event_witness_equal_off_on_and_workers_1_4() {
    let mut evidence = vec![];
    for workers in [1, 4] {
        for observed in [false, true] {
            let (_root, mut worker, socket) = frame_worker(workers, observed);
            for at in [10, 20, 30] {
                worker.step(Timestamp::from_nanos(at)).unwrap();
            }
            let mut frames = vec![];
            while let Ok(observation) = {
                let mut bytes = [0; observation::MAX_DATAGRAM_BYTES + 1];
                socket
                    .recv(&mut bytes)
                    .map(|n| serde_json::from_slice::<Observation>(&bytes[..n]).unwrap())
            } {
                if let Kind::Frames { frames: batch } = observation.kind {
                    frames.extend(batch);
                }
            }
            assert_eq!(frames.len(), 3 * usize::from(observed));
            if observed {
                assert_eq!(frames[0].from, "sender");
                assert_eq!(frames[0].to, "receiver");
                assert_eq!(frames[0].fate, observation::FrameFate::Scheduled);
                assert_eq!(frames[0].due_ns, Some(frames[0].sent_ns + 10));
                assert_eq!(frames[0].bytes, 64);
            }
            let mut policy = vec![];
            worker
                .driver
                .with_barrier(|b| {
                    b.write_policy_evidence(&mut policy).unwrap();
                    Ok(())
                })
                .unwrap();
            let publication = &worker.state.world.publication;
            evidence.push((
                policy,
                worker.publisher.published.clone(),
                publication.session.next_driver_sequence,
            ));
            assert_eq!(
                worker.driver.drain_observations().frames,
                FrameFacts::default()
            );
        }
    }
    for result in &evidence[1..] {
        assert_eq!(result, &evidence[0]);
    }
}

#[test]
fn actual_bounded_routing_overflow_reports_a_source_gap_and_keeps_authority() {
    let mut evidence = vec![];
    for capacity in [1, 1024] {
        let (_root, mut worker, socket) = frame_worker(1, true);
        worker.driver.set_frame_capacity(capacity);
        worker.step(Timestamp::from_nanos(10)).unwrap();
        let mut observations = vec![];
        while let Ok(observation) = {
            let mut bytes = [0; observation::MAX_DATAGRAM_BYTES + 1];
            socket
                .recv(&mut bytes)
                .map(|n| serde_json::from_slice::<Observation>(&bytes[..n]).unwrap())
        } {
            observations.push(observation);
        }
        let kept: usize = observations
            .iter()
            .map(|o| match &o.kind {
                Kind::Frames { frames } => frames.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(kept, if capacity == 1 { 1 } else { 3 });
        let missing: u64 = observations
            .windows(2)
            .map(|pair| pair[1].sequence - pair[0].sequence - 1)
            .sum();
        assert_eq!(missing, if capacity == 1 { 2 } else { 0 });
        let mut policy = vec![];
        worker
            .driver
            .with_barrier(|b| {
                b.write_policy_evidence(&mut policy).unwrap();
                Ok(())
            })
            .unwrap();
        evidence.push((policy, worker.publisher.published.clone()));
    }
    assert_eq!(evidence[0], evidence[1]);
}
