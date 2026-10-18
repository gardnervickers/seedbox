#[path = "support/driver_fixture.rs"]
mod driver_fixture;
use driver_fixture::*;

#[test]
fn paused_runnable_machine_resumes_at_current_time_without_catchup_work() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = controlled(workers, &[&[Op::Work(100)], &[Op::Work(100)]]);
        let ids = ids(&mut driver);
        driver
            .with_barrier(|barrier| {
                barrier.schedule(at(3), auto_key(), Action::Pause(Target::Machine(ids[0])))?;
                barrier.schedule(at(20), auto_key(), Action::Resume(Target::Machine(ids[0])))
            })
            .unwrap();
        let mut outcomes = Vec::new();
        for end in [3, 10, 20] {
            let outcome = driver.step(at(100)).unwrap();
            assert_eq!(outcome, StepOutcome::BarrierReached { at: at(end) });
            outcomes.push(outcome);
            driver
                .with_barrier(|barrier| {
                    assert_eq!(barrier.machine(ids[0])?.executed, 3);
                    assert_eq!(barrier.machine(ids[1])?.executed, end);
                    assert_eq!(barrier.paused(ids[0])?, end < 20);
                    Ok(())
                })
                .unwrap();
        }
        driver
            .with_barrier(|barrier| {
                let guest = barrier.machine(ids[0])?;
                assert_eq!(guest.reached, 20);
                assert_eq!(*guest.log.lock().unwrap(), [(0, 0, 3), (0, 20, 20)]);
                Ok(())
            })
            .unwrap();
        outcomes.push(driver.step(at(100)).unwrap());
        let observations = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(id, guest)| {
                        (
                            barrier.paused(id).unwrap(),
                            guest.reached,
                            guest.executed,
                            guest.log.lock().unwrap().clone(),
                            barrier.stopped(id).unwrap(),
                        )
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap();
        assert_eq!(observations[0].2, 13);
        assert_eq!(observations[1].2, 30);
        let observed = (outcomes, observations);
        if let Some(reference) = &reference {
            assert_eq!(&observed, reference);
        } else {
            reference = Some(observed);
        }
    }
}

#[test]
fn pause_suppresses_timer_until_resume_delivers_it_without_cpu_execution() {
    let mut driver = controlled(2, &[&[Op::Sleep(5), Op::Work(100)], &[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(2), auto_key(), Action::Pause(Target::Machine(id)))?;
            barrier.schedule(at(20), auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    for end in [2, 10] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        driver
            .with_barrier(|barrier| {
                assert!(barrier.paused(id)?);
                assert_eq!(barrier.machine(id)?.sleeping, Some(Some(5)));
                assert_eq!(barrier.machine(id)?.reached, 0);
                assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(20) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            assert_eq!(barrier.machine(id)?.sleeping, None);
            assert_eq!(barrier.machine(id)?.reached, 20);
            assert_eq!(barrier.machine(id)?.executed, 0);
            assert_eq!(
                *barrier.machine(id)?.log.lock().unwrap(),
                [(0, 0, 2), (0, 20, 20)]
            );
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(25)).unwrap(),
        StepOutcome::BarrierReached { at: at(25) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.executed, 5);
            Ok(())
        })
        .unwrap();
}

#[test]
fn all_paused_is_unfinished_snapshot_preserves_state_and_retains_input() {
    let mut driver = controlled(1, &[&[Op::Park, Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id)))?;
            // A later input with no execution effect keeps time moving to 7.
            barrier.schedule(
                at(7),
                auto_key(),
                Action::Throttle {
                    target: Target::Machine(id),
                    config: Default::default(),
                },
            )?;
            barrier.machine(id)?.input.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(7) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            barrier.snapshot(id)?;
            let guest = barrier.machine(id)?;
            assert_eq!(guest.reached, 0);
            assert_eq!(guest.executed, 0);
            assert_eq!(guest.log.lock().unwrap().len(), 1);
            assert!(guest.input.load(Ordering::Relaxed));
            assert_eq!(
                barrier.stopped(id)?,
                StopReason::Blocked {
                    reached: at(0),
                    deadline: None
                }
            );
            barrier.schedule(at(7), auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(7)).unwrap(),
        StepOutcome::BarrierReached { at: at(7) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert!(!barrier.machine(id)?.input.load(Ordering::Relaxed));
            assert_eq!(barrier.machine(id)?.executed, 0);
            assert_eq!(barrier.machine(id)?.reached, 7);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.executed, 3);
            Ok(())
        })
        .unwrap();
}

fn rejected(events: &[seedbox_driver::driver::Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::InputRejected { key, .. } => Some(key.to_string()),
            _ => None,
        })
        .collect()
}
fn settled(events: &[seedbox_driver::driver::Event]) -> Vec<(FaultAction, SettlementStatus)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultSettled { record, status, .. } => Some((record.action, *status)),
            _ => None,
        })
        .collect()
}

