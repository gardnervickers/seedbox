mod driver_fixture;
use driver_fixture::*;

#[test]
fn shutdown_after_reboot_targets_the_pending_boot() {
    let mut driver = controlled(1, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(1), Action::Reboot(id))?;
            barrier.schedule(at(1), Action::Shutdown(id))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    driver
        .with_barrier(|barrier| {
            let guest = barrier.machine(id)?;
            assert_eq!(guest.pending_boots, 0);
            assert_eq!(guest.boot_times, [0, 1]);
            assert!(guest.shutdown_requested && guest.shutdown_delivered);
            assert_eq!(guest.shutdown_latches, 1);
            assert_eq!(guest.shutdown_deliveries, 1);
            assert_eq!(guest.executed, 1);
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            Ok(())
        })
        .unwrap();
}

#[test]
fn shutdown_refreshes_blocked_scheduling_without_cpu_and_later_runs_exit() {
    let mut driver = controlled(2, &[&[Op::Sleep(100), Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    driver
        .with_barrier(|barrier| barrier.schedule(at(1), Action::Shutdown(id)))
        .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            let guest = barrier.machine(id)?;
            assert!(guest.shutdown_requested && guest.shutdown_delivered);
            assert_eq!(guest.shutdown_latches, 1);
            assert_eq!(guest.shutdown_deliveries, 1);
            assert_eq!(guest.executed, 0);
            assert_eq!(guest.reached, 1);
            assert_eq!(*guest.log.lock().unwrap(), [(0, 0, 1), (0, 1, 1)]);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(2)).unwrap(),
        StepOutcome::BarrierReached { at: at(2) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(3),
            reason: ExitReason::Shutdown
        }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(id)?,
                StopReason::Exited {
                    reached: at(3),
                    reason: ExitReason::Shutdown
                }
            );
            assert_eq!(barrier.machine(id)?.executed, 2);
            Ok(())
        })
        .unwrap();
}

#[test]
fn paused_shutdown_is_delivered_once_without_executing_cleanup_until_resume() {
    let mut driver = controlled(2, &[&[Op::Park, Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::Blocked { at: at(0) }
    );
    driver
        .with_barrier(|barrier| {
            for (time, action) in [
                (0, Action::Pause(id)),
                (5, Action::Shutdown(id)),
                (5, Action::Shutdown(id)),
                (10, Action::Resume(id)),
            ] {
                barrier.schedule(at(time), action)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    let saved = driver
        .with_barrier(|barrier| {
            assert!(barrier.paused(id)?);
            let guest = barrier.machine(id)?;
            assert!(guest.shutdown_requested);
            assert!(guest.shutdown_delivered);
            assert_eq!(guest.shutdown_latches, 1);
            assert_eq!(guest.shutdown_deliveries, 1);
            assert_eq!(guest.executed, 0);
            assert_eq!(guest.log.lock().unwrap().len(), 3);
            barrier.snapshot(id)
        })
        .unwrap();
    assert!(saved.shutdown_requested && saved.shutdown_delivered);
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert!(barrier.machine(id)?.shutdown_delivered);
            assert_eq!(barrier.machine(id)?.shutdown_deliveries, 1);
            assert_eq!(barrier.machine(id)?.executed, 0);
            assert_eq!(barrier.machine(id)?.reached, 10);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(12),
            reason: ExitReason::Shutdown
        }
    );
    assert!(saved.shutdown_requested && saved.shutdown_delivered);
    assert_eq!(saved.reached, 5);
}

#[test]
fn repeated_in_progress_and_exited_shutdown_requests_do_not_latch_or_deliver_again() {
    let mut driver = controlled_targets(1, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            for (time, action) in [
                (0, Action::Shutdown(id)),
                (0, Action::Shutdown(id)),
                (1, Action::Shutdown(id)),
                (5, Action::Shutdown(id)),
            ] {
                barrier.schedule(at(time), action)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::BarrierReached { at: at(1) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(5) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(id)?,
                StopReason::Exited {
                    reached: at(5),
                    reason: ExitReason::Shutdown
                }
            );
            assert_eq!(barrier.machine(id)?.shutdown_latches, 1);
            assert_eq!(barrier.machine(id)?.shutdown_deliveries, 1);
            assert_eq!(barrier.machine(id)?.executed, 2);
            Ok(())
        })
        .unwrap();
    for crash in [false, true] {
        let mut driver = controlled_targets(1, &[&[Op::Exit]]);
        let id = ids(&mut driver)[0];
        if crash {
            driver
                .with_barrier(|barrier| barrier.schedule(at(0), Action::Crash(id)))
                .unwrap();
        }
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Blocked { at: at(0) }
        );
        driver
            .with_barrier(|barrier| barrier.schedule(at(0), Action::Shutdown(id)))
            .unwrap();
        assert_eq!(
            driver.step(at(0)).unwrap(),
            StepOutcome::Blocked { at: at(0) }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.machine(id)?.shutdown_latches, 0);
                assert_eq!(barrier.machine(id)?.shutdown_deliveries, 0);
                Ok(())
            })
            .unwrap();
    }
}

