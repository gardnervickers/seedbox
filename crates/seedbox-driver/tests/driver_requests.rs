//! Host-scheduled process commands: admitted through the control policy,
//! dispatched with Driver-allocated command IDs and settled by acknowledgment.
use seedbox_driver::driver::{
    Action, Driver, Error, Event, EventKind, FaultAction, FaultOrigin, MachineId, MachineResponse,
    PlaybackRate, RejectionReason, SettlementStatus, StepOutcome, Target,
};
use seedbox_machine::{
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Request, Response},
    rng::RngStream,
};
use std::{io, time::Duration};
#[path = "support/driver_definition.rs"]
mod driver_definition;
#[path = "support/input_key.rs"]
mod input_key;
use input_key::{auto_key, key};

/// Announces readiness once per boot and acknowledges every command.
#[derive(Clone, Default)]
struct Fixture {
    boot: u64,
    resets: usize,
    work: u64,
    exit: Option<ExitReason>,
    fail: bool,
    silent: bool,
    ready: bool,
    commands: Vec<(u64, u32, ApplicationCommand)>,
}
impl Machine for Fixture {
    type Context = ();
    type Error = io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, io::Error> {
        let now = Timestamp::from_nanos(context.clock.now());
        let emit = |context: &mut RunContext<'_, ()>, payload| {
            context
                .outbox
                .emit(seedbox_machine::pv::Output { at: now, payload })
                .unwrap()
        };
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Reboot => {
                    self.boot += 1;
                    self.resets += 1;
                    self.exit = None;
                    self.ready = false;
                }
                Request::Crash => {
                    if self.exit.is_none() {
                        self.exit = Some(ExitReason::Crashed);
                    }
                }
                Request::Shutdown => {}
                Request::Application { id, command } if self.exit.is_none() => {
                    self.commands.push((self.boot, id.get(), command));
                    emit(
                        context,
                        Response::CommandAcknowledged {
                            id,
                            result: CommandResult::Completed,
                        },
                    );
                }
                _ => {}
            }
        }
        if self.fail {
            return Err(io::Error::other("fixture failure"));
        }
        if let Some(reason) = self.exit {
            return Ok(StopReason::Exited {
                reached: now,
                reason,
            });
        }
        if !self.ready && !self.silent {
            self.ready = true;
            emit(context, Response::Ready);
        }
        self.work += upto.as_nanos() - now.as_nanos();
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, io::Error> {
        Ok(self.clone())
    }
}
fn outputs(events: &[Event]) -> Vec<MachineResponse> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Output(output) => Some(output.clone()),
            _ => None,
        })
        .collect()
}
fn settled(events: &[Event]) -> Vec<(String, FaultAction, SettlementStatus)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultSettled { record, status, .. } => match &record.origin {
                FaultOrigin::Input { key } => Some((key.to_string(), record.action, *status)),
                _ => None,
            },
            _ => None,
        })
        .collect()
}
struct Rng;
impl RngStream for Rng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
const DEADLINE: Duration = Duration::from_nanos(1000);
fn stop(target: MachineId) -> Action {
    Action::Stop {
        target: Target::Machine(target),
        deadline: DEADLINE,
    }
}
fn make_with(target: Fixture) -> (Driver<Fixture>, MachineId) {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        vec![driver_definition::definition("target")],
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| {
            Ok(if definition.name == "target" {
                target.clone()
            } else {
                Fixture::default()
            })
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().nth(1).unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            let oracle = barrier.machines().next().unwrap().0;
            barrier.schedule(
                at(0),
                key("oracle-pause"),
                Action::Pause(Target::Machine(oracle)),
            )
        })
        .unwrap();
    (driver, id)
}
fn make() -> (Driver<Fixture>, MachineId) {
    make_with(Fixture::default())
}
/// Run until the target has reported readiness at `at(10)`.
fn ready() -> (Driver<Fixture>, MachineId) {
    let (mut driver, id) = make();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    driver.drain_observations();
    (driver, id)
}

