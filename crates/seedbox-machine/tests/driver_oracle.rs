use seedbox_machine::{
    driver::{Action, Driver, Error, EventKind, MachineId, StepOutcome},
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Request, Response},
    rng::RngStream,
};
use std::{num::NonZeroU32, sync::Arc, time::Duration};
#[path = "support/driver_definition.rs"]
mod driver_definition;
use seedbox_machine::pv::{ApplicationCommand, CommandResult, Output};
use std::sync::Mutex;

type Calls = Arc<Mutex<Vec<(bool, Timestamp)>>>;

#[derive(Clone)]
struct Probe {
    oracle: bool,
    main_only: bool,
    reason: ExitReason,
    calls: Calls,
}
impl Machine for Probe {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = Timestamp::from_nanos(context.clock.now());
        self.calls.lock().unwrap().push((self.oracle, now));
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Crash => {
                    return Ok(StopReason::Exited {
                        reached: now,
                        reason: ExitReason::Crashed,
                    });
                }
                Request::Application { id, command } => context
                    .outbox
                    .emit(Output {
                        at: now,
                        payload: Response::CommandAcknowledged {
                            id,
                            result: if command == ApplicationCommand::Stop {
                                CommandResult::Completed
                            } else {
                                CommandResult::Rejected
                            },
                        },
                    })
                    .unwrap(),
                _ => {}
            }
        }
        if upto > now {
            let at = if self.oracle {
                Timestamp::from_nanos(now.as_nanos() + 1)
            } else {
                upto
            };
            context
                .outbox
                .emit(Output {
                    at,
                    payload: Response::BootConsole {
                        bytes: vec![u8::from(self.oracle)],
                    },
                })
                .unwrap();
            if self.oracle && self.main_only {
                context
                    .outbox
                    .emit(Output {
                        at,
                        payload: Response::ApplicationOutcome { code: 23 },
                    })
                    .unwrap();
                return Ok(StopReason::Blocked {
                    reached: at,
                    deadline: None,
                });
            }
            if self.oracle {
                return Ok(StopReason::Exited {
                    reached: at,
                    reason: self.reason,
                });
            }
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
fn build(
    workers: usize,
    reason: ExitReason,
    main_only: bool,
) -> (Driver<Probe>, Calls, Vec<MachineId>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let observed = calls.clone();
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        workers,
        FixtureRng,
        || Ok(()),
        driver_definition::definition("fixture-0"),
        vec![driver_definition::definition("fixture-1")],
        Vec::new(),
        move |definition| {
            Ok(Probe {
                oracle: definition.name == "fixture-0",
                reason,
                main_only,
                calls: observed.clone(),
            })
        },
    )
    .unwrap();
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect()))
        .unwrap();
    (driver, calls, ids)
}
#[test]
fn oracle_exit_publishes_whole_round_then_sticks_before_due_effects() {
    for workers in [1, 2] {
        let (mut driver, calls, ids) = build(workers, ExitReason::ProgramExit { code: 23 }, false);
        driver
            .with_barrier(|b| b.schedule(Timestamp::from_nanos(10), Action::Crash(ids[1])))
            .unwrap();
        let expected = StepOutcome::Finished {
            at: Timestamp::from_nanos(10),
            reason: ExitReason::ProgramExit { code: 23 },
        };
        assert_eq!(driver.step(Timestamp::from_nanos(10)).unwrap(), expected);
        let outputs = driver
            .drain_events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Output(output) => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        driver.with_barrier(|b| {
            assert_eq!(outputs.len(),2);
            assert_eq!(outputs[0].at,Timestamp::from_nanos(1));
            assert_eq!(outputs[1].at,Timestamp::from_nanos(10));
            assert!(matches!(b.stopped(ids[0])?, StopReason::Exited { reached, .. } if reached == Timestamp::from_nanos(1)));
            assert_eq!(b.stopped(ids[1])?, StopReason::BoundaryReached);
            assert!(matches!(b.schedule(Timestamp::from_nanos(10), Action::Wake(ids[1])),Err(Error::Completed)));
            Ok(())
        }).unwrap();
        let count = calls.lock().unwrap().len();
        assert_eq!(driver.step(Timestamp::from_nanos(100)).unwrap(), expected);
        assert_eq!(calls.lock().unwrap().len(), count);
        driver
            .with_barrier(|b| {
                let _final_snapshot = b.snapshot(ids[0])?;
                Ok(())
            })
            .unwrap();
        assert!(driver.drain_events().is_empty());
    }
}
#[test]
fn crash_and_confirmed_application_stop_terminate_but_pause_does_not() {
    for stop in [false, true] {
        let (mut driver, calls, ids) = build(2, ExitReason::ProgramExit { code: 0 }, false);
        driver
            .with_barrier(|b| {
                b.schedule(Timestamp::from_nanos(0), Action::Pause(ids[0]))?;
                if stop {
                    b.schedule_request(
                        Timestamp::from_nanos(0),
                        ids[0],
                        Request::Application {
                            id: NonZeroU32::new(7).unwrap(),
                            command: ApplicationCommand::Stop,
                        },
                    )?;
                } else {
                    b.schedule(Timestamp::from_nanos(0), Action::Crash(ids[0]))?;
                }
                b.schedule(Timestamp::from_nanos(0), Action::Reboot(ids[0]))
            })
            .unwrap();
        assert_eq!(
            driver.step(Timestamp::from_nanos(0)).unwrap(),
            StepOutcome::Finished {
                at: Timestamp::from_nanos(0),
                reason: if stop {
                    ExitReason::Shutdown
                } else {
                    ExitReason::Crashed
                }
            }
        );
        assert_eq!(calls.lock().unwrap().len(), 1);
        if stop {
            assert_eq!(
                driver
                    .drain_events()
                    .into_iter()
                    .filter_map(|event| match event.kind {
                        EventKind::Output(output) => Some(output),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .len(),
                1
            );
            driver
                .with_barrier(|b| {
                    assert_eq!(b.stopped(ids[0])?, StopReason::BoundaryReached);
                    Ok(())
                })
                .unwrap();
        }
    }
    let (mut driver, calls, ids) = build(1, ExitReason::Shutdown, false);
    driver
        .with_barrier(|b| b.schedule(Timestamp::from_nanos(0), Action::Pause(ids[0])))
        .unwrap();
    assert_eq!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(0)
        }
    );
    assert!(calls.lock().unwrap().is_empty());
}

