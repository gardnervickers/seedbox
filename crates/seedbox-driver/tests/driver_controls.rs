#[path = "support/driver_fixture.rs"]
mod driver_fixture;
use driver_fixture::*;

fn held_record(outcome: &StepOutcome) -> Option<seedbox_driver::driver::FaultRecord> {
    match outcome {
        StepOutcome::BeforeFault { fault, .. } => Some(fault.clone()),
        _ => None,
    }
}

#[test]
fn initial_pause_holds_current_time_actions_and_dynamic_resume() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.set_rate(PlaybackRate::Paused)
        })
        .unwrap();
    let initial = status(&mut driver);
    for _ in 0..3 {
        assert!(
            matches!(driver.step(at(100)).unwrap(), StepOutcome::Waiting { at: time, .. } if time == at(0))
        );
        assert_eq!(status(&mut driver), initial);
    }
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            assert!(barrier.machine(id)?.log.lock().unwrap().is_empty());
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(0),
            reason: ExitReason::Crashed
        }
    );
    // The crash input's admission and its effect each commit a boundary.
    assert_eq!(status(&mut driver).position.boundary_index, 2);
}

#[test]
fn one_quantum_survives_short_barriers_and_zero_time_actions() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            assert!(matches!(barrier.step_quantum(), Err(Error::NotPaused)));
            assert!(matches!(
                barrier.pause_before_next_fault(PlaybackRate::Paused),
                Err(Error::InvalidRate)
            ));
            // Zero-time inputs whose effects leave execution unchanged.
            barrier.schedule(
                at(3),
                key("nominal"),
                Action::Throttle {
                    target: Target::Machine(id),
                    config: Default::default(),
                },
            )?;
            barrier.schedule(
                at(10),
                auto_key(),
                Action::Heal(seedbox_driver::driver::HealTarget::Input(key("nominal"))),
            )?;
            barrier.set_rate(PlaybackRate::Paused)?;
            barrier.step_quantum()?;
            assert!(matches!(barrier.step_quantum(), Err(Error::StepPending)));
            Ok(())
        })
        .unwrap();
    for end in [0, 2, 3, 7] {
        assert_eq!(
            driver.step(at(end)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        assert!(status(&mut driver).stepping);
        assert_eq!(status(&mut driver).position.completed_quanta, 0);
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    let completed = status(&mut driver);
    assert_eq!(completed.position.completed_quanta, 1);
    assert!(!completed.stepping);
    assert!(
        matches!(driver.step(at(100)).unwrap(), StepOutcome::Waiting { at: time, .. } if time == at(10))
    );
    assert_eq!(status(&mut driver), completed);
    driver
        .with_barrier(|barrier| barrier.step_quantum())
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(20) }
    );
    assert_eq!(status(&mut driver).position.completed_quanta, 2);
}