#[test]
fn pause_and_resume_follow_policy_state_and_rejected_inputs_never_consume_the_arm() {
    let mut driver = controlled_targets(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver.drain_events();
    let target = || Target::Machine(id);
    driver
        .with_barrier(|barrier| {
            // All four are admitted against the state before any effect applies.
            barrier.schedule(at(0), key("resume-1"), Action::Resume(target()))?;
            barrier.schedule(at(0), key("pause-1"), Action::Pause(target()))?;
            barrier.schedule(at(0), key("pause-2"), Action::Pause(target()))?;
            barrier.schedule(at(0), key("resume-2"), Action::Resume(target()))
        })
        .unwrap();
    // Both remaining machines are paused, so nothing can make progress.
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    let events = driver.drain_events();
    assert_eq!(rejected(&events), ["resume-1", "resume-2"]);
    // The repeated pause is admitted but stale at its FIFO turn.
    assert_eq!(
        settled(&events),
        [
            (FaultAction::Pause, SettlementStatus::Applied),
            (FaultAction::Pause, SettlementStatus::Rejected)
        ]
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            assert!(barrier.machine(id)?.log.lock().unwrap().is_empty());
            barrier.schedule(at(0), key("resume-3"), Action::Resume(target()))?;
            // Rejected at admission (the target is still paused), so neither
            // this input nor its absent effect consumes the arm.
            barrier.schedule(at(0), key("pause-3"), Action::Pause(target()))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    let held = driver.step(at(100)).unwrap();
    assert_eq!(
        driver_fixture::held(&held),
        Some((at(0), FaultAction::Resume, id))
    );
    assert_eq!(rejected(&driver.drain_events()), ["pause-3"]);
    assert_eq!(driver.step(at(100)).unwrap(), held);
    driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            assert!(matches!(
                barrier.schedule(at(0), key("held"), Action::Crash(target())),
                Err(Error::BeforeFault)
            ));
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::BarrierReached { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            // A resume of a running machine is rejected at admission; the
            // arm waits for the crash effect.
            barrier.schedule(at(0), key("resume-4"), Action::Resume(target()))?;
            barrier.schedule(at(0), key("crash"), Action::Crash(target()))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver_fixture::held(&driver.step(at(100)).unwrap()),
        Some((at(0), FaultAction::Crash, id))
    );
    assert_eq!(rejected(&driver.drain_events()), ["resume-4"]);
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.machine(id)?.crashed);
            barrier.schedule(at(0), key("pause-down"), Action::Pause(target()))?;
            barrier.schedule(at(0), key("resume-down"), Action::Resume(target()))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    assert_eq!(
        rejected(&driver.drain_events()),
        ["pause-down", "resume-down"]
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            // Only the resume's zero-budget admission run executed.
            assert_eq!(*barrier.machine(id)?.log.lock().unwrap(), [(0, 0, 0)]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn pause_at_timer_boundary_withholds_due_timer_and_same_time_resume_services_once() {
    let mut driver = controlled(2, &[&[Op::Sleep(5), Op::Work(100)], &[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(5), auto_key(), Action::Pause(Target::Machine(id)))?;
            barrier.schedule(at(8), auto_key(), Action::Resume(Target::Machine(id)))?;
            barrier.schedule(at(8), auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.sleeping, Some(Some(5)));
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(8) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.sleeping, None);
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 2);
            assert_eq!(barrier.machine(id)?.executed, 0);
            Ok(())
        })
        .unwrap();
}

#[test]
fn adoption_or_paused_crash_failure_poisons_driver_before_later_actions() {
    for crash in [false, true] {
        let mut driver = configured(2, {
            let mut guest = machine(0, &[Op::Work(100)]);
            guest.fail_run_at = (!crash).then_some(5);
            guest.fail_crash = crash;
            vec![guest, machine(1, &[Op::Work(100)])]
        });
        let ids = ids(&mut driver);
        let peer_log = driver
            .with_barrier(|barrier| {
                barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(ids[0])))?;
                barrier.schedule(
                    at(5),
                    auto_key(),
                    if crash {
                        Action::Crash(Target::Machine(ids[0]))
                    } else {
                        Action::Resume(Target::Machine(ids[0]))
                    },
                )?;
                barrier.schedule(at(5), auto_key(), Action::Pause(Target::Machine(ids[1])))?;
                Ok(barrier.machine(ids[1])?.log.clone())
            })
            .unwrap();
        assert!(matches!(driver.step(at(100)), Err(Error::Machine { id, .. }) if id == ids[0]));
        assert_eq!(peer_log.lock().unwrap().len(), 1);
        assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
        assert!(matches!(
            driver.with_barrier::<()>(|_| panic!("poisoned barrier")),
            Err(Error::Failed)
        ));
    }
}
