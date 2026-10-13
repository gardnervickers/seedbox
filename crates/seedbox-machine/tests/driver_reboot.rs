mod driver_fixture;
use driver_fixture::*;

#[test]
fn exited_target_can_reboot_while_separate_oracle_remains_alive() {
    let mut driver = controlled_targets(1, &[&[Op::Work(2), Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(2) }
    );
    driver
        .with_barrier(|barrier| barrier.schedule(at(2), Action::Reboot(id)))
        .unwrap();
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

#[test]
fn future_reboot_and_all_later_actions_survive_exit_with_the_same_identity() {
    let mut driver = controlled_targets(1, &[&[Op::Work(2), Op::Exit]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            for (time, action) in [
                (5, Action::Reboot(id)),
                (5, Action::Pause(id)),
                (7, Action::Wake(id)),
                (9, Action::Resume(id)),
                (10, Action::Crash(id)),
            ] {
                barrier.schedule(at(time), action)?;
            }
            Ok(())
        })
        .unwrap();
    for end in [5, 7, 9] {
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
                assert_eq!(barrier.paused(id)?, end < 9);
                let guest = barrier.machine(id)?;
                assert_eq!(guest.executed, 2);
                if end < 9 {
                    assert!(!guest.exited);
                    assert_eq!(guest.pending_boots, 0);
                    assert_eq!(guest.pending_at, None);
                } else {
                    assert!(!guest.exited);
                    assert_eq!(guest.reached, 9);
                    assert_eq!(guest.pending_boots, 0);
                    assert_eq!(guest.boot_times, [0, 5]);
                }
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
            assert_eq!(guest.executed, 3);
            assert_eq!(guest.boot_times, [0, 5]);
            assert_eq!(
                *guest.log.lock().unwrap(),
                [(0, 0, 5), (0, 5, 5), (0, 9, 9), (0, 9, 10)]
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn same_time_crash_reboot_and_pause_order_controls_the_fresh_boot() {
    for pause_last in [false, true] {
        let mut driver = controlled_targets(2, &[&[Op::Work(100)]]);
        let id = ids(&mut driver)[0];
        driver
            .with_barrier(|barrier| {
                let actions = if pause_last {
                    vec![Action::Crash(id), Action::Reboot(id), Action::Pause(id)]
                } else {
                    vec![
                        Action::Pause(id),
                        Action::Reboot(id),
                        Action::Crash(id),
                        Action::Reboot(id),
                    ]
                };
                for action in actions {
                    barrier.schedule(at(0), action)?;
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(
            driver.step(at(0)).unwrap(),
            if pause_last {
                StepOutcome::Blocked { at: at(0) }
            } else {
                StepOutcome::BarrierReached { at: at(0) }
            }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.paused(id)?, pause_last);
                assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
                assert!(!barrier.machine(id)?.crashed);
                assert_eq!(barrier.machine(id)?.executed, 0);
                assert_eq!(barrier.machine(id)?.pending_boots, 0);
                assert_eq!(
                    barrier.machine(id)?.log.lock().unwrap().len(),
                    if pause_last { 1 } else { 2 }
                );
                let expected = if pause_last {
                    vec!["crash", "reboot"]
                } else {
                    vec!["reboot", "crash", "reboot"]
                };
                assert_eq!(barrier.machine(id)?.lifecycle, expected);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn reboot_discards_due_and_overdue_old_timers_and_clears_pause() {
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
                    barrier.schedule(at(1), Action::Pause(id))?;
                }
                barrier.schedule(at(reboot_at), Action::Reboot(id))
            })
            .unwrap();
        assert_eq!(
            driver.step(at(100)).unwrap(),
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
fn reboot_retains_quantum_anchor_and_does_not_consume_pre_crash_arm() {
    let mut driver = controlled_targets(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(3), Action::Reboot(id))?;
            barrier.schedule(at(10), Action::Reboot(id))?;
            barrier.schedule(at(12), Action::Crash(id))?;
            barrier.pause_before_next_crash(PlaybackRate::Unlimited)
        })
        .unwrap();
    for end in [3, 10] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        driver
            .with_barrier(|barrier| {
                assert!(barrier.playback().before_next_crash);
                assert_eq!(
                    barrier.playback().position.completed_quanta,
                    u64::from(end == 10)
                );
                assert_eq!(barrier.machine(id)?.reached, end);
                assert_eq!(barrier.machine(id)?.pending_boots, 0);
                assert_eq!(barrier.machine(id)?.pending_at, None);
                assert_eq!(barrier.machine(id)?.boot_times.last(), Some(&end));
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BeforeAction {
            at: at(12),
            action: Action::Crash(id)
        }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            assert_eq!(barrier.machine(id)?.lifecycle, ["reboot", "reboot"]);
            assert_eq!(barrier.machine(id)?.pending_boots, 0);
            assert_eq!(barrier.machine(id)?.pending_at, None);
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
        vec![guest, machine(1, &[Op::Work(100)])]
    });
    let ids = ids(&mut driver);
    let log = driver
        .with_barrier(|barrier| {
            barrier.schedule(at(5), Action::Reboot(ids[0]))?;
            barrier.schedule(at(5), Action::Wake(ids[1]))?;
            barrier.schedule(at(6), Action::Crash(ids[1]))?;
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
                for (time, action) in [
                    (3, Action::Pause(ids[0])),
                    (7, Action::Reboot(ids[0])),
                    (10, Action::Crash(ids[1])),
                    (10, Action::Reboot(ids[1])),
                    (10, Action::Wake(ids[1])),
                    (14, Action::Reboot(ids[0])),
                    (21, Action::Crash(ids[0])),
                    (21, Action::Crash(ids[1])),
                ] {
                    barrier.schedule(at(time), action)?;
                }
                Ok(())
            })
            .unwrap();
        let mut outcomes = Vec::new();
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome);
            if matches!(outcome, StepOutcome::Blocked { .. }) {
                break;
            }
            assert!(outcomes.len() < 10);
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
        let observed = (outcomes, observations);
        if let Some(reference) = &reference {
            assert_eq!(&observed, reference);
        } else {
            reference = Some(observed);
        }
    }
}
