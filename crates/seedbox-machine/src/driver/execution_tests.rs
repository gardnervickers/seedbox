//! Driver-owned execution settings. Backend tests own partial-work pricing.
use super::fixture::auto_key;
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
                Request::Application { .. }
                | Request::StepRealtime { .. }
                | Request::SimulationReply { .. } => {}
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
    Action::Throttle {
        target: Target::Machine(machine),
        config: config(factor),
    }
}
fn heal(key: &InputKey) -> Action {
    Action::Heal(HealTarget::Input(key.clone()))
}
fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
fn calls(driver: &Driver<RecordingMachine>, id: MachineId) -> &[Call] {
    &driver.machines[id.0].machine.as_ref().unwrap().calls
}
fn rejected(events: &[Event]) -> Vec<(InputKey, RejectionReason)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::InputRejected { key, rejection } => Some((key.clone(), rejection.reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn throttle_and_heal_apply_in_fifo_order_without_a_run_of_their_own() {
    let mut driver = driver(0, 1, vec![RecordingMachine::default()]);
    let id = MachineId(0);
    let slow = key("slow");
    driver
        .schedule(at(0), slow.clone(), setting(id, 2000))
        .unwrap();
    driver
        .schedule(at(0), auto_key(), Action::Shutdown(Target::Machine(id)))
        .unwrap();
    // Admitted before the throttle's FIFO-tail effect applies: still unsettled.
    driver.schedule(at(0), key("early"), heal(&slow)).unwrap();
    assert!(calls(&driver, id).is_empty());
    assert!(matches!(
        driver.events[&at(0)][0],
        Scheduled::Control(control::Event::Input {
            action: Action::Throttle { .. },
            ..
        })
    ));
    driver.drain_events();
    driver.step(at(0)).unwrap();
    // The shutdown effect follows the throttle effect in FIFO order.
    assert_eq!(
        calls(&driver, id),
        [Call {
            now: 0,
            upto: 0,
            factor: 2000,
            lifecycle: vec!["shutdown"]
        }]
    );
    assert_eq!(driver.machines[0].execution, config(2000));
    assert_eq!(
        rejected(&driver.drain_events()),
        [(key("early"), RejectionReason::InvalidState)]
    );
    // An active handle keeps its key live until healed.
    assert!(matches!(
        driver.schedule(at(0), slow.clone(), setting(id, 3000)),
        Err(Error::LiveInputKey(found)) if found == slow
    ));
    driver.schedule(at(0), key("heal"), heal(&slow)).unwrap();
    driver.step(at(0)).unwrap();
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
    assert_eq!(calls(&driver, id).len(), 1);
    assert!(rejected(&driver.drain_events()).is_empty());
    driver.schedule(at(0), slow, setting(id, 3000)).unwrap();
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
    driver
        .schedule(at(0), auto_key(), Action::Pause(Target::Machine(id)))
        .unwrap();
    driver
        .schedule(at(5), auto_key(), setting(id, 2000))
        .unwrap();
    driver.step(at(5)).unwrap();
    assert!(calls(&driver, id).is_empty());
    assert!(driver.machines[0].paused);
    assert_eq!(driver.machines[0].stopped, blocked);
    assert_eq!(driver.machines[0].reached, at(0));
    assert_eq!(driver.now(), at(5));
    assert_eq!(driver.machines[0].execution, config(2000));
    driver
        .schedule(at(5), auto_key(), Action::Resume(Target::Machine(id)))
        .unwrap();
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
    driver.step(at(1)).unwrap();
    assert!(matches!(
        driver.machines[1].stopped,
        StopReason::Exited { .. }
    ));
    let exited = driver.machines[1].stopped;
    let slow = key("slow");
    driver
        .schedule(at(1), slow.clone(), setting(id, 2000))
        .unwrap();
    driver.step(at(1)).unwrap();
    assert_eq!(calls(&driver, id).len(), 1);
    assert_eq!(driver.machines[1].stopped, exited);
    assert_eq!(driver.machines[1].execution, config(2000));
    for (time, action) in [
        // A confirmed physical crash makes the boot down: Pause and Crash are
        // refused and Reboot is admitted directly. The new boot is live.
        (1, Action::Pause(Target::Machine(id))),
        (1, Action::Crash(Target::Machine(id))),
        (2, Action::Reboot(Target::Machine(id))),
        (3, heal(&slow)),
        (3, Action::Crash(Target::Machine(id))),
        (4, Action::Reboot(Target::Machine(id))),
    ] {
        driver.schedule(at(time), auto_key(), action).unwrap();
    }
    while driver.now() < at(5) {
        driver.step(at(5)).unwrap();
    }
    assert_eq!(
        calls(&driver, id)
            .iter()
            .filter(|call| !call.lifecycle.is_empty())
            .map(|call| (call.factor, call.lifecycle.clone()))
            .collect::<Vec<_>>(),
        [
            (2000, vec!["reboot"]),
            (1000, vec!["crash"]),
            (1000, vec!["reboot"])
        ]
    );
    let rejected = rejected(&driver.drain_events());
    assert_eq!(
        rejected
            .iter()
            .map(|(_, reason)| *reason)
            .collect::<Vec<_>>(),
        [RejectionReason::InvalidState, RejectionReason::InvalidState]
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
        let first = key("first");
        driver
            .schedule(at(0), first.clone(), setting(MachineId(0), 1001))
            .unwrap();
        driver
            .schedule(at(0), auto_key(), setting(MachineId(1), 3000))
            .unwrap();
        driver.schedule(at(1), auto_key(), heal(&first)).unwrap();
        driver
            .schedule(at(2), auto_key(), setting(MachineId(0), 4000))
            .unwrap();
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
        driver.schedule(at(2), auto_key(), setting(id, 2000)),
        Err(Error::PastTime { .. })
    ));
    assert!(matches!(
        driver.schedule(at(3), auto_key(), setting(MachineId(9), 2000)),
        Err(Error::UnknownMachine(_))
    ));
    assert!(driver.events.is_empty());
    assert_eq!(driver.machines[0].execution, ExecutionConfig::default());
    let slow = key("slow");
    driver
        .schedule(at(3), slow.clone(), setting(id, 2000))
        .unwrap();
    // The shutdown admission run fails; the later heal never applies.
    driver
        .schedule(at(3), auto_key(), Action::Shutdown(Target::Machine(id)))
        .unwrap();
    driver.schedule(at(3), auto_key(), heal(&slow)).unwrap();
    assert!(driver.step(at(3)).is_err());
    assert_eq!(driver.machines[0].execution, config(2000));
    let queued = driver.events.values().map(VecDeque::len).sum::<usize>();
    assert!(matches!(
        driver.schedule(at(3), auto_key(), setting(id, 4000)),
        Err(Error::Failed)
    ));
    assert_eq!(
        driver.events.values().map(VecDeque::len).sum::<usize>(),
        queued
    );
    assert_eq!(driver.machines[0].execution, config(2000));
}

#[test]
fn global_playback_pause_withholds_setting_admission() {
    let mut driver = driver(0, 1, vec![RecordingMachine::default()]);
    let id = MachineId(0);
    driver
        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Paused))
        .unwrap();
    driver
        .schedule(at(0), auto_key(), setting(id, 2000))
        .unwrap();
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
