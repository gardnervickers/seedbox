//! Private admission/codec tests. Probe executes no guest instructions.
use super::*;
use crate::clock::ClockRate;

struct Probe {
    first: bool,
}
impl Machine for Probe {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = bool;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while context.inbox.take().is_some() {}
        if self.first {
            context
                .outbox
                .emit(crate::pv::Output {
                    at: Timestamp::from_nanos(context.clock.now()),
                    payload: crate::pv::Response::Ready,
                })
                .unwrap();
            self.first = false;
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<bool, Self::Error> {
        Ok(self.first)
    }
}
fn driver(workers: usize) -> Driver<Probe> {
    Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        51,
        || Ok(()),
        fixture::definition(0),
        vec![fixture::definition(1)],
        vec![],
        NetworkConfig::default(),
        |_| Ok(Probe { first: true }),
    )
    .unwrap()
}
fn target() -> Target {
    Target::Machine(MachineId(1))
}
fn bytes(driver: &mut Driver<Probe>) -> Vec<u8> {
    let checkpoint = driver
        .checkpoint_map(|first| {
            Ok::<_, std::io::Error>(SnapshotReference::new([u8::from(first); 32], 1))
        })
        .unwrap();
    let mut bytes = vec![];
    checkpoint.write_to(&mut bytes).unwrap();
    bytes
}
fn prepare(driver: &mut Driver<Probe>, key: &str, action: Action, alias: &str) -> PreparedInput {
    driver
        .with_barrier(|barrier| {
            barrier.prepare_input_with_handle(
                InputKey::new(key).unwrap(),
                action,
                FaultHandle::named(alias).unwrap(),
            )
        })
        .unwrap()
}
#[test]
fn bounded_clock_prepare_is_pure_and_commit_matches_the_single_projected_draw() {
    for workers in [1, 4] {
        for action in [
            Action::DriftBounded {
                target: target(),
                max_rate_ppb: 100_000_000,
            },
            Action::StepRealtimeBounded {
                target: target(),
                max_offset_ns: 1_000_000,
            },
        ] {
            let mut d = driver(workers);
            let before = bytes(&mut d);
            let first = prepare(&mut d, "bounded", action.clone(), "clock");
            for _ in 0..4 {
                assert_eq!(prepare(&mut d, "bounded", action.clone(), "clock"), first);
                assert_eq!(bytes(&mut d), before, "proposal consumed source state");
            }
            let PreparedOutcome::Admitted(expected) = first.outcome().clone() else {
                panic!("clock admission expected")
            };
            match expected.action {
                FaultAction::Drift(rate) => {
                    assert!(i64::from(rate.ppb()).unsigned_abs() <= 100_000_000)
                }
                FaultAction::StepRealtime(offset) => assert!(offset.unsigned_abs() <= 1_000_000),
                other => panic!("wrong resolved clock action {other:?}"),
            }
            d.with_barrier(|barrier| barrier.commit_input(&first))
                .unwrap();
            let admitted: Vec<_> = d
                .drain_observations()
                .events
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::FaultAdmitted(record) => Some(record),
                    _ => None,
                })
                .collect();
            assert_eq!(admitted, [expected]);
        }
    }
}
#[test]
fn exact_clock_and_duplicate_rejection_leave_the_seeded_clock_stream_unchanged() {
    let mut baseline = driver(1);
    let baseline = prepare(
        &mut baseline,
        "baseline",
        Action::DriftBounded {
            target: target(),
            max_rate_ppb: 100_000_000,
        },
        "baseline",
    );
    let mut d = driver(4);
    let exact = prepare(
        &mut d,
        "exact",
        Action::Drift {
            target: target(),
            rate: ClockRate::new(5).unwrap(),
        },
        "exact",
    );
    d.with_barrier(|barrier| barrier.commit_input(&exact))
        .unwrap();
    d.with_barrier(|barrier| {
        barrier.schedule_with_handle(
            Timestamp::from_nanos(0),
            InputKey::new("rejected").unwrap(),
            Action::DriftBounded {
                target: target(),
                max_rate_ppb: 100_000_000,
            },
            FaultHandle::named("duplicate").unwrap(),
        )?;
        barrier.apply_due_now()
    })
    .unwrap();
    assert!(d.drain_events().iter().any(|event| matches!(&event.kind, EventKind::InputRejected { key, .. } if key.as_str() == "rejected")));
    let heal = d
        .with_barrier(|barrier| {
            barrier.prepare_input(
                InputKey::new("heal").unwrap(),
                Action::Heal(HealTarget::Handle(FaultHandle::named("exact").unwrap())),
            )
        })
        .unwrap();
    d.with_barrier(|barrier| barrier.commit_input(&heal))
        .unwrap();
    let next = prepare(
        &mut d,
        "next",
        Action::DriftBounded {
            target: target(),
            max_rate_ppb: 100_000_000,
        },
        "next",
    );
    let (PreparedOutcome::Admitted(a), PreparedOutcome::Admitted(b)) =
        (baseline.outcome(), next.outcome())
    else {
        panic!("clock admissions expected")
    };
    assert_eq!(a.action, b.action);
}
#[test]
fn invalid_bounds_are_refused_without_source_mutation() {
    for action in [
        Action::DriftBounded {
            target: target(),
            max_rate_ppb: 0,
        },
        Action::DriftBounded {
            target: target(),
            max_rate_ppb: 100_000_001,
        },
        Action::StepRealtimeBounded {
            target: target(),
            max_offset_ns: 0,
        },
        Action::StepRealtimeBounded {
            target: target(),
            max_offset_ns: u64::MAX,
        },
    ] {
        let mut d = driver(1);
        let before = bytes(&mut d);
        assert!(
            d.with_barrier(
                |barrier| barrier.prepare_input(InputKey::new("invalid").unwrap(), action)
            )
            .is_err()
        );
        assert_eq!(bytes(&mut d), before);
    }
}
#[test]
fn pending_bounded_actions_round_trip_and_restore_without_redrawing() {
    let mut source = driver(1);
    source
        .with_barrier(|barrier| {
            barrier.schedule_with_handle(
                Timestamp::from_nanos(10),
                InputKey::new("future-drift").unwrap(),
                Action::DriftBounded {
                    target: target(),
                    max_rate_ppb: 100_000_000,
                },
                FaultHandle::named("future").unwrap(),
            )?;
            barrier.schedule_with_handle(
                Timestamp::from_nanos(20),
                InputKey::new("future-step").unwrap(),
                Action::StepRealtimeBounded {
                    target: target(),
                    max_offset_ns: 1_000_000,
                },
                FaultHandle::named("step").unwrap(),
            )
        })
        .unwrap();
    let encoded = bytes(&mut source);
    let limits = DecodeLimits {
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
    };
    let checkpoint = Checkpoint::read_from(&mut encoded.as_slice(), limits)
        .unwrap()
        .try_map_snapshots(|reference| Ok::<_, std::io::Error>(reference.digest()[0] != 0))
        .unwrap();
    for workers in [1, 4] {
        let mut restored = Driver::<Probe>::from_checkpoint(
            &checkpoint,
            workers,
            || Ok(()),
            |first| Ok(Probe { first: *first }),
            |_| Ok(Probe { first: true }),
        )
        .unwrap();
        assert_eq!(bytes(&mut restored), encoded);
        restored.step(Timestamp::from_nanos(10)).unwrap();
        let actual: Vec<_> = restored
            .drain_observations()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::FaultAdmitted(record) => Some(record.action),
                _ => None,
            })
            .collect();
        let mut reference = driver(1);
        reference
            .with_barrier(|barrier| {
                barrier.schedule_with_handle(
                    Timestamp::from_nanos(10),
                    InputKey::new("future-drift").unwrap(),
                    Action::DriftBounded {
                        target: target(),
                        max_rate_ppb: 100_000_000,
                    },
                    FaultHandle::named("future").unwrap(),
                )
            })
            .unwrap();
        reference.step(Timestamp::from_nanos(10)).unwrap();
        let expected: Vec<_> = reference
            .drain_observations()
            .events
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::FaultAdmitted(record) => Some(record.action),
                _ => None,
            })
            .collect();
        assert_eq!(actual, expected);
    }
}
