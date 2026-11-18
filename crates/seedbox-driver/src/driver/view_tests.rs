//! `Barrier::view` reports what `inspect` does: machine states and the
//! active handles of every origin.
use super::fixture::owned::*;
use super::*;

fn view(driver: &mut Driver<Mock>) -> WorldView {
    driver.with_barrier(|barrier| barrier.view()).unwrap()
}
fn input(driver: &mut Driver<Mock>, time: u64, name: &str, action: Action) {
    driver.schedule(at(time), key(name), action).unwrap();
}
fn run_to(driver: &mut Driver<Mock>, upto: u64) {
    while driver.now() < at(upto) {
        driver.step(at(upto)).unwrap();
    }
}

#[test]
fn view_names_each_state_and_every_active_handle() {
    let ready = || vec![(0, Response::Ready)];
    let mut driver = build(
        (0..4).map(|index| spec(index, true, ready())).collect(),
        vec![],
        true,
    );
    let target = |index| Target::Machine(MachineId(index));
    input(&mut driver, 10, "pause", Action::Pause(target(1)));
    input(&mut driver, 10, "crash", Action::Crash(target(2)));
    input(
        &mut driver,
        10,
        "throttle",
        Action::Throttle {
            target: target(3),
            config: crate::execution::ExecutionConfig::new(2000).unwrap(),
        },
    );
    run_to(&mut driver, 20);
    let seen = view(&mut driver);
    let states: Vec<_> = seen
        .machines
        .iter()
        .map(|machine| (machine.id, machine.name.as_deref(), machine.state))
        .collect();
    assert_eq!(
        states,
        [
            (MachineId(0), Some("fixture-0"), MachineState::Running),
            (MachineId(1), Some("fixture-1"), MachineState::Paused),
            (MachineId(2), Some("fixture-2"), MachineState::Down),
            (MachineId(3), Some("fixture-3"), MachineState::Running),
        ]
    );
    assert_eq!(seen.handles.len(), 1);
    // Reading it changed nothing, and a listed handle is one a heal ends.
    assert_eq!(view(&mut driver), seen);
    input(
        &mut driver,
        20,
        "heal",
        Action::Heal(HealTarget::Handle(seen.handles[0].clone())),
    );
    input(&mut driver, 20, "reboot", Action::Reboot(target(2)));
    input(&mut driver, 20, "resume", Action::Resume(target(1)));
    run_to(&mut driver, 30);
    let seen = view(&mut driver);
    assert!(seen.handles.is_empty());
    assert!(seen
        .machines
        .iter()
        .all(|machine| machine.state == MachineState::Running));
}
