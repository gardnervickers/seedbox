//! Pure admission/dispatch tests using the existing snapshot/codec mock.
use crate::driver::fixture::owned::*;
use crate::driver::*;
use crate::persistence::{Encoder, Wire};
use std::sync::Mutex;

type BudgetCalls = Arc<Mutex<Vec<(u64, u64)>>>;

fn ready() -> Driver<Mock> {
    let mut d = build(
        (0..3)
            .map(|i| spec(i, true, vec![(0, Response::Ready)]))
            .collect(),
        vec![],
        true,
    );
    advance(&mut d, 3);
    d.drain_events();
    d
}
fn prepare(d: &mut Driver<Mock>, key_: &str, action: Action) -> PreparedInput {
    d.with_barrier(|b| b.prepare_input(key(key_), action))
        .unwrap()
}
fn due(d: &mut Driver<Mock>) -> DueOutcome {
    d.with_barrier(|b| b.apply_due_now()).unwrap()
}
fn commit(d: &mut Driver<Mock>, p: &PreparedInput) -> DueOutcome {
    d.with_barrier(|b| b.commit_input(p)).unwrap()
}
fn record(p: &PreparedInput) -> &FaultRecord {
    let PreparedOutcome::Admitted(record) = p.outcome() else {
        panic!("expected admission")
    };
    record
}
fn pause() -> Action {
    Action::Pause(Target::Machine(MachineId(1)))
}
fn throttle() -> Action {
    Action::Throttle {
        target: Target::Machine(MachineId(1)),
        config: ExecutionConfig::new(2000).unwrap(),
    }
}

/// Honest internal fingerprint: no capture/validation is claimed for poisoned or
/// intermediate synthetic states. Encode every deterministic Driver field plus
/// failed and native mock state; compare Playback itself too (including anchor).
fn internal(d: &Driver<Mock>) -> (Vec<u8>, Playback) {
    let Driver {
        execution,
        playback,
        observations,
        workers: _,
        factory: _,
    } = d;
    let ExecutionState {
        clock,
        rng,
        quantum_ns,
        quantum_end,
        machines,
        events,
        boundary_index,
        completed_quanta,
        quantum_committed,
        failed,
        stopped,
        oracle,
        finished,
        oracle_stops,
        control,
        inputs,
        trigger_marker,
        definitions,
        network,
        engine: _,
    } = execution;
    let ObservationState {
        staged_events,
        published,
        next_sequence,
        frames: _,
    } = observations;
    let SimulationRng::Seeded(rng) = rng else {
        panic!("owned RNG required by probe")
    };
    let mut e = Encoder { output: Vec::new() };
    macro_rules! put { ($($v:expr),* $(,)?) => { $(($v).put(&mut e).unwrap();)* }; }
    put!(
        clock.now(),
        rng,
        quantum_ns,
        quantum_end,
        events,
        staged_events,
        published,
        next_sequence,
        boundary_index,
        completed_quanta,
        quantum_committed,
        playback.checkpoint(),
        failed,
        stopped,
        oracle,
        finished,
        oracle_stops,
        control,
        inputs,
        trigger_marker,
        definitions,
        network
    );
    machines.len().put(&mut e).unwrap();
    for entry in machines {
        Entry {
            machine: Some(entry.machine.as_ref().unwrap().state.clone()),
            rng: entry.rng.clone(),
            lifecycle: entry.lifecycle.clone(),
            time: entry.time,
            position: entry.position,
            timer: entry.timer,
            reached: entry.reached,
            stopped: entry.stopped,
            inbox: entry.inbox.clone(),
            staged: entry.staged.clone(),
            last_ended_published: entry.last_ended_published,
        }
        .put(&mut e)
        .unwrap();
    }
    (e.output, playback.clone())
}
fn bytes(d: &mut Driver<Mock>) -> (Vec<u8>, Store) {
    artifact(d.checkpoint().unwrap())
}
fn no_change(d: &mut Driver<Mock>, f: impl FnOnce(&mut Driver<Mock>)) {
    let old = internal(d);
    let encoded = bytes(d);
    f(d);
    assert_eq!(internal(d), old);
    assert_eq!(bytes(d), encoded);
}
fn applied_pause(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind,
                EventKind::Transition {
                    machine: MachineId(1),
                    change: Transition::Paused,
                    ..
                }
            )
        })
        .count()
}

