//! Driver-owned binary persistence with a deliberately separate mock artifact codec.
mod authority_tests;
mod frame_capacity_tests;
mod streaming_tests;
use super::*;
use crate::driver::fixture::owned::network_fixture::{initial, net, policy, setup};
use crate::persistence::{Decoder, Encoder, Wire};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};
fn continue_from(d: &mut Driver<Mock>) -> (Vec<Event>, Position, Vec<State>, PlaybackStatus) {
    reference_observer_session(d);
    let observed = status(d);
    if d.execution.finished.is_none()
        && !observed.stopped
        && observed.rate == PlaybackRate::Paused
        && !observed.stepping
    {
        d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
    }
    let end = if observed.stepping { 10 } else { 40 };
    let events = if d.execution.finished.is_some() || observed.stopped {
        d.step(at(100)).unwrap();
        d.drain_events()
    } else {
        advance(d, end)
    };
    (events, d.position(), states(d), status(d))
}
fn exercise(mut d: Driver<Mock>) {
    let before = status(&mut d);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    let expected = continue_from(&mut d);
    drop(d);
    for workers in [1, 4] {
        let c = decode(&bytes, &store);
        let mut b = restore(&c, workers);
        drop(c);
        assert_eq!(status(&mut b), before);
        assert_eq!(continue_from(&mut b), expected);
    }
}
#[test]
fn all_stopped_boundary_families_roundtrip_after_source_destruction() {
    let mut d = Driver::new_seeded(
        at(0),
        Duration::from_nanos(u64::MAX - 1),
        1,
        51,
        || Ok(Context(None)),
        fixture::definition(0),
        vec![],
        vec![],
        NetworkConfig::default(),
        fresh,
    )
    .unwrap();
    d.schedule(
        at(0),
        key("fast"),
        Action::Drift {
            target: Target::Machine(MachineId(0)),
            rate: ClockRate::new(100_000_000).unwrap(),
        },
    )
    .unwrap();
    d.schedule(
        at(0),
        key("pause"),
        Action::Pause(Target::Machine(MachineId(0))),
    )
    .unwrap();
    d.schedule(
        at(u64::MAX - 1),
        key("future"),
        Action::Resume(Target::Machine(MachineId(0))),
    )
    .unwrap();
    d.step(at(0)).unwrap();
    d.step(at(u64::MAX - 2)).unwrap();
    exercise(d);
    let mut d = build(
        (0..3).map(|i| spec(i, true, vec![])).collect(),
        (3..5)
            .map(|i| spec(i, true, vec![(0, Response::Ready)]))
            .collect(),
        true,
    );
    d.schedule(
        at(25),
        key("spawn3"),
        Action::Spawn {
            name: "fixture-3".into(),
        },
    )
    .unwrap();
    d.schedule(
        at(26),
        key("spawn4"),
        Action::Spawn {
            name: "fixture-4".into(),
        },
    )
    .unwrap();
    advance(&mut d, 13);
    exercise(d);
    let mut d = build(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (25, call(1, r#""op":"spawn","machine":"fixture-2""#)),
                ],
            ),
            spec(1, true, vec![]),
        ],
        vec![spec(2, true, vec![(0, Response::Ready)])],
        true,
    );
    advance(&mut d, 13);
    exercise(d);
    let mut d = build(
        vec![
            spec(0, true, vec![]),
            spec(1, false, vec![(14, output(b"timer"))]),
            spec(2, true, vec![]),
        ],
        vec![],
        true,
    );
    d.schedule(
        at(0),
        key("drift"),
        Action::Drift {
            target: Target::Machine(MachineId(1)),
            rate: ClockRate::new(99_999_999).unwrap(),
        },
    )
    .unwrap();
    d.schedule(
        at(0),
        key("pause"),
        Action::Pause(Target::Machine(MachineId(2))),
    )
    .unwrap();
    d.schedule(
        at(7),
        key("resume"),
        Action::Resume(Target::Machine(MachineId(2))),
    )
    .unwrap();
    d.schedule(
        at(15),
        key("heal"),
        Action::Heal(HealTarget::Input(key("drift"))),
    )
    .unwrap();
    advance(&mut d, 3);
    exercise(d);
    let mut d = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    d.schedule(
        at(5),
        key("throttle"),
        Action::Throttle {
            target: Target::Machine(MachineId(1)),
            config: ExecutionConfig::new(2000).unwrap(),
        },
    )
    .unwrap();
    d.with_barrier(|b| b.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    while !matches!(d.step(at(10)).unwrap(), StepOutcome::BeforeFault { .. }) {}
    exercise(d);
    let mut d = build(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (2, call(1, r#""op":"sleep","duration_ns":13"#)),
                    (
                        3,
                        call(
                            2,
                            r#""op":"arm_trigger","source":{"machine":"fixture-1"},"marker":[120,121],"match_timeout_ns":20,"fault":{"action":"stop","target":{"machine":"fixture-1"}}"#,
                        ),
                    ),
                ],
            ),
            spec(
                1,
                false,
                vec![(0, Response::Ready), (5, output(b"x")), (8, output(b"y"))],
            ),
        ],
        vec![],
        true,
    );
    d.schedule(
        at(4),
        key("pause-process"),
        Action::PauseProcess {
            target: Target::Machine(MachineId(1)),
            deadline: Duration::from_nanos(20),
        },
    )
    .unwrap();
    advance(&mut d, 5);
    exercise(d);
    let mut d = build_network(
        vec![
            spec(0, true, vec![(1, frame([255; 6], 71))]),
            spec(1, true, vec![]),
            spec(2, false, vec![]),
        ],
        vec![],
        true,
        true,
    );
    d.schedule(
        at(3),
        key("crash"),
        Action::Crash(Target::Machine(MachineId(1))),
    )
    .unwrap();
    d.schedule(
        at(4),
        key("reboot"),
        Action::Reboot(Target::Machine(MachineId(1))),
    )
    .unwrap();
    advance(&mut d, 2);
    exercise(d);
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.step(at(3)).unwrap();
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Paused)?;
        b.step_quantum()
    })
    .unwrap();
    exercise(d);
    for drain in [false, true] {
        let mut d = build(vec![spec(0, true, vec![])], vec![], true);
        if drain {
            d.drain_events();
        }
        d.stage(
            at(0),
            EventKind::MachineAdded {
                machine: MachineId(0),
                cause: Cause::Host,
            },
        );
        exercise(d);
    }
    let mut d = build(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (
                        2,
                        call(
                            1,
                            r#""op":"clock_skew","target":{"machine":"fixture-1"},"max_offset_ns":100"#,
                        ),
                    ),
                    (
                        12,
                        call(
                            2,
                            r#""op":"clock_skew","target":{"machine":"fixture-1"},"max_offset_ns":100"#,
                        ),
                    ),
                ],
            ),
            spec(1, true, vec![(0, Response::Ready)]),
        ],
        vec![],
        true,
    );
    d.schedule(
        at(8),
        key("pause"),
        Action::Pause(Target::Machine(MachineId(1))),
    )
    .unwrap();
    d.schedule(
        at(9),
        key("reject-pause"),
        Action::Pause(Target::Machine(MachineId(1))),
    )
    .unwrap();
    d.schedule(
        at(10),
        key("resume"),
        Action::Resume(Target::Machine(MachineId(1))),
    )
    .unwrap();
    advance(&mut d, 6);
    exercise(d);
    let mut d = build_network(
        vec![
            spec(0, true, vec![(20, frame(mac(1), 5))]),
            spec(
                1,
                true,
                vec![(1, frame([255; 6], 1)), (25, frame(mac(0), 2))],
            ),
        ],
        vec![],
        true,
        true,
    );
    for (t, k, a) in [
        (
            3,
            "cut-a",
            Action::Partition {
                left: vec![Target::Machine(MachineId(0))],
                right: vec![Target::Machine(MachineId(1))],
            },
        ),
        (
            3,
            "cut-b",
            Action::Partition {
                left: vec![Target::Machine(MachineId(0))],
                right: vec![Target::Machine(MachineId(1))],
            },
        ),
        (12, "heal-a", Action::Heal(HealTarget::Input(key("cut-a")))),
        (22, "heal-b", Action::Heal(HealTarget::Input(key("cut-b")))),
    ] {
        d.schedule(at(t), key(k), a).unwrap();
    }
    advance(&mut d, 3);
    exercise(d);
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Paused))
        .unwrap();
    exercise(d);
    let mut d = build(
        vec![spec(
            0,
            true,
            vec![(3, Response::ApplicationOutcome { code: 17 })],
        )],
        vec![],
        true,
    );
    advance(&mut d, 5);
    exercise(d);
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.with_barrier(|b| {
        b.stop();
        Ok(())
    })
    .unwrap();
    exercise(d);
    let mut d = build(
        vec![spec(0, true, vec![])],
        vec![spec(1, true, vec![(0, Response::Ready)])],
        true,
    );
    d.schedule(
        at(0),
        key("spawn"),
        Action::Spawn {
            name: "fixture-1".into(),
        },
    )
    .unwrap();
    d.with_barrier(|b| b.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    d.step(at(0)).unwrap();
    exercise(d);
}
#[test]
fn future_binary_closure_is_exact_and_invalid_resources_remain_deferred() {
    use crate::definition::{DiskImage, FilesystemArchive};
    for image in [false, true] {
        let mut d = build(
            vec![spec(0, true, vec![])],
            vec![spec(1, true, vec![])],
            true,
        );
        let mut archive = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(4);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_cksum();
        archive
            .append_data(&mut header, "file", &b"data"[..])
            .unwrap();
        let original = archive.into_inner().unwrap();
        d.execution.definitions[0].application.filesystem =
            FilesystemArchive::new(original.clone()).unwrap();
        d.execution.definitions[0].resources.memory_bytes = 0;
        d.execution.definitions[0].application.launch.argv.clear();
        d.execution.definitions[0].disk = Some(if image {
            DiskImage::from_bytes(vec![7; 512]).unwrap()
        } else {
            DiskImage::zeroed(1u64 << 40).unwrap()
        });
        let source_file = std::env::temp_dir().join(format!(
            "seedbox-codec-source-{}-{image}",
            std::process::id()
        ));
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&source_file)
            .unwrap();
        file.write_all(&original).unwrap();
        drop(file);
        d.execution.definitions[0].application.filesystem =
            FilesystemArchive::new(std::fs::read(&source_file).unwrap()).unwrap();
        let (bytes, store) = artifact(d.checkpoint().unwrap());
        drop(d);
        std::fs::remove_file(source_file).unwrap();
        let c = decode(&bytes, &store);
        assert_eq!(c.definitions[0].application.filesystem.as_bytes(), original);
        assert_eq!(c.definitions[0].resources.memory_bytes, 0);
        assert!(c.definitions[0].application.launch.argv.is_empty());
        assert_eq!(
            c.definitions[0].disk.as_ref().unwrap().image().is_some(),
            image
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let mut b = Driver::from_checkpoint(
            &c,
            1,
            || Ok(Context(None)),
            Mock::restore,
            move |definition| {
                assert_eq!(definition.resources.memory_bytes, 0);
                assert!(definition.application.launch.argv.is_empty());
                count.fetch_add(1, Ordering::SeqCst);
                Err(io("deferred invalid future"))
            },
        )
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        b.schedule(
            at(1),
            key("future"),
            Action::Spawn {
                name: "fixture-1".into(),
            },
        )
        .unwrap();
        assert!(b.step(at(10)).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
#[test]
fn framing_truncation_unknown_tags_budgets_and_partial_io_fail_closed() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.step(at(1)).unwrap();
    // A real pending guest input exercises the blob quota; optional publication
    // log bytes are deliberately no longer part of this codec.
    d.execution.machines[0].inbox.push_back(crate::pv::Input {
        at: at(1),
        payload: crate::pv::Request::ReceiveFrame { bytes: vec![0; 14] },
    });
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    for n in 0..bytes.len() {
        assert!(
            Checkpoint::read_from(&mut &bytes[..n], limits()).is_err(),
            "accepted truncation {n}"
        );
    }
    let mut bad_tag = bytes.clone();
    bad_tag[28..32].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(Checkpoint::read_from(&mut &bad_tag[..], limits()).is_err());
    for index in [0, 8] {
        let mut bad = bytes.clone();
        bad[index] ^= 255;
        assert!(Checkpoint::read_from(&mut &bad[..], limits()).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(Checkpoint::read_from(&mut &extra[..], limits()).is_err());
    for field in 0..8 {
        let mut l = limits();
        match field {
            0 => l.encoded_bytes = 20,
            1 => l.allocation_bytes = 0,
            2 => l.collection_items = 0,
            3 => l.total_items = 0,
            4 => l.string_bytes = 0,
            5 => l.blob_bytes = 0,
            6 => l.depth = 0,
            _ => l.encoded_bytes = 0,
        };
        assert!(
            Checkpoint::read_from(&mut &bytes[..], l).is_err(),
            "budget {field}"
        );
    }
    struct Fail(usize);
    impl Write for Fail {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io("writer failure"));
            }
            let n = b.len().min(self.0);
            self.0 -= n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let refs = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
    for n in [0, 5, 20, bytes.len() - 1] {
        assert!(refs.write_to(&mut Fail(n)).is_err());
    }
    let mut again = Vec::new();
    refs.write_to(&mut again).unwrap();
    assert_eq!(again, bytes);
    struct Broken<'a>(&'a [u8], usize);
    impl Read for Broken<'_> {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            if self.1 == 0 {
                return Err(io("reader failure"));
            }
            let n = self.0.len().min(b.len()).min(self.1);
            b[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            self.1 -= n;
            Ok(n)
        }
    }
    assert!(Checkpoint::read_from(&mut Broken(&bytes, 30), limits()).is_err());
}
#[test]
fn decoded_invariants_reject_untrusted_complete_objects() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.step(at(1)).unwrap();
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    for kind in 0..8 {
        let mut c = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
        match kind {
            0 => c.quantum_ns = 0,
            1 => c.oracle = MachineId(100),
            2 => c.network = network::Network::new(NetworkConfig::default()),
            3 => c.completed_quanta = u64::MAX,
            4 => c.machines[0].machine = None,
            5 => c.machines[0].lifecycle.uncertain_failure(),
            6 => {
                c.events.insert(
                    at(0),
                    VecDeque::from([Scheduled::Driver(DriverEvent::Pause(MachineId(0)))]),
                );
            }
            _ => {
                c.inputs.insert(key("missing-input"), LiveInput::Pending);
            }
        }
        // Deliberately bypass the validating public writer to model imported bytes.
        let mut e = Encoder { output: Vec::new() };
        c.put(&mut e).unwrap();
        let mut bad = b"SBXDRVR\0".to_vec();
        // Use current version 1 framing so the intended state invariant, not version,
        // is what rejects these deliberately malformed imported bodies.
        bad.extend(1u32.to_le_bytes());
        bad.extend((e.output.len() as u64).to_le_bytes());
        bad.extend(e.output);
        assert!(
            Checkpoint::read_from(&mut &bad[..], limits()).is_err(),
            "invariant {kind}"
        );
    }
}
#[test]
fn byte_and_collection_lengths_are_checked_before_body_read_and_recursion() {
    struct HeaderOnly {
        bytes: std::io::Cursor<[u8; 8]>,
    }
    impl Read for HeaderOnly {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            assert!(self.bytes.position() < 8, "body read after rejected length");
            self.bytes.read(b)
        }
    }
    let mut low = limits();
    low.blob_bytes = 10;
    let mut d = Decoder::new(
        HeaderOnly {
            bytes: std::io::Cursor::new(1_000_000u64.to_le_bytes()),
        },
        2_000_000,
        low,
    );
    assert!(Vec::<u8>::get(&mut d).is_err());
    let mut low = limits();
    low.collection_items = 10;
    let mut d = Decoder::new(
        HeaderOnly {
            bytes: std::io::Cursor::new(100u64.to_le_bytes()),
        },
        2_000_000,
        low,
    );
    assert!(Vec::<Position>::get(&mut d).is_err());
    let mut encoded = Vec::new();
    for _ in 0..10 {
        encoded.push(6u8);
        encoded.extend(1u64.to_le_bytes());
    }
    encoded.push(0);
    let mut low = limits();
    low.depth = 4;
    let mut d = Decoder::new(&encoded[..], encoded.len() as u64, low);
    assert!(<serde_json::Value as Wire>::get(&mut d).is_err());
    let mut d = Decoder::new(&[2u8][..], 1, limits());
    assert!(bool::get(&mut d).is_err());
    let mut e = Encoder { output: Vec::new() };
    7u8.put(&mut e).unwrap();
    2u64.put(&mut e).unwrap();
    for _ in 0..2 {
        "same".to_owned().put(&mut e).unwrap();
        0u8.put(&mut e).unwrap();
    }
    let mut d = Decoder::new(&e.output[..], e.output.len() as u64, limits());
    assert!(<serde_json::Value as Wire>::get(&mut d).is_err());
    for value in [
        serde_json::json!(u64::MAX),
        serde_json::json!(i64::MIN),
        serde_json::json!(-0.0),
        serde_json::json!(f64::MIN_POSITIVE),
        serde_json::json!(0.1),
    ] {
        let mut e = Encoder { output: Vec::new() };
        value.put(&mut e).unwrap();
        let mut d = Decoder::new(&e.output[..], e.output.len() as u64, limits());
        assert_eq!(
            <serde_json::Value as Wire>::get(&mut d)
                .unwrap()
                .to_string(),
            value.to_string()
        );
    }
}
#[test]
fn duplicate_maps_invalid_rng_and_consuming_map_cleanup() {
    let mut d = Decoder::new(&[2u8, 0, 0, 0, 0, 0, 0, 0, 1, 10, 1, 20][..], 12, limits());
    assert!(<BTreeMap<u8, u8> as Wire>::get(&mut d).is_err());
    let mut d = build(
        (0..3).map(|i| spec(i, true, vec![])).collect(),
        vec![],
        true,
    );
    let drops = Arc::new(AtomicUsize::new(0));
    for e in &mut d.execution.machines {
        e.machine.as_mut().unwrap().saved_drops = Some(drops.clone());
    }
    let c = d.checkpoint().unwrap();
    let mut mapped = 0;
    let result = c.try_map_snapshots(|saved| {
        mapped += 1;
        if mapped == 2 {
            Err(io("mapping failure"))
        } else {
            Ok(saved)
        }
    });
    assert!(result.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(d.step(at(1)).is_ok());
    let (mut bytes, _) = artifact(d.checkpoint().unwrap()); // RNG cursor resides after now/domain/seed/block/cache.
    bytes[20 + 8 + 4 + 8 + 8 + 32] = 33;
    assert!(Checkpoint::read_from(&mut &bytes[..], limits()).is_err());
}

#[test]
fn held_effect_survives_running_rate_selection_and_rearm() {
    let limited = PlaybackRate::Limited {
        numerator: nz(3),
        denominator: nz(2),
    };
    for (rate, rearm) in [
        (PlaybackRate::Unlimited, false),
        (limited, false),
        (PlaybackRate::Unlimited, true),
    ] {
        let mut d = build(
            vec![spec(0, true, vec![]), spec(1, true, vec![])],
            vec![],
            true,
        );
        d.schedule(
            at(5),
            key("held-throttle"),
            Action::Throttle {
                target: Target::Machine(MachineId(1)),
                config: ExecutionConfig::new(2000).unwrap(),
            },
        )
        .unwrap();
        d.with_barrier(|b| b.pause_before_next_fault(PlaybackRate::Unlimited))
            .unwrap();
        while !matches!(d.step(at(10)).unwrap(), StepOutcome::BeforeFault { .. }) {}
        d.with_barrier(|b| {
            if rearm {
                b.pause_before_next_fault(rate)
            } else {
                b.set_rate(rate)
            }
        })
        .unwrap();
        let before = status(&mut d);
        assert_eq!(before.rate, rate);
        assert_eq!(before.before_next_fault, rearm);
        let fault = before.before_fault.as_ref().unwrap().fault;
        assert_eq!(
            d.execution.machines[1]
                .lifecycle
                .execution()
                .factor_permille(),
            1000
        );
        let (bytes, store) = artifact(d.checkpoint().unwrap());
        let finish = |driver: &mut Driver<Mock>| {
            reference_observer_session(driver);
            // Assert the round-tripped limited rate before normalizing pacing;
            // continuation assertions depend only on virtual execution, not time.
            assert_eq!(status(driver), before);
            if rate == limited {
                driver
                    .with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
                    .unwrap();
            }
            let mut events = advance(driver, 20);
            events.extend(advance(driver, 30));
            assert_eq!(
                driver.execution.machines[1]
                    .lifecycle
                    .execution()
                    .factor_permille(),
                2000
            );
            assert!(status(driver).before_fault.is_none());
            assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::Transition { machine, change: Transition::ExecutionSet(config), .. } if *machine == MachineId(1) && config.factor_permille() == 2000)).count(), 1);
            assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.fault == fault)).count(), 1);
            (events, driver.position(), states(driver), status(driver))
        };
        let expected = finish(&mut d);
        drop(d);
        for workers in [1, 4] {
            let c = decode(&bytes, &store);
            let mut restored = restore(&c, workers);
            drop(c);
            assert_eq!(finish(&mut restored), expected);
        }
    }
}