#[test]
fn crash_and_reboot_discard_pending_or_delivered_shutdown_without_affecting_snapshot() {
    for paused in [false, true] {
        let mut driver = controlled_targets(1, &[&[Op::Work(100)]]);
        let id = ids(&mut driver)[0];
        driver
            .with_barrier(|barrier| {
                if paused {
                    barrier.schedule(at(0), Action::Pause(id))?;
                }
                barrier.schedule(at(0), Action::Shutdown(id))
            })
            .unwrap();
        let outcome = driver.step(at(0)).unwrap();
        assert_eq!(
            outcome,
            if paused {
                StepOutcome::Blocked { at: at(0) }
            } else {
                StepOutcome::BarrierReached { at: at(0) }
            }
        );
        let saved = driver
            .with_barrier(|barrier| {
                assert!(barrier.machine(id)?.shutdown_delivered);
                let saved = barrier.snapshot(id)?;
                barrier.schedule(at(0), Action::Crash(id))?;
                barrier.schedule(at(0), Action::Reboot(id))?;
                Ok(saved)
            })
            .unwrap();
        assert_eq!(
            driver.step(at(0)).unwrap(),
            StepOutcome::BarrierReached { at: at(0) }
        );
        driver
            .with_barrier(|barrier| {
                assert!(!barrier.paused(id)?);
                assert!(!barrier.machine(id)?.shutdown_requested);
                assert!(!barrier.machine(id)?.shutdown_delivered);
                assert_eq!(barrier.machine(id)?.executed, 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            driver.step(at(5)).unwrap(),
            StepOutcome::BarrierReached { at: at(5) }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.machine(id)?.executed, 5);
                barrier.schedule(at(5), Action::Shutdown(id))
            })
            .unwrap();
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Blocked { at: at(7) }
        );
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.machine(id)?.shutdown_latches, 2);
                Ok(())
            })
            .unwrap();
        assert!(saved.shutdown_requested);
        assert!(saved.shutdown_delivered);
    }
}

#[test]
fn shutdown_request_or_delivery_error_poisons_before_later_actions() {
    for request_error in [false, true] {
        let mut driver = configured(2, {
            let mut guest = machine(0, &[Op::Work(100)]);
            guest.fail_shutdown = request_error;
            guest.fail_run_at = (!request_error).then_some(5);
            vec![guest, machine(1, &[Op::Work(100)])]
        });
        let ids = ids(&mut driver);
        let peer = driver
            .with_barrier(|barrier| {
                barrier.schedule(at(5), Action::Shutdown(ids[0]))?;
                barrier.schedule(at(5), Action::Wake(ids[1]))?;
                Ok(barrier.machine(ids[1])?.log.clone())
            })
            .unwrap();
        assert!(matches!(driver.step(at(100)), Err(Error::Machine { id, .. }) if id == ids[0]));
        assert_eq!(peer.lock().unwrap().len(), 1);
        assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
        assert!(matches!(
            driver.with_barrier::<()>(|_| panic!("failed shutdown barrier")),
            Err(Error::Failed)
        ));
    }
}

#[test]
fn shutdown_does_not_consume_pre_crash_arm_and_worker_count_does_not_change_results() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = controlled(
            workers,
            &[&[Op::Sleep(100), Op::Work(100)], &[Op::Work(100)]],
        );
        let ids = ids(&mut driver);
        driver
            .with_barrier(|barrier| {
                for (time, action) in [
                    (3, Action::Pause(ids[0])),
                    (5, Action::Shutdown(ids[0])),
                    (7, Action::Resume(ids[0])),
                    (7, Action::Shutdown(ids[0])),
                    (8, Action::Crash(ids[1])),
                ] {
                    barrier.schedule(at(time), action)?;
                }
                barrier.pause_before_next_crash(PlaybackRate::Unlimited)
            })
            .unwrap();
        let mut outcomes = Vec::new();
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome);
            if matches!(outcome, StepOutcome::BeforeAction { .. }) {
                break;
            }
            assert!(outcomes.len() < 10);
        }
        assert_eq!(
            *outcomes.last().unwrap(),
            StepOutcome::BeforeAction {
                at: at(8),
                action: Action::Crash(ids[1])
            }
        );
        driver
            .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        outcomes.push(driver.step(at(100)).unwrap());
        assert_eq!(
            *outcomes.last().unwrap(),
            StepOutcome::Finished {
                at: at(9),
                reason: ExitReason::Shutdown
            }
        );
        let observed = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(id, guest)| {
                        (
                            barrier.stopped(id).unwrap(),
                            guest.executed,
                            guest.shutdown_latches,
                            guest.shutdown_deliveries,
                            guest.log.lock().unwrap().clone(),
                        )
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap();
        let result = (outcomes, observed);
        if let Some(reference) = &reference {
            assert_eq!(&result, reference);
        } else {
            reference = Some(result);
        }
    }
}