#[test]
fn paused_commit_holds_without_touching_anchor_or_quantum_and_releases_once() {
    for stepping_release in [false, true] {
        let mut d = ready();
        d.with_barrier(|b| b.set_rate(PlaybackRate::Paused))
            .unwrap();
        let before = d.playback.clone();
        let position = d.position();
        let quantum_end = d.execution.quantum_end;
        let native = states(&d);
        let p = prepare(&mut d, "live", pause());
        assert_eq!(p.key(), &key("live"));
        assert_eq!(p.action(), &pause());
        assert_eq!(p.position(), position);
        assert_eq!(record(&p).fault.get(), 1);
        assert_eq!(
            commit(&mut d, &p),
            DueOutcome::BeforeFault {
                at: at(3),
                fault: Box::new(record(&p).clone())
            }
        );
        let mut held = before;
        held.held = Some(record(&p).clone());
        assert_eq!(
            d.playback, held,
            "including exact host anchor and step target"
        );
        assert_eq!(d.now(), position.at);
        assert_eq!(d.execution.quantum_end, quantum_end);
        assert_eq!(d.execution.completed_quanta, position.completed_quanta);
        assert_eq!(states(&d), native, "held effect must not run a machine");
        assert!(!d.execution.machines[1].lifecycle.paused());
        assert_eq!(d.position().boundary_index, position.boundary_index + 1);
        let events = d.drain_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(&e.kind, EventKind::FaultAdmitted(f) if f == record(&p)))
                .count(),
            1
        );
        assert_eq!(applied_pause(&events), 0);
        no_change(&mut d, |d| {
            assert!(matches!(
                d.with_barrier(|b| b.prepare_input(key("other"), pause())),
                Err(Error::BeforeFault)
            ));
            assert!(matches!(
                d.with_barrier(|b| b.commit_input(&p)),
                Err(Error::BeforeFault)
            ));
            assert!(matches!(
                d.with_barrier(|b| b.apply_due_now()),
                Err(Error::BeforeFault)
            ));
        });
        d.with_barrier(|b| b.set_rate(PlaybackRate::Paused))
            .unwrap();
        let before = internal(&d);
        assert!(matches!(
            d.step(at(3)).unwrap(),
            StepOutcome::BeforeFault { .. }
        ));
        assert_eq!(internal(&d), before);
        if stepping_release {
            d.with_barrier(|b| b.step_quantum()).unwrap();
            d.step(quantum_end).unwrap();
            assert_eq!(d.execution.completed_quanta, position.completed_quanta + 1);
            assert!(!status(&mut d).stepping);
        } else {
            d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
                .unwrap();
            d.step(at(3)).unwrap();
            assert_eq!(d.execution.completed_quanta, position.completed_quanta);
        }
        assert!(d.playback.held.is_none());
        assert!(d.execution.machines[1].lifecycle.paused());
        let events = d.drain_events();
        assert_eq!(applied_pause(&events), 1);
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::FaultSettled { record: fault, status: SettlementStatus::Applied, .. } if fault == record(&p))).count(), 1);
        let now = d.now();
        d.step(now).unwrap();
        assert!(d.drain_events().is_empty());
    }
}

#[test]
fn pending_step_survives_due_dispatch_and_next_step_finishes_exactly_one_quantum() {
    let mut d = ready();
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Paused)?;
        b.step_quantum()
    })
    .unwrap();
    let playback = d.playback.clone();
    let now = d.now();
    let end = d.execution.quantum_end;
    let quanta = d.execution.completed_quanta;
    let p = prepare(&mut d, "pause", pause());
    assert_eq!(commit(&mut d, &p), DueOutcome::Applied { at: now });
    assert_eq!(d.playback, playback);
    assert_eq!(d.now(), now);
    assert_eq!(d.execution.quantum_end, end);
    assert_eq!(d.execution.completed_quanta, quanta);
    assert!(d.execution.machines[1].lifecycle.paused());
    d.step(end).unwrap();
    assert_eq!(d.execution.completed_quanta, quanta + 1);
    assert!(!status(&mut d).stepping);
    assert!(matches!(
        d.step(at(30)).unwrap(),
        StepOutcome::Waiting { .. }
    ));
}

