//! Whole-Driver continuation with pure owned machines, never guests.
mod codec_tests;
use super::*;
use crate::driver::fixture::owned::*;
use crate::pv::{CommandResult, SimulationControlId};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn source_destroyed_capture_reused_workers_and_nonclone_snapshots() {
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
            spec(1, true, vec![(0, Response::Ready)]),
        ],
        vec![spec(2, true, vec![(0, Response::Ready)])],
        true,
    );
    for n in [2, 5, 9, 10, 13] {
        d.step(at(n)).unwrap();
    }
    let before = d.position();
    // Intra-block RNG cursors, not just u64-aligned draws, have accumulated.
    assert!(states(&d)[0].entropy.len() > 32);
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 60);
    let expected_states = states(&d);
    let expected_position = d.position();
    drop(d);
    for workers in [1, 4] {
        let mut branch = restore(&c, workers);
        assert_eq!(branch.position(), before);
        assert_eq!(advance(&mut branch, 60), expected);
        assert_eq!(states(&branch), expected_states);
        assert_eq!(branch.position(), expected_position);
        assert_eq!(
            branch.execution.machines.len(),
            3,
            "future spawn did not occur"
        );
    }
}

#[test]
fn seeded_constructor_is_exactly_old_private_seeded_stream() {
    let specs = vec![
        spec(0, true, vec![(0, Response::Ready)]),
        spec(1, true, vec![]),
    ];
    let mut old = build(specs.clone(), vec![], false);
    let mut owned = build(specs, vec![], true);
    for end in [1, 5, 10, 11, 20, 25, 30] {
        assert_eq!(old.step(at(end)).unwrap(), owned.step(at(end)).unwrap());
        assert_eq!(old.position(), owned.position());
        assert_eq!(old.drain_events(), owned.drain_events());
        assert_eq!(states(&old), states(&owned));
    }
    assert!(matches!(
        old.checkpoint(),
        Err(Error::CheckpointUnsupported)
    ));
}

#[test]
fn source_keeps_publications_but_imported_session_has_no_suffix() {
    for drain in [false, true] {
        let mut d = build(vec![spec(0, true, vec![])], vec![], true);
        if drain {
            d.drain_events();
        }
        // Staging is private pending publication, not equivalent to published.
        d.stage(
            at(0),
            EventKind::MachineAdded {
                machine: MachineId(0),
                cause: Cause::Host,
            },
        );
        let c = d.checkpoint().unwrap();
        assert_eq!(d.observations.staged_events.len(), 1);
        assert_eq!(d.observations.published.is_empty(), drain);
        reference_observer_session(&mut d);
        let expected = advance(&mut d, 11);
        for workers in [1, 4] {
            let mut b = restore(&c, workers);
            assert_eq!(advance(&mut b, 11), expected);
            assert!(b.drain_events().is_empty());
        }
    }
}