#[test]
fn pre_crash_hold_retains_exact_current_time_cursor_and_allows_only_playback() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    let held = driver.step(at(100)).unwrap();
    assert_eq!(
        driver_fixture::held(&held),
        Some((at(0), FaultAction::Crash, id))
    );
    let position = status(&mut driver);
    // Both admissions committed; the first Crash effect did not.
    assert_eq!(position.position.boundary_index, 2);
    assert_eq!(position.before_fault, held_record(&held));
    assert_eq!(position.position.completed_quanta, 0);
    for _ in 0..3 {
        assert_eq!(driver.step(at(100)).unwrap(), held);
        assert_eq!(status(&mut driver), position);
    }
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            assert!(barrier.machine(id)?.log.lock().unwrap().is_empty());
            assert!(matches!(barrier.snapshot(id), Err(Error::BeforeFault)));
            assert!(matches!(
                barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id))),
                Err(Error::BeforeFault)
            ));
            assert!(matches!(
                barrier.add(|| panic!("held factory ran")),
                Err(Error::BeforeFault)
            ));
            barrier.step_quantum()
        })
        .unwrap();
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::Finished {
            at: at(0),
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(status(&mut driver).position.boundary_index, 3); // Oracle crash ends admission before later occurrences.
    assert!(!status(&mut driver).stepping);
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(0),
            reason: ExitReason::Crashed
        }
    );
    driver
        .with_barrier(|barrier| {
            assert!(matches!(barrier.step_quantum(), Err(Error::Completed)));
            assert!(barrier.machine(id)?.log.lock().unwrap().is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn pre_crash_hold_precedes_same_time_timer_and_counts_quantum_once() {
    let mut driver = controlled(2, &[&[Op::Work(1), Op::Sleep(9), Op::Fail]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(10), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        held(&driver.step(at(100)).unwrap()),
        Some((at(10), FaultAction::Crash, id))
    );
    let held = status(&mut driver);
    assert_eq!(held.position.completed_quanta, 1);
    driver.with_barrier(|barrier| {
        assert!(matches!(barrier.stopped(id)?, StopReason::Blocked { deadline: Some(deadline), .. } if deadline == at(10)));
        assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
        barrier.set_rate(PlaybackRate::Unlimited)
    }).unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(10),
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(status(&mut driver).position.completed_quanta, 1);
}

#[test]
fn explicit_rate_and_step_controls_cancel_a_pre_crash_arm() {
    for step in [false, true] {
        let mut driver = controlled(1, &[&[Op::Work(100)]]);
        let id = ids(&mut driver)[0];
        driver
            .with_barrier(|barrier| {
                barrier.schedule(at(3), auto_key(), Action::Crash(Target::Machine(id)))?;
                barrier.pause_before_next_fault(PlaybackRate::Unlimited)?;
                if step {
                    barrier.set_rate(PlaybackRate::Paused)?;
                    // Rearm while at the same position, then make an explicit pause
                    // and step request; neither request changes the simulation.
                    barrier.pause_before_next_fault(PlaybackRate::Unlimited)?;
                    barrier.set_rate(PlaybackRate::Paused)?;
                    barrier.step_quantum()
                } else {
                    barrier.set_rate(PlaybackRate::Unlimited)
                }
            })
            .unwrap();
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Finished {
                at: at(3),
                reason: ExitReason::Crashed
            }
        );
    }
}

#[test]
fn blocked_single_quantum_reports_blocked_without_polling_and_keeps_admission() {
    let mut driver = controlled(1, &[&[Op::Park, Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.set_rate(PlaybackRate::Paused)?;
            barrier.step_quantum()
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    let blocked = status(&mut driver);
    assert!(blocked.stepping);
    for _ in 0..2 {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Blocked { at: at(0) }
        );
        assert_eq!(status(&mut driver), blocked);
    }
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
            barrier.machine(id)?.input.store(true, Ordering::Relaxed);
            barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id)))
        })
        .unwrap();
    // Pause applies without a run and keeps the quantum admission pending.
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    assert!(status(&mut driver).stepping);
    // Resume's zero-budget run delivers the stimulus (a separate barrier:
    // a same-time Resume would be admitted before the Pause effect applies).
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    assert!(!status(&mut driver).stepping);
}