#[test]
fn limited_waiting_bypassed_without_reanchor_but_armed_hold_uses_existing_semantics() {
    for armed in [false, true] {
        let mut d = ready();
        let rate = PlaybackRate::Limited {
            numerator: nz(1),
            denominator: nz(1),
        };
        // Opaque pacing origin; step_at receives this caller-selected instant and exact offsets, never an elapsed-host oracle.
        let wall = Instant::now() + Duration::from_secs(60);
        d.playback.set_rate(rate, d.position(), wall);
        d.step_at(at(5), wall).unwrap();
        assert!(matches!(
            d.step_at(at(6), wall).unwrap(),
            StepOutcome::Waiting { .. }
        ));
        if armed {
            d.playback.before_next = true;
        }
        let playback = d.playback.clone();
        let now = d.now();
        let end = d.execution.quantum_end;
        let quanta = d.execution.completed_quanta;
        let p = prepare(&mut d, "throttle", throttle());
        let result = commit(&mut d, &p);
        if armed {
            assert_eq!(
                result,
                DueOutcome::BeforeFault {
                    at: now,
                    fault: Box::new(record(&p).clone())
                }
            );
            assert_eq!(d.playback.rate, PlaybackRate::Paused);
            assert!(!d.playback.before_next);
            assert_eq!(
                d.execution.machines[1]
                    .lifecycle
                    .execution()
                    .factor_permille(),
                1000
            );
        } else {
            assert_eq!(result, DueOutcome::Applied { at: now });
            assert_eq!(d.playback, playback);
            assert_eq!(
                d.execution.machines[1]
                    .lifecycle
                    .execution()
                    .factor_permille(),
                2000
            );
        }
        assert_eq!(d.now(), now);
        assert_eq!(d.execution.quantum_end, end);
        assert_eq!(d.execution.completed_quanta, quanta);
    }
}

#[test]
fn pure_prepare_and_aborted_journal_do_not_allocate_or_publish() {
    let mut d = ready();
    for action in [
        pause(),
        throttle(),
        Action::Spawn {
            name: "missing".into(),
        },
    ] {
        no_change(&mut d, |d| {
            let result = d.with_barrier(|b| b.prepare_input(key("proposal"), action));
            drop(result); // model a failed journal: no commit is sent
        });
    }
    let p = prepare(&mut d, "first", throttle());
    assert_eq!(record(&p).fault.get(), 1);
    assert_eq!(record(&p).handle.as_ref().unwrap().to_string(), "p1");
    commit(&mut d, &p);
    no_change(&mut d, |d| {
        let rejected = prepare(d, "duplicate", throttle());
        assert!(matches!(
            rejected.outcome(),
            PreparedOutcome::Rejected(InputRejection {
                reason: RejectionReason::InvalidState,
                ..
            })
        ));
        assert!(matches!(
            d.with_barrier(|b| b.commit_input(&rejected)),
            Err(Error::InvalidRequest)
        ));
    });
    // A rejected throttle consumes a handle on a clone, never on the source.
    let p = prepare(
        &mut d,
        "heal",
        Action::Heal(HealTarget::Input(key("first"))),
    );
    assert_eq!(record(&p).fault.get(), 2);
    commit(&mut d, &p);
    let p = prepare(&mut d, "next", throttle());
    assert_eq!(record(&p).fault.get(), 3);
    assert_eq!(record(&p).handle.as_ref().unwrap().to_string(), "p2");
}