#[test]
fn unsupported_poisoned_and_structurally_invalid_capture_never_calls_machine() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], false);
    let calls = d.execution.machines[0]
        .machine
        .as_ref()
        .unwrap()
        .captures
        .clone();
    assert!(matches!(d.checkpoint(), Err(Error::CheckpointUnsupported)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    d.execution.failed = true;
    assert!(matches!(d.checkpoint(), Err(Error::Failed)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.execution.machines[0].rng = None;
    assert!(matches!(d.checkpoint(), Err(Error::InvalidCheckpoint(_))));
}

#[test]
fn nth_capture_failure_leaves_source_and_rng_unchanged() {
    let initial = vec![spec(0, true, vec![]), spec(1, true, vec![])];
    let mut d = build(initial.clone(), vec![], true);
    let mut reference = build(initial, vec![], true);
    d.step(at(5)).unwrap();
    reference.step(at(5)).unwrap();
    let before = d.position();
    d.execution.machines[1]
        .machine
        .as_mut()
        .unwrap()
        .state
        .fail_capture = true;
    assert!(matches!(
        d.checkpoint(),
        Err(Error::Machine {
            id: MachineId(1),
            ..
        })
    ));
    assert_eq!(d.position(), before);
    assert!(!d.execution.failed);
    d.execution.machines[1]
        .machine
        .as_mut()
        .unwrap()
        .state
        .fail_capture = false;
    assert_eq!(advance(&mut d, 30), advance(&mut reference, 30));
    assert_eq!(states(&d), states(&reference));
    assert!(d.checkpoint().is_ok());
}

#[test]
fn invalid_restore_is_rejected_before_context_or_machine_callbacks() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    let mut c = d.checkpoint().unwrap();
    c.quantum_ns = 0;
    let result = Driver::<Mock>::from_checkpoint(
        &c,
        1,
        || panic!("context before validation"),
        |_| panic!("machine before validation"),
        fresh,
    );
    assert!(matches!(result, Err(Error::InvalidCheckpoint(_))));
    c.quantum_ns = 10;
    let result = Driver::<Mock>::from_checkpoint(
        &c,
        0,
        || panic!("context with zero workers"),
        |_| panic!("restore with zero workers"),
        fresh,
    );
    assert!(matches!(result, Err(Error::InvalidWorkerCount)));
}

#[test]
fn partial_restore_and_context_failure_drop_machines_and_join_contexts() {
    let mut d = build(
        vec![spec(0, true, vec![]), spec(1, true, vec![])],
        vec![],
        true,
    );
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    drop(d);
    let contexts = Arc::new(AtomicUsize::new(0));
    let machines = Arc::new(AtomicUsize::new(0));
    let count = contexts.clone();
    let dropped = machines.clone();
    let mut restored = 0;
    let result = Driver::<Mock>::from_checkpoint(
        &c,
        4,
        move || Ok(Context(Some(count.clone()))),
        move |s| {
            restored += 1;
            if restored == 2 {
                return Err(io("nth restore"));
            }
            let mut m = Mock::restore(s)?;
            m.dropped = Some(dropped.clone());
            Ok(m)
        },
        |_| panic!("future factory used for restore"),
    );
    assert!(matches!(
        result,
        Err(Error::Machine {
            id: MachineId(1),
            ..
        })
    ));
    assert_eq!(machines.load(Ordering::SeqCst), 1);
    assert_eq!(
        contexts.load(Ordering::SeqCst),
        4,
        "return must join contexts"
    );
    let attempts = Arc::new(AtomicUsize::new(0));
    let count = contexts.clone();
    let attempt = attempts.clone();
    let result = Driver::<Mock>::from_checkpoint(
        &c,
        4,
        move || {
            let n = attempt.fetch_add(1, Ordering::SeqCst);
            if n == 2 {
                Err(io("context failure"))
            } else {
                Ok(Context(Some(count.clone())))
            }
        },
        |_| panic!("restore before context startup completed"),
        fresh,
    );
    assert!(matches!(result, Err(Error::Context { worker: 2, .. })));
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
    assert_eq!(contexts.load(Ordering::SeqCst), 6);
    assert!(
        restore(&c, 1).checkpoint().is_ok(),
        "failure damaged reusable capture"
    );
}

#[test]
fn paused_lagging_blocked_fractional_timer_and_handles_continue() {
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
    assert!(d.execution.machines[2].lifecycle.paused());
    assert!(d.execution.machines[2].position < at(3));
    assert_eq!(d.execution.machines[1].timer, Some(at(14)));
    assert!(matches!(
        d.execution.inputs[&key("drift")],
        LiveInput::Handle(_)
    ));
    // Retained due-but-unconsumed input is machine-time Entry state, not a
    // future Scheduled item. Capture must keep it while this machine is paused.
    d.execution.machines[2].inbox.push_back(Input {
        at: at(0),
        payload: Request::SimulationReply {
            id: SimulationControlId {
                session: nz(9),
                request: nz(4),
            },
            payload: b"retained".to_vec(),
        },
    });
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 25);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(advance(&mut b, 25), expected);
        assert_eq!(states(&b), states(&d));
        assert!(!b.execution.inputs.contains_key(&key("drift")));
        assert_eq!(b.position(), d.position());
    }
}