#[test]
fn process_commands_receive_boot_scoped_ids_and_settle_on_acknowledgment() {
    let (mut driver, id) = ready();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(10), key("stop"), stop(id))?;
            barrier.schedule(
                at(11),
                key("restart"),
                Action::Restart {
                    target: Target::Name("target".into()),
                    deadline: DEADLINE,
                },
            )?;
            barrier.schedule(at(12), key("crash"), Action::Crash(Target::Machine(id)))?;
            barrier.schedule(at(13), key("reboot"), Action::Reboot(Target::Machine(id)))
        })
        .unwrap();
    while driver
        .with_barrier(|b| Ok(b.playback().position.at))
        .unwrap()
        < at(30)
    {
        driver.step(at(30)).unwrap();
    }
    driver
        .with_barrier(|barrier| {
            barrier.schedule(
                at(30),
                key("freeze"),
                Action::PauseProcess {
                    target: Target::Machine(id),
                    deadline: DEADLINE,
                },
            )?;
            barrier.schedule(
                at(31),
                key("thaw"),
                Action::ResumeProcess {
                    target: Target::Machine(id),
                    deadline: DEADLINE,
                },
            )
        })
        .unwrap();
    driver.step(at(40)).unwrap();
    driver.step(at(40)).unwrap();
    let events = driver.drain_observations().events;
    assert_eq!(
        settled(&events),
        [
            (
                "stop".into(),
                FaultAction::Application(ApplicationCommand::Stop),
                SettlementStatus::Applied
            ),
            (
                "restart".into(),
                FaultAction::Application(ApplicationCommand::Start),
                SettlementStatus::Applied
            ),
            (
                "crash".into(),
                FaultAction::Crash,
                SettlementStatus::Applied
            ),
            (
                "reboot".into(),
                FaultAction::Reboot,
                SettlementStatus::Applied
            ),
            (
                "freeze".into(),
                FaultAction::Application(ApplicationCommand::Pause),
                SettlementStatus::Applied
            ),
            (
                "thaw".into(),
                FaultAction::Application(ApplicationCommand::Resume),
                SettlementStatus::Applied
            ),
        ]
    );
    // Command IDs come from the Driver's per-boot allocator.
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.machine(id)?.commands,
                [
                    (0, 1, ApplicationCommand::Stop),
                    (0, 2, ApplicationCommand::Start),
                    (1, 1, ApplicationCommand::Pause),
                    (1, 2, ApplicationCommand::Resume),
                ]
            );
            assert_eq!(barrier.machine(id)?.resets, 1);
            Ok(())
        })
        .unwrap();
    // Acknowledgments keep their emitting boot.
    let acks: Vec<_> = outputs(&events)
        .into_iter()
        .filter(|output| matches!(output.payload, Response::CommandAcknowledged { .. }))
        .map(|output| output.boot)
        .collect();
    assert_eq!(acks, [0, 0, 1, 1]);
}

#[test]
fn process_commands_wait_for_readiness_and_unavailable_targets_reject_before_admission() {
    let (mut driver, id) = make();
    // Admitted at 0, dispatched once Ready commits at 10.
    driver
        .with_barrier(|barrier| barrier.schedule(at(0), key("early"), stop(id)))
        .unwrap();
    driver.step(at(10)).unwrap();
    driver.step(at(10)).unwrap();
    let events = driver.drain_observations().events;
    assert_eq!(
        settled(&events),
        [
            (
                "oracle-pause".into(),
                FaultAction::Pause,
                SettlementStatus::Applied
            ),
            (
                "early".into(),
                FaultAction::Application(ApplicationCommand::Stop),
                SettlementStatus::Applied
            ),
        ]
    );
    let settled_at = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::FaultSettled {
                record, outcome_at, ..
            } if matches!(&record.origin, FaultOrigin::Input { key: k } if *k == key("early")) => {
                *outcome_at
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(settled_at, at(10));
    // A physically exited, uncrashed target fails ordinary-control preflight:
    // rejected with its key and no fault ID (a trigger would admit and settle
    // Unavailable instead; see the service policy tests).
    let (mut driver, id) = make_with(Fixture {
        exit: Some(ExitReason::Shutdown),
        ..Fixture::default()
    });
    driver.step(at(10)).unwrap();
    driver.drain_observations();
    driver
        .with_barrier(|barrier| barrier.schedule(at(10), key("late"), stop(id)))
        .unwrap();
    driver.step(at(10)).unwrap();
    let events = driver.drain_observations().events;
    assert!(events
        .iter()
        .all(|event| !matches!(event.kind, EventKind::FaultAdmitted(_))));
    assert!(events.iter().any(|event| matches!(
        &event.kind,
        EventKind::InputRejected { key: found, rejection }
            if *found == key("late") && rejection.reason == RejectionReason::TargetUnavailable
    )));
}

#[test]
fn an_unacknowledged_process_command_fails_the_driver_at_its_own_deadline() {
    let (mut driver, id) = make_with(Fixture {
        silent: true,
        ..Fixture::default()
    });
    driver
        .with_barrier(|barrier| {
            barrier.schedule(
                at(0),
                key("stop"),
                Action::Stop {
                    target: Target::Machine(id),
                    deadline: Duration::from_nanos(25),
                },
            )
        })
        .unwrap();
    let mut result = Ok(StepOutcome::Blocked { at: at(0) });
    for _ in 0..10 {
        result = driver.step(at(100));
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(Error::Control(_))));
    let events = driver.drain_observations().events;
    let failed = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::FaultSettled { record, status, .. }
                if *status == SettlementStatus::Failed =>
            {
                Some((record.origin.clone(), event.at))
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(failed, (FaultOrigin::Input { key: key("stop") }, at(25)));
}

#[test]
fn failed_admission_run_poisons_before_any_response_can_be_observed() {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        Vec::new(),
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        |_| {
            Ok(Fixture {
                fail: true,
                ..Fixture::default()
            })
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), auto_key(), Action::Shutdown(Target::Machine(id)))
        })
        .unwrap();
    assert!(matches!(driver.step(at(0)), Err(Error::Machine { .. })));
    assert!(matches!(
        driver.with_barrier(|_| Ok(())),
        Err(Error::Failed)
    ));
    // The failed batch publishes none of its staged outputs.
    assert!(outputs(&driver.drain_observations().events).is_empty());
    assert!(matches!(driver.step(at(0)), Err(Error::Failed)));
}