#[test]
fn unstaged_settlement_notice_is_projected_for_heal_without_mutating_real_inputs() {
    let mut d = ready();
    let now = d.now();
    d.execution
        .inputs
        .insert(key("setting"), LiveInput::Pending);
    let (setting, _) = Driver::<Mock>::input_admission(
        &mut d.execution.control,
        &d.execution.machines,
        &d.execution.inputs,
        &key("setting"),
        &throttle(),
        now,
    )
    .unwrap()
    .unwrap();
    d.execution
        .control
        .invoke(&d.execution.machines, |service, _| {
            service.applied(setting, 3, 3)
        })
        .unwrap();
    let before = internal(&d);
    // Without projecting both unstaged notices, Pending cannot resolve Heal(Input).
    let q = prepare(
        &mut d,
        "healing",
        Action::Heal(HealTarget::Input(key("setting"))),
    );
    assert_eq!(record(&q).action, FaultAction::Heal);
    assert_eq!(record(&q).handle.as_ref().unwrap().to_string(), "p1");
    assert_eq!(internal(&d), before);
    assert!(matches!(
        d.execution.inputs.get(&key("setting")),
        Some(LiveInput::Pending)
    ));
    // Publish the earlier setting notices explicitly only after testing purity.
    d.stage_notices();
    // Install a real service settlement notice but deliberately leave it
    // unstaged, as an internal pre-publication fixture. Projection must retire
    // setting's live handle on the clone, not call real stage_notices().
    let (fault, _) = Driver::<Mock>::input_admission(
        &mut d.execution.control,
        &d.execution.machines,
        &d.execution.inputs,
        &key("internal-heal"),
        &Action::Heal(HealTarget::Input(key("setting"))),
        now,
    )
    .unwrap()
    .unwrap();
    d.execution
        .control
        .invoke(&d.execution.machines, |service, _| {
            service.applied(fault, 3, 3)
        })
        .unwrap();
    let before = internal(&d);
    let rejected = prepare(
        &mut d,
        "after-notice",
        Action::Heal(HealTarget::Input(key("setting"))),
    );
    assert!(matches!(
        rejected.outcome(),
        PreparedOutcome::Rejected(InputRejection {
            reason: RejectionReason::InvalidState,
            ..
        })
    ));
    assert_eq!(internal(&d), before);
    assert!(matches!(
        d.execution.inputs.get(&key("setting")),
        Some(LiveInput::Handle(_))
    ));
    assert!(matches!(
        d.with_barrier(|b| b.commit_input(&q)),
        Err(Error::PreparedInputMismatch)
    ));
    assert_eq!(internal(&d), before);
}

#[test]
fn stale_position_id_and_boot_state_are_detected_before_any_real_mutation() {
    let mut d = ready();
    let p = prepare(&mut d, "old", throttle());
    d.step(at(4)).unwrap();
    no_change(&mut d, |d| {
        assert!(matches!(
            d.with_barrier(|b| b.commit_input(&p)),
            Err(Error::PreparedInputMismatch)
        ))
    });

    let mut d = ready();
    let p = prepare(&mut d, "old", throttle());
    // Control-only internal fixture: advance IDs and leave an unstaged
    // admission notice, without publishing a boundary. This is intentionally
    // not claimed a public returned/capturable service state.
    let now = d.now();
    Driver::<Mock>::input_admission(
        &mut d.execution.control,
        &d.execution.machines,
        &d.execution.inputs,
        &key("internal"),
        &Action::Pause(Target::Machine(MachineId(2))),
        now,
    )
    .unwrap()
    .unwrap();
    let before = internal(&d);
    assert_eq!(d.position(), p.position());
    assert!(matches!(
        d.with_barrier(|b| b.commit_input(&p)),
        Err(Error::PreparedInputMismatch)
    ));
    assert_eq!(internal(&d), before);

    let mut d = ready();
    let p = prepare(&mut d, "old", pause());
    // Read-only preview uses the actual committed lifecycle, not position alone.
    d.execution.machines[1]
        .lifecycle
        .admission(0, 3, 3, crate::oracle::observation::Transition::Crash)
        .unwrap();
    d.execution.machines[1]
        .lifecycle
        .admission(
            0,
            3,
            3,
            crate::oracle::observation::Transition::Reboot { new_boot: 1 },
        )
        .unwrap();
    let before = internal(&d);
    assert!(matches!(
        d.with_barrier(|b| b.commit_input(&p)),
        Err(Error::PreparedInputMismatch)
    ));
    assert_eq!(internal(&d), before);
}