#[test]
fn before_fault_keeps_admitted_handle_and_applies_held_effect_once() {
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
    let held = status(&mut d);
    assert_eq!(
        d.execution.machines[1]
            .lifecycle
            .execution()
            .factor_permille(),
        1000
    );
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    let expected = advance(&mut d, 20);
    assert_eq!(
        d.execution.machines[1]
            .lifecycle
            .execution()
            .factor_permille(),
        2000
    );
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(status(&mut b), held);
        let position = b.position();
        assert!(matches!(
            b.step(at(100)).unwrap(),
            StepOutcome::BeforeFault { .. }
        ));
        assert_eq!(b.position(), position);
        b.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        assert_eq!(advance(&mut b, 20), expected);
        assert_eq!(
            b.execution.machines[1]
                .lifecycle
                .execution()
                .factor_permille(),
            2000
        );
    }
}

#[test]
fn pending_commands_replies_sleep_and_partial_trigger_keep_control_cursors() {
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
    // Command completion at 7 and sleep wake at 15 are still pending; trigger
    // has consumed x but not y, so restarting its matcher would miss the effect.
    assert!(d.execution.events.keys().any(|t| *t > at(5)));
    assert!(matches!(
        d.execution.inputs.get(&key("pause-process")),
        Some(LiveInput::Admitted { .. })
    ));
    assert!(d.execution.trigger_marker.is_some());
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 30);
    assert!(expected.iter().any(|e| matches!(
        e.kind,
        EventKind::TriggerResolved {
            result: TriggerResult::Fired,
            ..
        }
    )));
    assert!(
        states(&d)[0]
            .received
            .iter()
            .filter(|i| matches!(i.payload, Request::SimulationReply { .. }))
            .count()
            >= 2
    );
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(advance(&mut b, 30), expected);
        assert_eq!(states(&b), states(&d));
        assert_eq!(b.position(), d.position());
    }
}

#[test]
fn causal_network_flights_pair_cursors_fifo_and_stacked_cuts_continue() {
    let mut d = build_network(
        vec![
            spec(0, true, vec![(20, frame(mac(1), 5))]),
            spec(
                1,
                true,
                vec![
                    (1, frame([255; 6], 1)),
                    (2, frame(mac(0), 2)),
                    (18, frame(mac(0), 3)),
                    (25, frame(mac(0), 4)),
                ],
            ),
            spec(2, false, vec![]),
        ],
        vec![],
        true,
        true,
    );
    for (when, name, action) in [
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
        (4, "crash", Action::Crash(Target::Machine(MachineId(1)))),
        (5, "pause-rx", Action::Pause(Target::Machine(MachineId(0)))),
        (10, "reboot", Action::Reboot(Target::Machine(MachineId(1)))),
        (12, "heal-a", Action::Heal(HealTarget::Input(key("cut-a")))),
        (
            15,
            "resume-rx",
            Action::Resume(Target::Machine(MachineId(0))),
        ),
        (22, "heal-b", Action::Heal(HealTarget::Input(key("cut-b")))),
    ] {
        d.schedule(at(when), key(name), action).unwrap();
    }
    advance(&mut d, 3);
    assert!(!d.execution.network.in_flight().is_empty());
    assert!(matches!(
        d.execution.inputs.get(&key("cut-a")),
        Some(LiveInput::Handle(_))
    ));
    assert!(matches!(
        d.execution.inputs.get(&key("cut-b")),
        Some(LiveInput::Handle(_))
    ));
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 35);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(advance(&mut b, 35), expected);
        assert_eq!(states(&b), states(&d));
        assert_eq!(
            b.execution.network.pair_cursor(MachineId(1), MachineId(0)),
            d.execution.network.pair_cursor(MachineId(1), MachineId(0))
        );
        assert!(
            b.execution.network.is_idle(),
            "remaining flights: {:?}",
            b.execution.network.in_flight()
        );
        // Even an empty-flight capture must retain pair RNG and FIFO history.
        let c2 = b.checkpoint().unwrap();
        reference_observer_session(&mut b);
        let mut b2 = restore(&c2, workers);
        for router in [&mut b.execution.network, &mut b2.execution.network] {
            let Response::TransmitFrame { bytes } = frame(mac(0), 6) else {
                unreachable!()
            };
            router.route(MachineId(1), 35, &bytes).unwrap();
        }
        assert_eq!(
            b.execution.network.in_flight(),
            b2.execution.network.in_flight()
        );
        assert_eq!(
            b.execution.network.pair_cursor(MachineId(1), MachineId(0)),
            b2.execution.network.pair_cursor(MachineId(1), MachineId(0))
        );
    }
}

