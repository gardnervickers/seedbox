use super::*;

fn controlled(workers: usize, programs: &[&[Op]]) -> Driver<TestMachine> {
    crate::driver::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        workers,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            Ok(programs
                .iter()
                .enumerate()
                .map(|(id, ops)| machine(id, ops))
                .collect())
        },
    )
    .unwrap()
}

fn status(driver: &mut Driver<TestMachine>) -> PlaybackStatus {
    driver
        .with_barrier(|barrier| Ok(barrier.playback()))
        .unwrap()
}

#[test]
fn rational_pacing_waits_only_at_committed_boundaries_without_sleeping() {
    let host = Instant::now();
    let rate = PlaybackRate::Limited {
        numerator: 2.try_into().unwrap(),
        denominator: 3.try_into().unwrap(),
    };
    let mut driver = controlled(1, &[&[Op::Work(100)]]);
    driver.playback.set_rate(rate, driver.position(), host);
    assert_eq!(
        driver.step_at(at(100), host).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    let committed = status(&mut driver);
    for elapsed in [0, 1, 14] {
        assert_eq!(
            driver
                .step_at(at(100), host + Duration::from_nanos(elapsed))
                .unwrap(),
            StepOutcome::Waiting {
                at: at(10),
                delay: Duration::from_nanos(15 - elapsed)
            }
        );
        assert_eq!(status(&mut driver), committed);
    }
    assert_eq!(
        driver
            .step_at(at(100), host + Duration::from_nanos(15))
            .unwrap(),
        StepOutcome::BarrierReached { at: at(20) }
    );
    // A rate change establishes a fresh anchor at the current barrier.
    driver
        .playback
        .set_rate(rate, driver.position(), host + Duration::from_secs(1));
    assert_eq!(
        driver
            .step_at(at(100), host + Duration::from_secs(1))
            .unwrap(),
        StepOutcome::BarrierReached { at: at(30) }
    );
}

#[test]
fn rate_wait_holds_new_current_time_actions_until_host_pacing_permits() {
    let host = Instant::now();
    let mut driver = controlled(1, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    driver.playback.set_rate(
        PlaybackRate::Limited {
            numerator: 1.try_into().unwrap(),
            denominator: 1.try_into().unwrap(),
        },
        driver.position(),
        host,
    );
    assert_eq!(
        driver.step_at(at(100), host).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    driver
        .with_barrier(|barrier| barrier.schedule(at(10), Action::Crash(id)))
        .unwrap();
    let before = status(&mut driver);
    assert!(matches!(
        driver.step_at(at(100), host).unwrap(),
        StepOutcome::Waiting { .. }
    ));
    assert_eq!(status(&mut driver), before);
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.machine(id)?.crashed);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver
            .step_at(at(100), host + Duration::from_nanos(10))
            .unwrap(),
        StepOutcome::Finished {
            at: at(10),
            reason: ExitReason::Crashed
        }
    );
}