#[test]
fn prepare_and_commit_refuse_due_work_but_apply_due_now_drains_it() {
    let mut d = ready();
    let p = prepare(&mut d, "live", pause());
    d.schedule(at(3), key("authored"), throttle()).unwrap();
    no_change(&mut d, |d| {
        assert!(matches!(
            d.with_barrier(|b| b.prepare_input(key("live"), pause())),
            Err(Error::DueWorkPending)
        ));
        assert!(matches!(
            d.with_barrier(|b| b.commit_input(&p)),
            Err(Error::DueWorkPending)
        ));
    });
    assert_eq!(due(&mut d), DueOutcome::Applied { at: at(3) });
    assert_eq!(
        d.execution.machines[1]
            .lifecycle
            .execution()
            .factor_permille(),
        2000
    );
    assert!(d
        .with_barrier(|b| b.prepare_input(key("live"), pause()))
        .is_ok());
}

#[test]
fn due_detection_includes_frames_and_timers_and_drains_timer_generated_calls() {
    // Explicit internal pre-dispatch states: these are not evidence that normal
    // step leaves due work behind. The same router/timer owners decide refusal.
    let mut d = build_network(
        (0..3).map(|i| spec(i, true, vec![])).collect(),
        vec![],
        true,
        true,
    );
    let Response::TransmitFrame { bytes: packet } = frame(mac(1), 7) else {
        unreachable!()
    };
    d.execution.network.route(MachineId(0), 0, &packet).unwrap();
    let arrival = d.execution.network.next_arrival().unwrap();
    d.execution.clock.set(arrival);
    let before = internal(&d);
    assert!(matches!(
        d.with_barrier(|b| b.prepare_input(key("live"), pause())),
        Err(Error::DueWorkPending)
    ));
    assert_eq!(internal(&d), before);
    assert_eq!(due(&mut d), DueOutcome::Applied { at: at(arrival) });
    assert!(d.execution.network.next_arrival().is_none());
    assert!(states(&d)[1]
        .received
        .iter()
        .any(|i| matches!(i.payload, Request::ReceiveFrame { .. })));

    let mut d = build(
        vec![
            spec(0, true, vec![]),
            spec(
                1,
                false,
                vec![(
                    5,
                    call(1, r#""op":"pause","target":{"machine":"fixture-2"}"#),
                )],
            ),
            spec(2, true, vec![]),
        ],
        vec![],
        true,
    );
    d.execution.clock.set(5);
    d.execution.machines[1].stopped = StopReason::Blocked {
        reached: at(0),
        deadline: Some(at(5)),
    };
    d.execution.machines[1].timer = Some(at(5));
    let before = internal(&d);
    assert!(matches!(
        d.with_barrier(|b| b.prepare_input(key("live"), pause())),
        Err(Error::DueWorkPending)
    ));
    assert_eq!(internal(&d), before);
    assert_eq!(due(&mut d), DueOutcome::Applied { at: at(5) });
    assert!(
        d.execution.machines[2].lifecycle.paused(),
        "timer-generated same-time call was not drained"
    );
    assert!(!d.execution.events.contains_key(&at(5)));
    assert!(
        states(&d).iter().all(|s| s.entropy.is_empty()),
        "zero-time drain executed positive budget"
    );
}

#[test]
fn schedule_shape_errors_match_prepare_without_mutation_or_journaling() {
    let bad = [
        pause(), // live key case below
        Action::Pause(Target::Machine(MachineId(100))),
        Action::Pause(Target::Name("unknown".into())),
        Action::Spawn {
            name: "unknown".into(),
        },
        Action::Stop {
            target: Target::Machine(MachineId(1)),
            deadline: Duration::ZERO,
        },
        Action::Stop {
            target: Target::Machine(MachineId(1)),
            deadline: Duration::MAX,
        },
        Action::StepRealtime {
            target: Target::Machine(MachineId(1)),
            nanoseconds: std::num::NonZeroI64::new(i64::MIN).unwrap(),
        },
        Action::Partition {
            left: vec![Target::Machine(MachineId(100))],
            right: vec![Target::Machine(MachineId(1))],
        },
    ];
    for (index, action) in bad.into_iter().enumerate() {
        let mut d = ready();
        if index == 0 {
            d.schedule(at(20), key("bad"), throttle()).unwrap();
        }
        let before = internal(&d);
        let projected = d
            .with_barrier(|b| b.prepare_input(key("bad"), action.clone()))
            .unwrap_err();
        let scheduled = d.schedule(d.now(), key("bad"), action).unwrap_err();
        assert_eq!(projected.to_string(), scheduled.to_string());
        assert_eq!(internal(&d), before);
        assert!(!d.execution.failed);
    }
}

#[test]
fn stopped_finished_and_poisoned_preflights_never_mutate() {
    for kind in 0..3 {
        let mut d = ready();
        let p = prepare(&mut d, "live", pause());
        if kind == 0 {
            d.with_barrier(|b| {
                b.stop();
                Ok(())
            })
            .unwrap();
        }
        if kind == 1 {
            let p = prepare(&mut d, "end", Action::Crash(Target::Machine(MachineId(0))));
            assert!(matches!(
                commit(&mut d, &p),
                DueOutcome::Finished {
                    reason: ExitReason::Crashed,
                    ..
                }
            ));
        }
        if kind == 2 {
            d.execution.machines[0]
                .machine
                .as_mut()
                .unwrap()
                .state
                .fail_run = true;
            assert!(d.step(at(4)).is_err());
        }
        let before = internal(&d);
        let encoded = (kind != 2).then(|| bytes(&mut d));
        for operation in 0..3 {
            let err = match operation {
                0 => d
                    .with_barrier(|b| b.prepare_input(key("other"), pause()).map(|_| ()))
                    .unwrap_err(),
                1 => d
                    .with_barrier(|b| b.commit_input(&p).map(|_| ()))
                    .unwrap_err(),
                _ => d
                    .with_barrier(|b| b.apply_due_now().map(|_| ()))
                    .unwrap_err(),
            };
            assert!(matches!(
                (kind, err),
                (0, Error::Stopped) | (1, Error::Completed) | (2, Error::Failed)
            ));
            assert_eq!(internal(&d), before);
            if let Some(encoded) = &encoded {
                assert_eq!(&bytes(&mut d), encoded);
            }
        }
    }
}

#[test]
fn sequential_same_time_entries_apply_but_prequeued_fifo_still_rejects_reboot() {
    for batch in [false, true] {
        let mut d = ready();
        let crash = Action::Crash(Target::Machine(MachineId(1)));
        let reboot = Action::Reboot(Target::Machine(MachineId(1)));
        d.schedule(at(3), key("crash"), crash).unwrap();
        if !batch {
            assert!(matches!(due(&mut d), DueOutcome::Applied { .. }));
        }
        d.schedule(at(3), key("reboot"), reboot).unwrap();
        assert!(matches!(due(&mut d), DueOutcome::Applied { .. }));
        let events = d.drain_events();
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::InputRejected { key: k, .. } if k == &key("reboot"))).count(), usize::from(batch));
        assert_eq!(
            d.execution.machines[1].lifecycle.boot(),
            if batch { 0 } else { 1 }
        );
        assert_eq!(states(&d)[1].dead, batch);
    }
}