#[test]
fn finished_oracle_and_permanent_stop_are_capturable_but_not_resumed() {
    for permanent in [false, true] {
        let mut d = build(
            vec![spec(
                0,
                true,
                vec![(3, Response::ApplicationOutcome { code: 17 })],
            )],
            vec![],
            true,
        );
        if permanent {
            d.with_barrier(|b| {
                b.stop();
                Ok(())
            })
            .unwrap();
        } else {
            advance(&mut d, 5);
        }
        d.execution.oracle_stops.insert((7, nz(31)));
        let before = d.position();
        let c = d.checkpoint().unwrap();
        reference_observer_session(&mut d);
        let expected = d.step(at(100)).unwrap();
        let publications = d.drain_events();
        drop(d);
        let mut b = restore(&c, 4);
        assert_eq!(b.step(at(100)).unwrap(), expected);
        assert_eq!(b.position(), before);
        assert_eq!(b.drain_events(), publications);
        assert!(b.execution.oracle_stops.contains(&(7, nz(31))));
    }
}

#[test]
fn seeded_clock_fault_cursor_and_pending_input_rejections_continue() {
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
    // One live key and a future already-invalid state transition: neither may
    // disappear or be admitted twice when its boundary is restored.
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
    assert!(matches!(
        d.execution.inputs.get(&key("pause")),
        Some(LiveInput::Pending)
    ));
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 25);
    assert_eq!(
        states(&d)[1]
            .received
            .iter()
            .filter(|i| matches!(i.payload, Request::StepRealtime { .. }))
            .count(),
        2,
        "both seeded clock faults must reach the guest, not fail schema/admission"
    );
    assert!(expected.iter().any(
        |e| matches!(&e.kind, EventKind::InputRejected { key: k, .. } if k == &key("reject-pause"))
    ));
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(advance(&mut b, 25), expected);
        assert_eq!(states(&b), states(&d));
    }
}

#[test]
fn staged_prior_boot_ack_does_not_consume_restored_oracle_stop() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    let command = nz(7);
    d.execution
        .control
        .admission(
            &mut d.execution.machines,
            MachineId(0),
            at(0),
            crate::oracle::observation::Transition::Reboot { new_boot: 1 },
        )
        .unwrap();
    d.execution
        .control
        .offered(
            MachineId(0),
            command,
            crate::oracle::service::CommandKind::Application(crate::pv::ApplicationCommand::Stop),
            crate::oracle::observation::Origin::External,
        )
        .unwrap();
    d.execution
        .control
        .accepted(
            MachineId(0),
            &mut d.execution.machines[0].lifecycle,
            1,
            command,
            at(0),
            at(0),
        )
        .unwrap();
    d.execution.oracle_stops.insert((1, command));
    let ack = |boot| MachineResponse {
        machine: MachineId(0),
        boot,
        at: at(0),
        payload: Response::CommandAcknowledged {
            id: command,
            result: CommandResult::Completed,
        },
    };
    d.execution.machines[0].staged.push(ack(0));
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    drop(d);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        b.commit_boundary().unwrap();
        assert!(b.execution.finished.is_none());
        assert!(b.execution.oracle_stops.contains(&(1, command)));
        b.execution.machines[0].staged.push(ack(1));
        b.commit_boundary().unwrap();
        assert_eq!(b.execution.finished, Some((at(0), ExitReason::Shutdown)));
        assert!(!b.execution.oracle_stops.contains(&(1, command)));
    }
}