#[test]
fn retired_cursor_and_reports_are_validated_before_publication_or_restore() {
    let mut d = net(initial(), setup(8), 1);
    advance(&mut d, 1);
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    for kind in 0..5 {
        let mut c = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
        let mut output = MachineResponse {
            machine: MachineId(1),
            boot: 0,
            at: at(0),
            payload: Response::BootEnded {
                ended_boot: 0,
                disposition: seedbox_machine::machine::EvidenceDisposition::Crashed,
                native_booted: false,
                disk_lost_sectors: 0,
                surviving_disk: None,
                retired_state_digest: None,
            },
        };
        let expected = match kind {
            0 => {
                c.machines[1].last_ended_published = Some(1);
                "retired boot exceeds committed lifecycle ordinal"
            }
            1 => {
                c.machines[1].staged.push(output);
                "retired output lacks cursor"
            }
            2 => {
                c.machines[1].last_ended_published = Some(0);
                c.machines[1].staged.extend([output.clone(), output]);
                "duplicate retained boot-end output"
            }
            3 => {
                c.machines[1].last_ended_published = Some(0);
                if let Response::BootEnded {
                    retired_state_digest,
                    ..
                } = &mut output.payload
                {
                    *retired_state_digest = Some("a".repeat(64));
                }
                c.machines[1].staged.push(output);
                "never-booted retirement cannot claim RAM or natural termination"
            }
            _ => {
                c.machines[1].last_ended_published = Some(0);
                if let Response::BootEnded { ended_boot, .. } = &mut output.payload {
                    *ended_boot = 1;
                }
                c.machines[1].staged.push(output);
                "retired output ordinal"
            }
        };
        let mut destination = Vec::new();
        assert!(
            matches!(c.write_to(&mut destination), Err(CodecError::Invalid(reason)) if reason == expected)
        );
        assert!(
            destination.is_empty(),
            "cheap refusal precedes any writer output"
        );
        let mut e = Encoder { output: Vec::new() };
        c.put(&mut e).unwrap(); // deliberately bypass validating publication
        let mut bad = bytes[..12].to_vec();
        bad.extend((e.output.len() as u64).to_le_bytes());
        bad.extend(e.output);
        assert!(
            matches!(Checkpoint::read_from(&mut &bad[..], limits()), Err(CodecError::Invalid(reason)) if reason == expected)
        );
    }
}