#[test]
fn start_paused_authored_hold_and_live_replay_have_identical_events_workers_one_four() {
    for workers in [1, 4] {
        let mut source = build(
            (0..3).map(|i| spec(i, true, vec![])).collect(),
            vec![],
            true,
        );
        let c = source.checkpoint().unwrap();
        let mut paused = restore(&c, workers);
        let mut replay = restore(&c, workers);
        paused
            .with_barrier(|b| b.set_rate(PlaybackRate::Paused))
            .unwrap();
        for d in [&mut paused, &mut replay] {
            d.schedule(at(0), key("authored"), throttle()).unwrap();
        }
        assert!(matches!(due(&mut paused), DueOutcome::BeforeFault { .. }));
        assert!(matches!(
            paused.with_barrier(|b| b.prepare_input(key("live"), pause())),
            Err(Error::BeforeFault)
        ));
        paused
            .with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        paused.step(at(0)).unwrap();
        due(&mut replay);
        assert_eq!(paused.position(), replay.position());
        paused
            .with_barrier(|b| b.set_rate(PlaybackRate::Paused))
            .unwrap();
        let p = prepare(&mut paused, "live", pause());
        let q = prepare(&mut replay, "live", pause());
        assert_eq!(p, q);
        assert!(matches!(
            commit(&mut paused, &p),
            DueOutcome::BeforeFault { .. }
        ));
        assert!(matches!(
            commit(&mut replay, &q),
            DueOutcome::Applied { .. }
        ));
        paused
            .with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        paused.step(at(0)).unwrap();
        assert_eq!(advance(&mut paused, 30), advance(&mut replay, 30));
        assert_eq!(states(&paused), states(&replay));
        assert_eq!(paused.position(), replay.position());
    }
}