#[test]
fn reserved_spawn_identity_is_retained_without_factory_or_seed_re_admission() {
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
    assert!(matches!(
        d.step(at(0)).unwrap(),
        StepOutcome::BeforeFault { .. }
    ));
    assert_eq!(d.execution.machines.len(), 1);
    assert!(d.execution.control.spawn_pending());
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    let expected = advance(&mut d, 20);
    assert_eq!(d.execution.machines.len(), 2);
    drop(d);
    for workers in [1, 4] {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let mut b = Driver::from_checkpoint(
            &c,
            workers,
            || Ok(Context(None)),
            Mock::restore,
            move |definition| {
                calls.fetch_add(1, Ordering::SeqCst);
                fresh(definition)
            },
        )
        .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(b.execution.control.spawn_pending());
        b.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        assert_eq!(advance(&mut b, 20), expected);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
    let mut failed = Driver::from_checkpoint(
        &c,
        1,
        || Ok(Context(None)),
        Mock::restore,
        |_| Err(io("future construction failed")),
    )
    .unwrap();
    failed
        .with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    assert!(failed.step(at(20)).is_err());
    assert!(matches!(failed.checkpoint(), Err(Error::Failed)));
    assert!(matches!(failed.step(at(20)), Err(Error::Failed)));
}

#[test]
fn paused_machine_need_not_map_the_current_shared_time_to_be_captured() {
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
    // Future work keeps the simulation from quiescing, but is not due yet.
    d.schedule(
        at(u64::MAX - 1),
        key("future"),
        Action::Resume(Target::Machine(MachineId(0))),
    )
    .unwrap();
    d.step(at(0)).unwrap();
    d.step(at(u64::MAX - 2)).unwrap();
    assert_eq!(d.now(), at(u64::MAX - 2));
    assert!(d.execution.machines[0]
        .time
        .floor(d.now().as_nanos())
        .is_err());
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    drop(d);
    let mut b = restore(&c, 4);
    assert_eq!(b.now(), at(u64::MAX - 2));
    assert!(b.execution.machines[0].lifecycle.paused());
    assert!(b.checkpoint().is_ok());
}

#[test]
fn limited_playback_reanchors_at_restored_time_without_charging_downtime() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    let rate = PlaybackRate::Limited {
        numerator: nz(1),
        denominator: nz(1),
    };
    // Opaque restore pacing origin; all tested step_at times are explicit origin-plus-offset inputs, not live elapsed time.
    let origin = Instant::now();
    d.playback.set_rate(rate, d.position(), origin);
    d.step_at(at(3), origin).unwrap();
    let position = d.position();
    assert_eq!(position.at, at(3));
    let logical = status(&mut d);
    assert_eq!(logical.rate, rate);
    assert_eq!(
        d.playback.delay(position, origin),
        Some(Duration::from_nanos(3))
    );
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    // Capture resets only its owned anchor, not the live source's pacing debt.
    assert_eq!(
        d.playback.delay(position, origin),
        Some(Duration::from_nanos(3))
    );
    let expected = {
        d.step_at(at(4), origin + Duration::from_nanos(3)).unwrap();
        d.drain_events()
    };
    assert_eq!(
        d.playback
            .delay(d.position(), origin + Duration::from_nanos(3)),
        Some(Duration::from_nanos(1))
    );
    drop(d);
    for workers in [1, 4] {
        // A controlled future Instant, not a sleep or a real wall deadline.
        let after_downtime = origin + Duration::from_secs(3600);
        let mut b = restore(&c, workers);
        assert_eq!(status(&mut b), logical);
        assert_eq!(b.playback.delay(position, after_downtime), None);
        b.step_at(at(4), after_downtime).unwrap();
        assert_eq!(b.drain_events(), expected);
        // The hour of host downtime cannot pay for new simulation progress.
        assert_eq!(
            b.playback.delay(b.position(), after_downtime),
            Some(Duration::from_nanos(1))
        );
        assert_eq!(
            b.playback
                .delay(b.position(), after_downtime + Duration::from_nanos(1)),
            None
        );
        assert_eq!(status(&mut b).rate, rate);
    }
}

