use seedbox_driver::test_support::fixture::*;

/// Crash, then reboot at the next barrier at the same simulation time. A
/// Reboot input queued with the Crash would be admitted before the crash
/// effect applies and be rejected (see the same-time test below).
fn crash_then_reboot(driver: &mut Driver<TestMachine>, id: MachineId, time: u64) {
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(time), auto_key(), Action::Crash(Target::Machine(id)))
        })
        .unwrap();
    driver.step(at(time)).unwrap();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(time), auto_key(), Action::Reboot(Target::Machine(id)))
        })
        .unwrap();
}

#[test]
fn exited_target_can_crash_and_reboot_while_separate_oracle_remains_alive() {
    let mut driver = controlled_targets(1, &[&[Op::Work(2), Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(2) }
    );
    // A machine that shut down by itself is not down: Reboot needs a crash first.
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(2), key("early"), Action::Reboot(Target::Machine(id)))
        })
        .unwrap();
    driver.step(at(2)).unwrap();
    assert!(driver
        .drain_observations()
        .events
        .iter()
        .any(|event| matches!(
            &event.kind,
            EventKind::InputRejected { key: found, .. } if *found == key("early")
        )));
    crash_then_reboot(&mut driver, id, 2);
    assert_eq!(
        driver.step(at(2)).unwrap(),
        StepOutcome::BarrierReached { at: at(2) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier
                    .machines()
                    .find(|(_, guest)| guest.name == 0)
                    .unwrap()
                    .0,
                id
            );
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            assert!(!barrier.paused(id)?);
            let guest = barrier.machine(id)?;
            assert_eq!(guest.reached, 2);
            assert_eq!(guest.executed, 2);
            assert!(!guest.exited);
            assert_eq!(guest.pending_boots, 0);
            assert_eq!(guest.pending_at, None);
            assert_eq!(guest.boot_times, [0, 2]);
            // The fixture treats a crash of an exited boot as already ended.
            assert_eq!(guest.lifecycle, ["reboot"]);
            assert_eq!(guest.log.lock().unwrap().len(), 2);
            let snapshot = barrier.snapshot(id)?;
            assert_eq!(snapshot.pending_boots, 0);
            assert_eq!(snapshot.pending_at, None);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(4) }
    );
}