#[test]
fn unarmed_paused_hold_roundtrips_codec_and_releases_once_workers_one_four() {
    let mut d = ready();
    d.with_barrier(|b| b.set_rate(PlaybackRate::Paused))
        .unwrap();
    let p = prepare(&mut d, "held", throttle());
    assert!(matches!(commit(&mut d, &p), DueOutcome::BeforeFault { .. }));
    let (encoded, store) = bytes(&mut d);
    reference_observer_session(&mut d);
    d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    let expected = advance(&mut d, 30);
    let expected_states = states(&d);
    drop(d);
    let c = decode(&encoded, &store);
    for workers in [1, 4] {
        let mut d = restore(&c, workers);
        assert_eq!(d.playback.held.as_ref(), Some(record(&p)));
        assert!(!d.playback.before_next);
        assert!(matches!(
            d.step(at(100)).unwrap(),
            StepOutcome::BeforeFault { .. }
        ));
        d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        assert_eq!(advance(&mut d, 30), expected);
        assert_eq!(states(&d), expected_states);
    }
}

#[test]
fn immediate_unavailable_admission_has_exact_record_even_without_pending_effect() {
    let mut d = ready();
    let p = prepare(
        &mut d,
        "crash",
        Action::Crash(Target::Machine(MachineId(1))),
    );
    commit(&mut d, &p);
    let before = internal(&d);
    let p = prepare(
        &mut d,
        "process",
        Action::Stop {
            target: Target::Machine(MachineId(1)),
            deadline: Duration::from_nanos(20),
        },
    );
    assert_eq!(internal(&d), before);
    assert_eq!(record(&p).fault.get(), 2);
    assert_eq!(
        record(&p).target,
        FaultTarget::Machine {
            machine: MachineId(1),
            boot: 0
        }
    );
    assert!(matches!(commit(&mut d, &p), DueOutcome::Applied { .. }));
    assert!(d
        .drain_observations()
        .events
        .iter()
        .any(|e| matches!(&e.kind,
        EventKind::FaultSettled { record: actual, status: SettlementStatus::Unavailable, .. }
        if actual == record(&p))));
}