#[test]
fn simulation_admission_rng_crosses_cache_boundary_after_three_captured_seeds() {
    let mut d = build(
        (0..3).map(|i| spec(i, true, vec![])).collect(),
        vec![spec(3, true, vec![(0, Response::Ready)])],
        true,
    );
    advance(&mut d, 3);
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let mut reference = SeededRng::new(51);
    let mut first_three = [0; 24];
    reference.fill_bytes(&mut first_three);
    let mut later_seeds = [0; 16];
    reference.fill_bytes(&mut later_seeds);
    let mut captured_cursor = c.rng.clone();
    let mut actual_seeds = [0; 16];
    captured_cursor.fill_bytes(&mut actual_seeds);
    assert_eq!(actual_seeds, later_seeds);
    // Fourth seed finishes cached block 0; fifth starts block 1. One is a
    // host addition and one is a declared spawn, both after capture.
    fn admit(d: &mut Driver<Mock>) -> Vec<Event> {
        let id = d
            .with_barrier(|b| b.add(|| Ok(Mock::new(spec(99, true, vec![])))))
            .unwrap();
        assert_eq!(id, MachineId(3));
        d.schedule(
            d.now(),
            key("later-spawn"),
            Action::Spawn {
                name: "fixture-3".into(),
            },
        )
        .unwrap();
        advance(d, 20)
    }
    fn next_simulation_bytes(d: &Driver<Mock>) -> Vec<u8> {
        let SimulationRng::Seeded(rng) = &d.execution.rng else {
            panic!("owned RNG expected")
        };
        let mut rng = rng.clone();
        let mut bytes = vec![0; 45];
        rng.fill_bytes(&mut bytes);
        bytes
    }
    let expected = admit(&mut d);
    assert_eq!(d.execution.machines.len(), 5);
    let expected_states = states(&d);
    let expected_position = d.position();
    let mut reference_next = vec![0; 45];
    reference.fill_bytes(&mut reference_next);
    assert_eq!(next_simulation_bytes(&d), reference_next);
    for (id, seed) in [MachineId(3), MachineId(4)]
        .into_iter()
        .zip(later_seeds.as_chunks::<8>().0)
    {
        let seed = u64::from_le_bytes(*seed);
        let mut machine_rng = SeededRng::new(seed);
        let mut bytes = vec![0; expected_states[id.0].entropy.len()];
        machine_rng.fill_bytes(&mut bytes);
        assert_eq!(
            bytes, expected_states[id.0].entropy,
            "admitted machine received wrong seed"
        );
    }
    drop(d);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(admit(&mut b), expected);
        assert_eq!(states(&b), expected_states);
        assert_eq!(b.position(), expected_position);
        assert_eq!(next_simulation_bytes(&b), reference_next);
    }
}