struct FixtureRng;
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}

#[derive(Clone)]
struct Acknowledgments {
    consume: Arc<std::sync::atomic::AtomicBool>,
    reply: Arc<Mutex<Option<(NonZeroU32, CommandResult)>>>,
}
impl Machine for Acknowledgments {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        if self.consume.load(std::sync::atomic::Ordering::Relaxed) {
            while context.inbox.take().is_some() {}
        }
        if let Some((id, result)) = self.reply.lock().unwrap().take() {
            context
                .outbox
                .emit(Output {
                    at: Timestamp::from_nanos(context.clock.now()),
                    payload: Response::CommandAcknowledged { id, result },
                })
                .unwrap();
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
#[test]
fn only_consumed_stop_and_matching_successful_ack_finish() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let consume = Arc::new(AtomicBool::new(false));
    let reply = Arc::new(Mutex::new(None));
    let machine = Acknowledgments {
        consume: consume.clone(),
        reply: reply.clone(),
    };
    let mut driver = Driver::new(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        1,
        FixtureRng,
        || Ok(()),
        driver_definition::definition("oracle"),
        Vec::new(),
        Vec::new(),
        move |_| Ok(machine.clone()),
    )
    .unwrap();
    let id = driver
        .with_barrier(|b| Ok(b.machines().next().unwrap().0))
        .unwrap();
    let stop = |command| Request::Application {
        id: NonZeroU32::new(command).unwrap(),
        command: ApplicationCommand::Stop,
    };
    driver
        .with_barrier(|b| b.schedule_request(Timestamp::from_nanos(0), id, stop(7)))
        .unwrap();
    assert!(matches!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::BarrierReached { .. }
    ));
    consume.store(true, Ordering::Relaxed);
    *reply.lock().unwrap() = Some((NonZeroU32::new(7).unwrap(), CommandResult::Rejected));
    driver
        .with_barrier(|b| b.schedule(Timestamp::from_nanos(0), Action::Wake(id)))
        .unwrap();
    assert!(matches!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::BarrierReached { .. }
    ));
    // A different, actually admitted command cannot complete Stop8.
    driver
        .with_barrier(|b| {
            b.schedule_request(
                Timestamp::from_nanos(0),
                id,
                Request::Application {
                    id: NonZeroU32::new(99).unwrap(),
                    command: ApplicationCommand::Start,
                },
            )
        })
        .unwrap();
    driver.step(Timestamp::from_nanos(0)).unwrap();
    *reply.lock().unwrap() = Some((NonZeroU32::new(99).unwrap(), CommandResult::Completed));
    driver
        .with_barrier(|b| b.schedule_request(Timestamp::from_nanos(0), id, stop(8)))
        .unwrap();
    assert!(matches!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::BarrierReached { .. }
    ));
    // No acknowledgment exists yet for accepted Stop8.
    driver
        .with_barrier(|b| b.schedule(Timestamp::from_nanos(0), Action::Wake(id)))
        .unwrap();
    assert!(matches!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::BarrierReached { .. }
    ));
    *reply.lock().unwrap() = Some((NonZeroU32::new(8).unwrap(), CommandResult::Completed));
    driver
        .with_barrier(|b| b.schedule(Timestamp::from_nanos(0), Action::Wake(id)))
        .unwrap();
    assert_eq!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::Finished {
            at: Timestamp::from_nanos(0),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
fn oracle_application_outcome_finishes_without_claiming_machine_exit() {
    let (mut driver, _, ids) = build(2, ExitReason::Shutdown, true);
    assert_eq!(
        driver.step(Timestamp::from_nanos(10)).unwrap(),
        StepOutcome::Finished {
            at: Timestamp::from_nanos(10),
            reason: ExitReason::ProgramExit { code: 23 }
        }
    );
    assert!(driver
        .drain_events()
        .into_iter()
        .filter_map(|event| match event.kind {
            EventKind::Output(output) => Some(output),
            _ => None,
        })
        .collect::<Vec<_>>()
        .iter()
        .any(|output| matches!(output.payload, Response::ApplicationOutcome { code: 23 })));
    driver
        .with_barrier(|b| {
            assert!(matches!(b.stopped(ids[0])?, StopReason::Blocked { .. }));
            Ok(())
        })
        .unwrap();
}