#[test]
fn closed_codec_version_and_orphaned_network_ownership_fail_before_restore() {
    let mut d = net(initial(), setup(8), 1);
    advance(&mut d, 1);
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    assert_eq!(&bytes[8..12], &1u32.to_le_bytes());
    for version in [0u32, 2, u32::MAX] {
        let mut old = bytes.clone();
        old[8..12].copy_from_slice(&version.to_le_bytes());
        assert!(matches!(
            Checkpoint::read_from(&mut &old[..], limits()),
            Err(CodecError::Invalid(message)) if message == "version"
        ));
    }
    for shape_layer in [false, true] {
        let mut c = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
        if shape_layer {
            c.network.shape(
                1,
                "orphan".into(),
                &[MachineId(1)],
                &[MachineId(2)],
                policy(10),
                NetworkFaults::default(),
            );
        } else {
            c.network
                .partition("orphan".into(), &[MachineId(1)], &[MachineId(2)]);
        }
        let mut e = Encoder { output: Vec::new() };
        c.put(&mut e).unwrap();
        let mut bad = bytes[..12].to_vec();
        bad.extend((e.output.len() as u64).to_le_bytes());
        bad.extend(e.output);
        assert!(matches!(
            Checkpoint::read_from(&mut &bad[..], limits()),
            Err(CodecError::Invalid(_))
        ));
    }
}