#[test]
fn in_flight_frame_is_delivered_to_receivers_rebooted_current_boot() {
    let sent = frame([255; 6], 71);
    let mut d = build_network(
        vec![
            spec(0, true, vec![(1, sent.clone())]),
            spec(1, true, vec![]),
            spec(2, false, vec![]),
        ],
        vec![],
        true,
        true,
    );
    d.schedule(
        at(3),
        key("receiver-crash"),
        Action::Crash(Target::Machine(MachineId(1))),
    )
    .unwrap();
    d.schedule(
        at(4),
        key("receiver-reboot"),
        Action::Reboot(Target::Machine(MachineId(1))),
    )
    .unwrap();
    advance(&mut d, 2);
    let flights = d.execution.network.in_flight();
    let arrival = flights
        .iter()
        .find(|(_, from, to, _)| *from == MachineId(0) && *to == MachineId(1))
        .unwrap()
        .0;
    assert!(arrival > 4);
    assert_eq!(d.execution.machines[1].lifecycle.boot(), 0);
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 15);
    let expected_states = states(&d);
    let expected_position = d.position();
    assert_eq!(d.execution.machines[1].lifecycle.boot(), 1);
    let inputs = &expected_states[1].received;
    let crash = inputs
        .iter()
        .position(|i| matches!(i.payload, Request::Crash))
        .unwrap();
    let reboot = inputs
        .iter()
        .position(|i| matches!(i.payload, Request::Reboot))
        .unwrap();
    let frames: Vec<_> = inputs
        .iter()
        .enumerate()
        .filter(|(_, i)| matches!(i.payload, Request::ReceiveFrame { .. }))
        .collect();
    assert_eq!(frames.len(), 1);
    assert!(crash < reboot && reboot < frames[0].0);
    assert_eq!(inputs[crash].at, at(3));
    assert_eq!(inputs[reboot].at, at(4));
    assert_eq!(frames[0].1.at, at(arrival));
    let Response::TransmitFrame { bytes } = sent else {
        unreachable!()
    };
    assert_eq!(frames[0].1.payload, Request::ReceiveFrame { bytes });
    drop(d);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert_eq!(advance(&mut b, 15), expected);
        assert_eq!(states(&b), expected_states);
        assert_eq!(b.position(), expected_position);
        assert_eq!(b.execution.machines[1].lifecycle.boot(), 1);
    }
}

#[test]
fn nth_capture_failure_drops_partial_saved_owners_without_poisoning_source() {
    let mut d = build(
        (0..3).map(|i| spec(i, true, vec![])).collect(),
        vec![],
        true,
    );
    let dropped = Arc::new(AtomicUsize::new(0));
    for entry in &mut d.execution.machines {
        entry.machine.as_mut().unwrap().saved_drops = Some(dropped.clone());
    }
    d.execution.machines[2]
        .machine
        .as_mut()
        .unwrap()
        .state
        .fail_capture = true;
    let before = states(&d);
    let position = d.position();
    assert!(matches!(
        d.checkpoint(),
        Err(Error::Machine {
            id: MachineId(2),
            ..
        })
    ));
    assert_eq!(
        dropped.load(Ordering::SeqCst),
        2,
        "partial snapshots must drop before return"
    );
    assert_eq!(states(&d), before);
    assert_eq!(d.position(), position);
    assert!(!d.execution.failed);
    d.execution.machines[2]
        .machine
        .as_mut()
        .unwrap()
        .state
        .fail_capture = false;
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
    drop(c);
    assert_eq!(dropped.load(Ordering::SeqCst), 5);
    assert!(d.step(at(1)).is_ok());
}

#[test]
fn unfinished_quantum_and_pending_step_target_survive_restore() {
    let mut d = build(vec![spec(0, true, vec![])], vec![], true);
    d.step(at(3)).unwrap();
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Paused)?;
        b.step_quantum()
    })
    .unwrap();
    let c = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    assert_eq!(c.quantum_end, at(10));
    assert!(status(&mut d).stepping);
    let expected = advance(&mut d, 10);
    assert_eq!(d.execution.completed_quanta, 1);
    assert!(!status(&mut d).stepping);
    for workers in [1, 4] {
        let mut b = restore(&c, workers);
        assert!(status(&mut b).stepping);
        assert_eq!(advance(&mut b, 10), expected);
        assert_eq!(status(&mut b), status(&mut d));
        assert!(matches!(
            b.step(at(20)).unwrap(),
            StepOutcome::Waiting { .. }
        ));
    }
}
