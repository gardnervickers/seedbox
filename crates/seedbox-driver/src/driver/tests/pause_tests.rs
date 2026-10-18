use super::fixture::auto_key;
use super::*;

#[test]
fn invalid_adoption_report_is_rejected_and_pause_schedule_validation_is_atomic() {
    let mut driver = crate::driver::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        1,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
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
                barrier.schedule(
                    at(0),
                    auto_key(),
                    Action::Pause(Target::Machine(MachineId(9)))
                ),
                Err(Error::UnknownMachine(_))
            ));
            assert!(matches!(
                barrier.schedule(
                    at(0),
                    auto_key(),
                    Action::Resume(Target::Machine(MachineId(9)))
                ),
                Err(Error::UnknownMachine(_))
            ));
            assert!(matches!(
                barrier.paused(MachineId(9)),
                Err(Error::UnknownMachine(_))
            ));
            barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id)))?;
            barrier.schedule(at(5), auto_key(), Action::Resume(Target::Machine(id)))
        })
        .unwrap();
    assert!(
        matches!(driver.step(at(100)), Err(Error::InvalidStop { id: invalid, .. }) if invalid == id)
    );
    assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
}