/// Confirmed boot state, not the origin of a crash, owns reboot eligibility.
/// A self-crashed boot is down: Reboot is admitted directly and a further
/// Crash is rejected. A boot that shut down by itself still needs a Crash.
#[test]
fn self_crashed_target_reboots_directly_but_self_shutdown_needs_a_crash() {
    let mut driver = controlled_targets(1, &[&[Op::Work(2), Op::Crash], &[Op::Work(2), Op::Exit]]);
    let [crashed, shut] = ids(&mut driver)[..] else {
        unreachable!()
    };
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(2) }
    );
    driver.drain_observations();
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(crashed)?,
                StopReason::Exited {
                    reached: at(2),
                    reason: ExitReason::Crashed
                }
            );
            barrier.schedule(
                at(2),
                key("crash-again"),
                Action::Crash(Target::Machine(crashed)),
            )?;
            barrier.schedule(
                at(2),
                key("reboot-crashed"),
                Action::Reboot(Target::Machine(crashed)),
            )?;
            barrier.schedule(
                at(2),
                key("reboot-shut"),
                Action::Reboot(Target::Machine(shut)),
            )
        })
        .unwrap();
    driver.step(at(2)).unwrap();
    let events = driver.drain_observations().events;
    let rejected = |name: &str| {
        events.iter().any(|event| {
            matches!(&event.kind,
            EventKind::InputRejected { key: found, .. } if *found == key(name))
        })
    };
    let applied = |name: &str| {
        events.iter().any(|event| {
            matches!(&event.kind,
            EventKind::FaultSettled { record, status, .. }
                if record.origin == FaultOrigin::Input { key: key(name) }
                    && *status == SettlementStatus::Applied)
        })
    };
    assert!(
        rejected("crash-again"),
        "an already crashed boot is not crashed again"
    );
    assert!(
        applied("reboot-crashed"),
        "a self-crashed boot reboots directly"
    );
    assert!(rejected("reboot-shut"), "a self-shutdown boot is not down");
    driver
        .with_barrier(|barrier| {
            let guest = barrier.machine(crashed)?;
            assert_eq!(guest.boot_times, [0, 2]);
            assert_eq!(guest.lifecycle, ["reboot"], "no synthetic crash reached it");
            assert_eq!(barrier.stopped(crashed)?, StopReason::BoundaryReached);
            assert_eq!(barrier.machine(shut)?.boot_times, [0]);
            Ok(())
        })
        .unwrap();
    // Existing policy: the self-shutdown boot reboots after an explicit Crash.
    crash_then_reboot(&mut driver, shut, 2);
    driver.step(at(2)).unwrap();
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(shut)?.boot_times, [0, 2]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn future_lifecycle_inputs_survive_exit_with_the_same_identity() {
    let mut driver = controlled_targets(1, &[&[Op::Work(2), Op::Exit]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            for (time, action) in [
                (4, Action::Crash(Target::Machine(id))),
                (5, Action::Reboot(Target::Machine(id))),
                (6, Action::Pause(Target::Machine(id))),
                (9, Action::Resume(Target::Machine(id))),
                (10, Action::Crash(Target::Machine(id))),
            ] {
                barrier.schedule(at(time), auto_key(), action)?;
            }
            Ok(())
        })
        .unwrap();
    for end in [4, 5, 6, 9] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(
                    barrier
                        .machines()
                        .find(|(_, guest)| guest.name == 0)
                        .unwrap()
                        .0,
                    id
                );
                assert_eq!(barrier.paused(id)?, end == 6);
                let guest = barrier.machine(id)?;
                assert_eq!(guest.boot_times.len(), if end < 5 { 1 } else { 2 });
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            let guest = barrier.machine(id)?;
            assert_eq!(guest.lifecycle, ["reboot", "crash"]);
            assert_eq!(guest.executed, 4);
            assert_eq!(guest.boot_times, [0, 5]);
            assert_eq!(
                *guest.log.lock().unwrap(),
                [(0, 0, 4), (0, 5, 5), (0, 5, 6), (0, 9, 9), (0, 9, 10)]
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn same_time_crash_then_reboot_inputs_apply_the_crash_and_reject_the_reboot() {
    let mut driver = controlled_targets(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver.drain_observations();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), key("crash"), Action::Crash(Target::Machine(id)))?;
            barrier.schedule(at(0), key("reboot"), Action::Reboot(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    // Generated effects join the FIFO tail, so the Reboot input is admitted
    // while the target is still up, before the Crash effect applies.
    let events = driver.drain_observations().events;
    let facts: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultAdmitted(record) => Some(format!("admitted {:?}", record.action)),
            EventKind::InputRejected { key, rejection } => {
                Some(format!("rejected {key} {:?}", rejection.reason))
            }
            EventKind::FaultSettled { record, status, .. } => {
                Some(format!("settled {:?} {status:?}", record.action))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        facts,
        [
            "admitted Crash",
            "rejected reboot InvalidState",
            "settled Crash Applied"
        ]
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.machine(id)?.crashed);
            assert_eq!(barrier.machine(id)?.lifecycle, ["crash"]);
            // The key was released by its rejection.
            barrier.schedule(at(0), key("reboot"), Action::Reboot(Target::Machine(id)))?;
            barrier.schedule(at(1), key("pause"), Action::Pause(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::BarrierReached { at: at(0) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(1) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            assert!(!barrier.machine(id)?.crashed);
            assert_eq!(barrier.machine(id)?.executed, 1);
            assert_eq!(barrier.machine(id)?.lifecycle, ["crash", "reboot"]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn crash_and_reboot_discard_due_and_overdue_old_timers_and_pause() {
    for reboot_at in [5, 10] {
        let mut driver = controlled_targets(2, &[&[Op::Sleep(5), Op::Work(100)]]);
        let id = ids(&mut driver)[0];
        assert_eq!(
            driver.step(at(1)).unwrap(),
            StepOutcome::BarrierReached { at: at(1) }
        );
        driver
            .with_barrier(|barrier| {
                if reboot_at == 10 {
                    barrier.schedule(at(1), auto_key(), Action::Pause(Target::Machine(id)))?;
                }
                barrier.schedule(
                    at(reboot_at),
                    auto_key(),
                    Action::Crash(Target::Machine(id)),
                )
            })
            .unwrap();
        driver.step(at(100)).unwrap();
        driver
            .with_barrier(|barrier| {
                assert!(!barrier.paused(id)?);
                barrier.schedule(
                    at(reboot_at),
                    auto_key(),
                    Action::Reboot(Target::Machine(id)),
                )
            })
            .unwrap();
        assert_eq!(
            driver.step(at(reboot_at)).unwrap(),
            StepOutcome::BarrierReached { at: at(reboot_at) }
        );
        driver
            .with_barrier(|barrier| {
                assert!(!barrier.paused(id)?);
                assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
                let guest = barrier.machine(id)?;
                assert_eq!(guest.reached, reboot_at);
                assert_eq!(guest.sleeping, None);
                assert_eq!(guest.executed, 0);
                assert_eq!(guest.pending_boots, 0);
                assert_eq!(guest.pending_at, None);
                assert_eq!(guest.log.lock().unwrap().len(), 2);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            driver.step(at(reboot_at + 1)).unwrap(),
            StepOutcome::BarrierReached {
                at: at(reboot_at + 1)
            }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(
                    barrier.stopped(id)?,
                    StopReason::Blocked {
                        reached: at(reboot_at),
                        deadline: Some(at(reboot_at + 5))
                    }
                );
                assert_eq!(barrier.machine(id)?.executed, 0);
                assert_eq!(barrier.machine(id)?.boot_times, [0, reboot_at]);
                assert_eq!(barrier.machine(id)?.pending_boots, 0);
                assert_eq!(barrier.machine(id)?.pending_at, None);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn reboot_retains_quantum_anchor_and_later_crash_is_held() {
    let mut driver = controlled_targets(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(2), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.schedule(at(3), auto_key(), Action::Reboot(Target::Machine(id)))?;
            barrier.schedule(at(9), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.schedule(at(10), auto_key(), Action::Reboot(Target::Machine(id)))?;
            barrier.schedule(at(12), auto_key(), Action::Crash(Target::Machine(id)))
        })
        .unwrap();
    for end in [2, 3, 9, 10] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(
                    barrier.playback().position.completed_quanta,
                    u64::from(end == 10)
                );
                assert_eq!(barrier.machine(id)?.pending_boots, 0);
                assert_eq!(barrier.machine(id)?.pending_at, None);
                if end == 3 || end == 10 {
                    assert_eq!(barrier.machine(id)?.reached, end);
                    assert_eq!(barrier.machine(id)?.boot_times.last(), Some(&end));
                }
                Ok(())
            })
            .unwrap();
    }
    driver
        .with_barrier(|barrier| barrier.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    assert_eq!(
        held(&driver.step(at(100)).unwrap()),
        Some((at(12), FaultAction::Crash, id))
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            assert_eq!(
                barrier.machine(id)?.lifecycle,
                ["crash", "reboot", "crash", "reboot"]
            );
            assert_eq!(barrier.machine(id)?.boot_times.last(), Some(&10));
            Ok(())
        })
        .unwrap();
}

#[test]
fn reboot_errors_poison_driver_before_remaining_actions_or_work() {
    let mut driver = configured(2, {
        let mut guest = machine(0, &[Op::Work(100)]);
        guest.fail_reboot = true;
        vec![
            machine(usize::MAX, &[Op::Park]),
            guest,
            machine(1, &[Op::Work(100)]),
        ]
    });
    let ids = ids(&mut driver);
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(5), auto_key(), Action::Crash(Target::Machine(ids[0])))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    let log = driver
        .with_barrier(|barrier| {
            barrier.schedule(at(5), auto_key(), Action::Reboot(Target::Machine(ids[0])))?;
            barrier.schedule(at(5), auto_key(), Action::Pause(Target::Machine(ids[1])))?;
            barrier.schedule(at(6), auto_key(), Action::Crash(Target::Machine(ids[1])))?;
            Ok(barrier.machine(ids[1])?.log.clone())
        })
        .unwrap();
    assert!(matches!(driver.step(at(100)), Err(Error::Machine { id, .. }) if id == ids[0]));
    assert_eq!(log.lock().unwrap().len(), 1);
    assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
    assert!(matches!(
        driver.with_barrier::<()>(|_| panic!("failed reboot barrier")),
        Err(Error::Failed)
    ));
}

#[test]
fn repeated_boot_lifecycle_results_are_worker_independent() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = controlled_targets(workers, &[&[Op::Work(100)], &[Op::Work(100)]]);
        let ids = ids(&mut driver);
        driver
            .with_barrier(|barrier| {
                for (index, (time, action)) in [
                    (3, Action::Pause(Target::Machine(ids[0]))),
                    (6, Action::Crash(Target::Machine(ids[0]))),
                    (7, Action::Reboot(Target::Machine(ids[0]))),
                    (10, Action::Crash(Target::Machine(ids[1]))),
                    (10, Action::Reboot(Target::Machine(ids[1]))),
                    (11, Action::Reboot(Target::Machine(ids[1]))),
                    (13, Action::Crash(Target::Machine(ids[0]))),
                    (14, Action::Reboot(Target::Machine(ids[0]))),
                    (21, Action::Crash(Target::Machine(ids[0]))),
                    (21, Action::Crash(Target::Machine(ids[1]))),
                ]
                .into_iter()
                .enumerate()
                {
                    barrier.schedule(at(time), key(&format!("input-{index}")), action)?;
                }
                Ok(())
            })
            .unwrap();
        let mut outcomes = Vec::new();
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome.clone());
            if matches!(outcome, StepOutcome::Blocked { .. }) {
                break;
            }
            assert!(outcomes.len() < 20);
        }
        let observations = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .filter(|(_, guest)| guest.name != usize::MAX)
                    .map(|(id, guest)| {
                        (
                            barrier.stopped(id).unwrap(),
                            barrier.paused(id).unwrap(),
                            guest.executed,
                            guest.lifecycle.clone(),
                            guest.log.lock().unwrap().clone(),
                        )
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap();
        assert_eq!(
            observations[0].3,
            ["crash", "reboot", "crash", "reboot", "crash"]
        );
        assert_eq!(observations[1].3, ["crash", "reboot", "crash"]);
        let observed = (outcomes, observations, driver.drain_observations().events);
        if let Some(reference) = &reference {
            assert_eq!(&observed, reference);
        } else {
            reference = Some(observed);
        }
    }
}