#[test]
fn same_time_process_effects_follow_fifo_input_order() {
    let (mut driver, first) = ready();
    driver
        .with_barrier(|barrier| {
            let oracle = barrier.machines().next().unwrap().0;
            barrier.schedule(at(10), auto_key(), Action::Resume(Target::Machine(oracle)))
        })
        .unwrap();
    driver.step(at(20)).unwrap();
    driver.drain_observations();
    let oracle = driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(20), auto_key(), stop(first))?;
            barrier.schedule(at(20), auto_key(), stop(oracle))
        })
        .unwrap();
    assert!(matches!(
        driver.step(at(20)).unwrap(),
        StepOutcome::Finished {
            reason: ExitReason::Shutdown,
            ..
        }
    ));
    let responses = outputs(&driver.drain_observations().events);
    assert_eq!(
        responses
            .iter()
            .map(|response| response.machine)
            .collect::<Vec<_>>(),
        [first, oracle]
    );
}

#[test]
fn process_dispatch_is_held_before_the_next_fault_and_applies_once() {
    let (mut driver, id) = ready();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(
                at(10),
                key("freeze"),
                Action::PauseProcess {
                    target: Target::Machine(id),
                    deadline: DEADLINE,
                },
            )?;
            barrier.schedule(at(10), key("crash"), Action::Crash(Target::Machine(id)))?;
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    let held = driver.step(at(10)).unwrap();
    assert!(matches!(&held, StepOutcome::BeforeFault { at: t, fault }
        if *t == at(10)
            && fault.action == FaultAction::Application(ApplicationCommand::Pause)
            && fault.origin == FaultOrigin::Input { key: key("freeze") }));
    assert_eq!(driver.step(at(10)).unwrap(), held);
    driver
        .with_barrier(|barrier| {
            assert!(barrier.machine(id)?.commands.is_empty());
            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    let held = driver.step(at(10)).unwrap();
    assert!(matches!(&held, StepOutcome::BeforeFault { fault, .. }
        if fault.action == FaultAction::Crash));
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.machine(id)?.commands,
                [(0, 1, ApplicationCommand::Pause)]
            );
            assert!(barrier.machine(id)?.exit.is_none());
            barrier.set_rate(PlaybackRate::Unlimited)
        })
        .unwrap();
    driver.step(at(10)).unwrap();
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.exit, Some(ExitReason::Crashed));
            assert_eq!(barrier.machine(id)?.commands.len(), 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        settled(&driver.drain_observations().events),
        [
            (
                "freeze".into(),
                FaultAction::Application(ApplicationCommand::Pause),
                SettlementStatus::Applied
            ),
            (
                "crash".into(),
                FaultAction::Crash,
                SettlementStatus::Applied
            ),
        ]
    );
}