#[test]
fn oom_victim_output_survives_persisted_continuation_and_worker_counts() {
    let kill = Response::OutOfMemoryKill {
        kill: seedbox_engine_protocol::out_of_memory::Kill {
            pid: 31,
            command: "allocator".into(),
            anon_rss_kib: 300948,
            application: true,
        },
    };
    let mut d = build(
        vec![
            spec(0, true, vec![]),
            spec(
                1,
                true,
                vec![
                    (0, Response::Ready),
                    (5, kill),
                    (6, Response::ApplicationOutcome { code: 137 }),
                ],
            ),
        ],
        vec![],
        true,
    );
    advance(&mut d, 3);
    // exercise destroys the source and compares publications and machine state
    // after binary decode/restoration with both one and four workers.
    exercise(d);
}

#[test]
fn an_imported_property_ledger_is_checked_before_restore() {
    let mut d = build(
        vec![spec(0, true, vec![(0, Response::Ready)])],
        vec![],
        true,
    );
    advance(&mut d, 4);
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    let mut c = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
    c.properties = serde_json::from_value(serde_json::json!({
        "sequences": {"m": {"incarnation": 1, "next": (0..70).map(|s| (s.to_string(), serde_json::json!(1))).collect::<serde_json::Map<_, _>>()}}
    }))
    .unwrap();
    assert!(matches!(
        c.write_to(&mut Vec::new()),
        Err(CodecError::Invalid("property ledger bounds"))
    ));
}

#[test]
fn device_fault_handles_inputs_and_requests_roundtrip() {
    use crate::pv::{DeviceFault, DiskOp};
    let mut d = build(
        vec![
            spec(0, true, vec![(0, Response::Ready)]),
            spec(1, true, vec![(0, Response::Ready)]),
        ],
        vec![],
        true,
    );
    let install = |d: &mut Driver<Mock>, when, name: &str, fault| {
        d.with_barrier(|b| {
            b.schedule_with_handle(
                at(when),
                key(name),
                Action::DeviceFault {
                    target: Target::Name("fixture-1".into()),
                    fault,
                },
                FaultHandle::named(name.to_owned()).unwrap(),
            )
        })
        .unwrap();
    };
    install(
        &mut d,
        5,
        "slow",
        DeviceFault::DiskLatency {
            op: DiskOp::Flush,
            latency_ns: 3_000,
            jitter_ns: 7,
        },
    );
    install(&mut d, 15, "race", DeviceFault::Preempt { ppm: 77 });
    d.schedule(
        at(25),
        key("heal"),
        Action::Heal(HealTarget::Handle(
            FaultHandle::named("slow".to_owned()).unwrap(),
        )),
    )
    .unwrap();
    advance(&mut d, 10);
    exercise(d);
}
