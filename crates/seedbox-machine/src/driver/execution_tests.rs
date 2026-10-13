//! Driver-owned execution settings. Backend tests own partial-work pricing.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    now: u64,
    upto: u64,
    factor: u32,
    lifecycle: Vec<&'static str>,
}
#[derive(Default)]
struct RecordingMachine {
    calls: Vec<Call>,
    exited: bool,
    fail: bool,
}
impl Machine for RecordingMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let mut lifecycle = Vec::new();
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Crash => {
                    self.exited = true;
                    lifecycle.push("crash");
                }
                Request::Reboot => {
                    self.exited = false;
                    lifecycle.push("reboot");
                }
                Request::Shutdown => lifecycle.push("shutdown"),
                Request::Application { .. } | Request::SimulationReply { .. } => {}
            }
        }
        self.calls.push(Call {
            now: context.clock.now(),
            upto: upto.as_nanos(),
            factor: context.execution.factor_permille(),
            lifecycle,
        });
        if self.fail {
            return Err(std::io::Error::other("recording machine failure"));
        }
        Ok(if self.exited {
            StopReason::Exited {
                reached: Timestamp::from_nanos(context.clock.now()),
                reason: ExitReason::Crashed,
            }
        } else {
            StopReason::BoundaryReached
        })
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn config(n: u32) -> ExecutionConfig {
    ExecutionConfig::new(n).unwrap()
}
fn driver(start: u64, workers: usize, machines: Vec<RecordingMachine>) -> Driver<RecordingMachine> {
    super::fixture::driver(
        at(start),
        Duration::from_nanos(10),
        workers,
        SeededRng::new(7),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap()
}
fn setting(machine: MachineId, factor: u32) -> Action {
    Action::SetExecution {
        machine,
        config: config(factor),
    }
}
fn calls(driver: &Driver<RecordingMachine>, id: MachineId) -> &[Call] {
    &driver.machines[id.0].machine.as_ref().unwrap().calls
}

#[test]
fn setting_is_admitted_at_barrier_in_fifo_order_without_a_run_of_its_own() {
    let mut driver = driver(0, 1, vec![RecordingMachine::default()]);
    let id = MachineId(0);
    for action in [
        Action::Wake(id),
        setting(id, 2000),
        Action::Wake(id),
        setting(id, 1001),
        Action::Shutdown(id),
        Action::SetExecution {
            machine: id,
            config: ExecutionConfig::default(),
        },
        Action::Wake(id),
    ] {
        driver.schedule(at(0), action).unwrap();
    }
    assert!(calls(&driver, id).is_empty());
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
    assert!(
        matches!(driver.events[&at(0)][1], Scheduled::Driver(DriverEvent::SetExecution { machine, config: found }) if machine == id && found == config(2000))
    );
    driver.step(at(0)).unwrap();
    assert_eq!(
        calls(&driver, id)
            .iter()
            .map(|call| call.factor)
            .collect::<Vec<_>>(),
        [1000, 2000, 1001, 1000]
    );
    assert_eq!(calls(&driver, id)[2].lifecycle, ["shutdown"]);
    assert!(calls(&driver, id)
        .iter()
        .all(|call| call.now == 0 && call.upto == 0));
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
}

#[test]
fn paused_setting_does_not_adopt_machine_time_or_shift_its_timer() {
    let mut driver = driver(0, 1, vec![RecordingMachine::default()]);
    let id = MachineId(0);
    let blocked = StopReason::Blocked {
        reached: at(0),
        deadline: Some(at(20)),
    };
    driver.machines[0].stopped = blocked;
    driver.schedule(at(0), Action::Pause(id)).unwrap();
    driver.schedule(at(5), setting(id, 2000)).unwrap();
    driver.step(at(5)).unwrap();
    assert!(calls(&driver, id).is_empty());
    assert!(driver.machines[0].paused);
    assert_eq!(driver.machines[0].stopped, blocked);
    assert_eq!(driver.machines[0].reached, at(0));
    assert_eq!(driver.now(), at(5));
    assert_eq!(driver.machines[0].execution, config(2000));
    driver.schedule(at(5), Action::Resume(id)).unwrap();
    driver.step(at(5)).unwrap();
    assert_eq!(
        calls(&driver, id),
        [Call {
            now: 5,
            upto: 5,
            factor: 2000,
            lifecycle: vec![]
        }]
    );
}

#[test]
fn exited_settings_and_crash_reboot_retain_the_configured_value() {
    let mut driver = driver(
        0,
        1,
        vec![
            RecordingMachine::default(),
            RecordingMachine {
                exited: true,
                ..Default::default()
            },
        ],
    );
    let id = MachineId(1);
    driver.schedule(at(0), Action::Wake(id)).unwrap();
    driver.step(at(0)).unwrap();
    let exited = driver.machines[1].stopped;
    driver.schedule(at(0), setting(id, 2000)).unwrap();
    driver.step(at(0)).unwrap();
    assert_eq!(calls(&driver, id).len(), 1);
    assert_eq!(driver.machines[1].stopped, exited);
    for action in [
        Action::Reboot(id),
        Action::Pause(id),
        setting(id, 1001),
        Action::Crash(id),
        Action::SetExecution {
            machine: id,
            config: ExecutionConfig::default(),
        },
        Action::Reboot(id),
    ] {
        driver.schedule(at(0), action).unwrap();
    }
    driver.step(at(0)).unwrap();
    assert_eq!(
        calls(&driver, id)
            .iter()
            .map(|call| (call.factor, call.lifecycle.clone()))
            .collect::<Vec<_>>(),
        [
            (1000, vec![]),
            (2000, vec!["reboot"]),
            (1001, vec!["crash"]),
            (1000, vec!["reboot"])
        ]
    );
    assert!(!driver.machines[1].paused);
    assert_eq!(driver.machines[1].execution, ExecutionConfig::default());
}

#[test]
fn settings_are_machine_local_and_worker_count_independent() {
    let run = |workers| {
        let mut driver = driver(
            0,
            workers,
            vec![RecordingMachine::default(), RecordingMachine::default()],
        );
        driver.schedule(at(0), setting(MachineId(0), 1001)).unwrap();
        driver.schedule(at(0), setting(MachineId(1), 3000)).unwrap();
        driver.schedule(at(2), setting(MachineId(0), 4000)).unwrap();
        while driver.now() < at(4) {
            driver.step(at(4)).unwrap();
        }
        [
            calls(&driver, MachineId(0)).to_vec(),
            calls(&driver, MachineId(1)).to_vec(),
        ]
    };
    let serial = run(1);
    assert_eq!(serial, run(2));
    assert!(serial[0].iter().any(|call| call.factor == 1001));
    assert!(serial[0].iter().any(|call| call.factor == 4000));
    assert!(serial[1].iter().all(|call| call.factor == 3000));
}

#[test]
fn invalid_scheduling_and_failed_steps_do_not_apply_later_settings() {
    let mut driver = driver(
        3,
        1,
        vec![RecordingMachine {
            fail: true,
            ..Default::default()
        }],
    );
    let id = MachineId(0);
    assert!(matches!(
        driver.schedule(at(2), setting(id, 2000)),
        Err(Error::PastTime { .. })
    ));
    assert!(matches!(
        driver.schedule(at(3), setting(MachineId(9), 2000)),
        Err(Error::UnknownMachine(_))
    ));
    assert!(driver.events.is_empty());
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
    for action in [setting(id, 2000), Action::Wake(id), setting(id, 3000)] {
        driver.schedule(at(3), action).unwrap();
    }
    assert!(driver.step(at(3)).is_err());
    assert_eq!(driver.machines[0].execution, config(2000));
    let queued = driver.events[&at(3)].len();
    assert!(matches!(
        driver.schedule(at(3), setting(id, 4000)),
        Err(Error::Failed)
    ));
    assert_eq!(driver.events[&at(3)].len(), queued);
    assert_eq!(driver.machines[0].execution, config(2000));
}

#[test]
fn global_playback_pause_withholds_setting_admission() {
    let mut driver = driver(0, 1, vec![RecordingMachine::default()]);
    let id = MachineId(0);
    driver
        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Paused))
        .unwrap();
    driver.schedule(at(0), setting(id, 2000)).unwrap();
    driver.step(at(0)).unwrap();
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
    assert!(calls(&driver, id).is_empty());
    assert_eq!(driver.events[&at(0)].len(), 1);
    driver
        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    driver.step(at(0)).unwrap();
    assert_eq!(driver.machines[0].execution, config(2000));
    assert!(calls(&driver, id).is_empty());
}
