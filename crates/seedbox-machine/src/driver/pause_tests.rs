use super::*;

fn controlled(workers: usize, programs: &[&[Op]]) -> Driver<TestMachine> {
    Driver::new(
        at(0),
        Duration::from_nanos(10),
        workers,
        || Ok(()),
        |clock| {
            Ok(programs
                .iter()
                .enumerate()
                .map(|(id, ops)| machine(clock.clone(), id, ops))
                .collect())
        },
    )
    .unwrap()
}

#[test]
fn paused_runnable_machine_resumes_at_current_time_without_catchup_work() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = controlled(workers, &[&[Op::Work(100)], &[Op::Work(100)]]);
        let ids = ids(&mut driver);
        driver
            .with_barrier(|barrier| {
                barrier.schedule(at(3), Action::Pause(ids[0]))?;
                barrier.schedule(at(20), Action::Resume(ids[0]))
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
            barrier.schedule(at(2), Action::Pause(id))?;
            barrier.schedule(at(20), Action::Resume(id))
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
fn all_paused_is_unfinished_snapshot_preserves_state_and_wake_retains_input() {
    let mut driver = controlled(1, &[&[Op::Park, Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), Action::Pause(id))?;
            barrier.schedule(at(7), Action::Wake(id))?;
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
            barrier.schedule(at(7), Action::Resume(id))
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

#[test]
fn pause_and_resume_are_ordered_idempotent_and_never_trigger_pre_crash_arm() {
    let mut driver = controlled(2, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            for action in [
                Action::Resume(id),
                Action::Pause(id),
                Action::Pause(id),
                Action::Wake(id),
                Action::Resume(id),
                Action::Resume(id),
                Action::Pause(id),
                Action::Crash(id),
            ] {
                barrier.schedule(at(0), action)?;
            }
            barrier.pause_before_next_crash(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BeforeAction {
            at: at(0),
            action: Action::Crash(id)
        }
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            assert_eq!(*barrier.machine(id)?.log.lock().unwrap(), [(0, 0, 0)]);
            assert_eq!(barrier.playback().position.boundary_index, 7);
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert!(barrier.machine(id)?.crashed);
            for action in [Action::Pause(id), Action::Resume(id), Action::Wake(id)] {
                barrier.schedule(at(0), action)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
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
            barrier.schedule(at(5), Action::Pause(id))?;
            barrier.schedule(at(8), Action::Resume(id))?;
            barrier.schedule(at(8), Action::Resume(id))
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
        let mut driver = Driver::new(
            at(0),
            Duration::from_nanos(10),
            2,
            || Ok(()),
            |clock| {
                let mut guest = machine(clock.clone(), 0, &[Op::Work(100)]);
                guest.fail_run_at = (!crash).then_some(5);
                guest.fail_crash = crash;
                Ok(vec![guest, machine(clock, 1, &[Op::Work(100)])])
            },
        )
        .unwrap();
        let ids = ids(&mut driver);
        let peer_log = driver
            .with_barrier(|barrier| {
                barrier.schedule(at(0), Action::Pause(ids[0]))?;
                barrier.schedule(
                    at(5),
                    if crash {
                        Action::Crash(ids[0])
                    } else {
                        Action::Resume(ids[0])
                    },
                )?;
                barrier.schedule(at(5), Action::Wake(ids[1]))?;
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

#[test]
fn invalid_adoption_report_is_rejected_and_pause_schedule_validation_is_atomic() {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        1,
        || Ok(()),
        |_| {
            Ok(vec![ProbeMachine(Box::new(|| {
                Ok(StopReason::Blocked {
                    reached: at(6),
                    deadline: None,
                })
            }))])
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            assert!(matches!(
                barrier.schedule(at(0), Action::Pause(MachineId(9))),
                Err(Error::UnknownMachine(_))
            ));
            assert!(matches!(
                barrier.schedule(at(0), Action::Resume(MachineId(9))),
                Err(Error::UnknownMachine(_))
            ));
            assert!(matches!(
                barrier.paused(MachineId(9)),
                Err(Error::UnknownMachine(_))
            ));
            barrier.schedule(at(0), Action::Pause(id))?;
            barrier.schedule(at(5), Action::Resume(id))
        })
        .unwrap();
    assert!(
        matches!(driver.step(at(100)), Err(Error::InvalidStop { id: invalid, .. }) if invalid == id)
    );
    assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
}
