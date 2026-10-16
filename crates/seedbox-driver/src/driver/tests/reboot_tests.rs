use super::fixture::auto_key;
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

#[test]
fn reboot_schedule_errors_leave_queue_unchanged() {
    let mut driver = controlled(1, &[&[Op::Work(100)]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(3)).unwrap(),
        StepOutcome::BarrierReached { at: at(3) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(matches!(
                barrier.schedule(at(2), auto_key(), Action::Reboot(Target::Machine(id))),
                Err(Error::PastTime { .. })
            ));
            assert!(matches!(
                barrier.schedule(
                    at(3),
                    auto_key(),
                    Action::Reboot(Target::Machine(MachineId(99)))
                ),
                Err(Error::UnknownMachine(_))
            ));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(5)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    driver
        .with_barrier(|barrier| {
            assert!(barrier.machine(id)?.lifecycle.is_empty());
            Ok(())
        })
        .unwrap();
}
