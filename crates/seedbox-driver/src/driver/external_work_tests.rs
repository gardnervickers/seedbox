use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct Idle(Arc<AtomicUsize>);
impl Machine for Idle {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        self.0.fetch_add(1, Ordering::SeqCst);
        while context.inbox.take().is_some() {}
        Ok(StopReason::Blocked {
            reached: Timestamp::from_nanos(context.clock.now()),
            deadline: None,
        })
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
fn driver(workers: usize) -> (Driver<Idle>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let spy = calls.clone();
    let driver = Driver::new_seeded(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        51,
        || Ok(()),
        fixture::definition(0),
        vec![],
        vec![],
        NetworkConfig::default(),
        move |_| Ok(Idle(spy.clone())),
    )
    .unwrap();
    (driver, calls)
}
fn at(driver: &mut Driver<Idle>) -> u64 {
    driver
        .with_barrier(|barrier| Ok(barrier.playback().position.at.as_nanos()))
        .unwrap()
}
#[test]
fn external_deadline_advances_idle_driver_in_quanta_without_running_idle_machines() {
    for workers in [1, 4] {
        let (mut driver, calls) = driver(workers);
        assert!(
            matches!(driver.step(Timestamp::from_nanos(100)).unwrap(), StepOutcome::Blocked { at } if at.as_nanos() == 0)
        );
        let initial_calls = calls.load(Ordering::SeqCst);
        for expected in (10..=100).step_by(10) {
            assert!(
                matches!(driver.step_toward_external(Timestamp::from_nanos(100)).unwrap(), StepOutcome::Blocked { at } if at.as_nanos() == expected)
            );
            assert_eq!(at(&mut driver), expected);
            assert_eq!(calls.load(Ordering::SeqCst), initial_calls);
        }
        assert_eq!(
            driver
                .with_barrier(|barrier| Ok(barrier.playback().position.completed_quanta))
                .unwrap(),
            10
        );
    }
}
#[test]
fn external_deadline_honors_global_playback_pause_without_running_or_advancing() {
    let (mut driver, calls) = driver(4);
    driver
        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Paused))
        .unwrap();
    assert!(
        matches!(driver.step_toward_external(Timestamp::from_nanos(100)).unwrap(), StepOutcome::Waiting { at, .. } if at.as_nanos() == 0)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(at(&mut driver), 0);
}
#[test]
fn external_deadline_is_not_checkpoint_state_and_restore_requires_fresh_host_authority() {
    let (mut source, _) = driver(1);
    source.step(Timestamp::from_nanos(100)).unwrap();
    source
        .step_toward_external(Timestamp::from_nanos(100))
        .unwrap();
    let checkpoint = source.checkpoint().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let restore_calls = calls.clone();
    let create_calls = calls.clone();
    let mut restored = Driver::from_checkpoint(
        &checkpoint,
        4,
        || Ok(()),
        move |_| Ok(Idle(restore_calls.clone())),
        move |_| Ok(Idle(create_calls.clone())),
    )
    .unwrap();
    assert_eq!(at(&mut restored), 10);
    assert!(
        matches!(restored.step(Timestamp::from_nanos(100)).unwrap(), StepOutcome::Blocked { at } if at.as_nanos() == 10)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    restored
        .step_toward_external(Timestamp::from_nanos(100))
        .unwrap();
    assert_eq!(at(&mut restored), 20);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