#[test]
fn permanent_stop_prevents_action_work_factory_and_resume() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(id)))?;
            barrier.set_rate(PlaybackRate::Paused)?;
            barrier.step_quantum()?;
            barrier.stop();
            barrier.stop();
            assert!(matches!(
                barrier.set_rate(PlaybackRate::Unlimited),
                Err(Error::Stopped)
            ));
            assert!(matches!(barrier.step_quantum(), Err(Error::Stopped)));
            assert!(matches!(
                barrier.pause_before_next_fault(PlaybackRate::Unlimited),
                Err(Error::Stopped)
            ));
            assert!(matches!(
                barrier.add(|| panic!("stopped factory ran")),
                Err(Error::Stopped)
            ));
            assert!(matches!(
                barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id))),
                Err(Error::Stopped)
            ));
            assert!(matches!(barrier.snapshot(id), Err(Error::Stopped)));
            Ok(())
        })
        .unwrap();
    let stopped = status(&mut driver);
    assert!(stopped.stopped);
    assert!(!stopped.stepping);
    for _ in 0..3 {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Stopped { at: at(0) }
        );
        assert_eq!(status(&mut driver), stopped);
    }
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            assert!(barrier.machine(id)?.log.lock().unwrap().is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn controls_preserve_worker_independent_results_positions_and_action_order() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = controlled(
            workers,
            &[
                &[Op::Work(1), Op::Sleep(5), Op::Work(30), Op::Exit],
                &[Op::Sleep(10), Op::Fail],
            ],
        );
        let id = ids(&mut driver)[1];
        driver
            .with_barrier(|barrier| {
                barrier.schedule(at(10), key("pause"), Action::Pause(Target::Machine(id)))?;
                barrier.schedule(at(10), key("crash"), Action::Crash(Target::Machine(id)))?;
                barrier.pause_before_next_fault(PlaybackRate::Unlimited)
            })
            .unwrap();
        let held = driver.step(at(100)).unwrap();
        assert!(matches!(held, StepOutcome::BeforeFault { .. }));
        let held_status = status(&mut driver);
        driver
            .with_barrier(|barrier| barrier.step_quantum())
            .unwrap();
        let resumed = driver.step(at(100)).unwrap();
        let paused = status(&mut driver);
        assert!(!paused.stepping);
        driver
            .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        let mut outcomes = vec![held, resumed];
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome.clone());
            if matches!(outcome, StepOutcome::Finished { .. }) {
                break;
            }
            assert!(outcomes.len() < 20);
        }
        let logs = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(_, guest)| guest.log.lock().unwrap().clone())
                    .collect::<Vec<_>>())
            })
            .unwrap();
        let observed = (held_status, paused, status(&mut driver), outcomes, logs);
        if let Some(reference) = &reference {
            assert_eq!(&observed, reference);
        } else {
            reference = Some(observed);
        }
    }
}

#[test]
fn rearming_held_crash_resumes_only_that_occurrence_and_skips_noop_crashes() {
    let mut driver = controlled_targets(2, &[&[Op::Work(100)], &[Op::Work(100)]]);
    let ids = ids(&mut driver);
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(ids[0])))?;
            // Admitted, but stale (rejected) at its turn after the first crash.
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(ids[0])))?;
            barrier.schedule(at(0), auto_key(), Action::Crash(Target::Machine(ids[1])))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        held(&driver.step(at(100)).unwrap()),
        Some((at(0), FaultAction::Crash, ids[0]))
    );
    let first = status(&mut driver).position;
    driver
        .with_barrier(|barrier| barrier.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    assert_eq!(
        held(&driver.step(at(100)).unwrap()),
        Some((at(0), FaultAction::Crash, ids[1]))
    );
    let second = status(&mut driver).position;
    assert_eq!(second.at, first.at);
    assert_eq!(second.completed_quanta, first.completed_quanta);
    // Only the applied crash commits; the stale one settles Rejected.
    assert_eq!(second.boundary_index, first.boundary_index + 1);
    driver
        .with_barrier(|barrier| {
            assert!(barrier.machine(ids[0])?.crashed);
            assert!(!barrier.machine(ids[1])?.crashed);
            assert!(barrier.machine(ids[1])?.log.lock().unwrap().is_empty());
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
}

#[test]
fn dynamic_pause_cancels_an_inflight_quantum_and_stop_holds_existing_progress() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.set_rate(PlaybackRate::Paused)?;
            barrier.step_quantum()
        })
        .unwrap();
    assert_eq!(
        driver.step(at(3)).unwrap(),
        StepOutcome::BarrierReached { at: at(3) }
    );
    assert!(status(&mut driver).stepping);
    driver
        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Paused))
        .unwrap();
    assert!(!status(&mut driver).stepping);
    assert!(
        matches!(driver.step(at(100)).unwrap(), StepOutcome::Waiting { at: reached, .. } if reached == at(3))
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.reached, 3);
            barrier.stop();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Stopped { at: at(3) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.reached, 3);
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
}