#[test]
fn due_completion_keeps_pending_step_and_anchor_until_ordinary_step_observes_finish() {
    let mut d = ready();
    d.with_barrier(|b| {
        b.set_rate(PlaybackRate::Paused)?;
        b.step_quantum()
    })
    .unwrap();
    let playback = d.playback.clone();
    let position = d.position();
    let end = d.execution.quantum_end;
    let p = prepare(
        &mut d,
        "finish",
        Action::Crash(Target::Machine(MachineId(0))),
    );
    assert_eq!(
        commit(&mut d, &p),
        DueOutcome::Finished {
            at: position.at,
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(d.playback, playback);
    assert_eq!(d.now(), position.at);
    assert_eq!(d.execution.quantum_end, end);
    assert_eq!(d.execution.completed_quanta, position.completed_quanta);
    assert!(matches!(
        d.step(at(100)).unwrap(),
        StepOutcome::Finished { .. }
    ));
    assert!(!status(&mut d).stepping);
}

struct Spy {
    mock: Mock,
    calls: BudgetCalls,
}
impl Machine for Spy {
    type Context = Context;
    type Error = Io;
    type Snapshot = Saved;
    fn run(&mut self, c: &mut RunContext<'_, Context>, upto: Timestamp) -> Result<StopReason, Io> {
        self.calls
            .lock()
            .unwrap()
            .push((c.clock.now(), upto.as_nanos()));
        self.mock.run(c, upto)
    }
    fn snapshot(&mut self) -> Result<Saved, Io> {
        self.mock.snapshot()
    }
}
fn spy() -> (Driver<Spy>, BudgetCalls) {
    let mut d = ready();
    let c = d.checkpoint().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let restored = calls.clone();
    let d = Driver::<Spy>::from_checkpoint(
        &c,
        4,
        || Ok(Context(None)),
        move |s| {
            Ok(Spy {
                mock: Mock::restore(s)?,
                calls: restored.clone(),
            })
        },
        |_| panic!("unexpected factory during admission"),
    )
    .unwrap();
    (d, calls)
}

#[test]
fn applied_is_dispatch_not_future_ack_settlement_and_only_zero_budgets_run() {
    let (mut d, calls) = spy();
    let before = d.playback.clone();
    let position = d.position();
    let p = d
        .with_barrier(|b| {
            b.prepare_input(
                key("process"),
                Action::PauseProcess {
                    target: Target::Machine(MachineId(1)),
                    deadline: Duration::from_nanos(20),
                },
            )
        })
        .unwrap();
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        d.with_barrier(|b| b.commit_input(&p)).unwrap(),
        DueOutcome::Applied { at: at(3) }
    );
    assert_eq!(*calls.lock().unwrap(), vec![(3, 3)]);
    assert_eq!(d.now(), position.at);
    assert_eq!(d.playback, before);
    let events = d.drain_events();
    assert!(events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::FaultAdmitted(f) if f == record(&p))));
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::FaultSettled { .. })));
    d.step(at(6)).unwrap();
    assert!(d.drain_events().iter().any(|e| matches!(&e.kind, EventKind::FaultSettled { record: fault, status: SettlementStatus::Applied, .. } if fault == record(&p))));
}

#[test]
fn zero_budget_machine_error_poison_preserves_confirmed_facts_and_never_retries() {
    for via_due in [false, true] {
        let (mut d, calls) = spy();
        let p = d
            .with_barrier(|b| {
                b.prepare_input(key("crash"), Action::Crash(Target::Machine(MachineId(1))))
            })
            .unwrap();
        d.execution.machines[1]
            .machine
            .as_mut()
            .unwrap()
            .mock
            .state
            .fail_run = true;
        let result = if via_due {
            d.schedule(
                at(3),
                key("crash"),
                Action::Crash(Target::Machine(MachineId(1))),
            )
            .unwrap();
            d.with_barrier(|b| b.apply_due_now())
        } else {
            d.with_barrier(|b| b.commit_input(&p))
        };
        assert!(matches!(
            result,
            Err(Error::Machine {
                id: MachineId(1),
                ..
            })
        ));
        assert_eq!(*calls.lock().unwrap(), vec![(3, 3)]);
        assert!(d.execution.failed);
        assert!(d.execution.machines.iter().all(|e| e.staged.is_empty()));
        let events = d.drain_events();
        assert!(events
            .iter()
            .any(|e| matches!(&e.kind, EventKind::FaultAdmitted(f) if f == record(&p))));
        assert!(events.iter().any(|e| matches!(&e.kind, EventKind::FaultSettled { record: fault, status: SettlementStatus::Failed, .. } if fault == record(&p))));
        assert!(matches!(d.step(at(20)), Err(Error::Failed)));
        assert!(matches!(d.checkpoint(), Err(Error::Failed)));
        assert!(matches!(
            d.with_barrier(|b| b.apply_due_now()),
            Err(Error::Failed)
        ));
        assert_eq!(*calls.lock().unwrap(), vec![(3, 3)]);
    }
}
