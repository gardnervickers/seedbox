use seedbox_driver::driver::{
    Action, Driver, Error, EventKind, FaultOrigin, MachineId, SettlementStatus, StepOutcome, Target,
};
use seedbox_machine::{
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Request, Response},
    rng::RngStream,
};
use std::{num::NonZeroU32, sync::Arc, time::Duration};
#[path = "support/driver_definition.rs"]
mod driver_definition;
#[path = "support/input_key.rs"]
mod input_key;
use input_key::{auto_key, key};
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
        seedbox_driver::driver::NetworkConfig::default(),
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
            .with_barrier(|b| {
                b.schedule(
                    Timestamp::from_nanos(10),
                    auto_key(),
                    Action::Crash(Target::Machine(ids[1])),
                )
            })
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
            assert!(matches!(b.schedule(Timestamp::from_nanos(10), auto_key(), Action::Pause(Target::Machine(ids[1]))),Err(Error::Completed)));
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
fn crash_terminates_and_cuts_off_later_inputs_but_pause_does_not() {
    let (mut driver, calls, ids) = build(2, ExitReason::ProgramExit { code: 0 }, false);
    driver
        .with_barrier(|b| {
            b.schedule(
                Timestamp::from_nanos(0),
                auto_key(),
                Action::Pause(Target::Machine(ids[0])),
            )?;
            b.schedule(
                Timestamp::from_nanos(0),
                auto_key(),
                Action::Crash(Target::Machine(ids[0])),
            )?;
            b.schedule(
                Timestamp::from_nanos(0),
                key("after"),
                Action::Pause(Target::Machine(ids[1])),
            )?;
            b.schedule(
                Timestamp::from_nanos(5),
                key("later"),
                Action::Pause(Target::Machine(ids[1])),
            )
        })
        .unwrap();
    assert_eq!(
        driver.step(Timestamp::from_nanos(0)).unwrap(),
        StepOutcome::Finished {
            at: Timestamp::from_nanos(0),
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    // The same-time input was admitted before the crash effect applied, but its
    // effect is cut off; the future input is never admitted and has no event.
    let events = driver.drain_events();
    assert!(!events.iter().any(|event| matches!(&event.kind,
        EventKind::FaultAdmitted(record) | EventKind::FaultSettled { record, .. }
            if record.origin == FaultOrigin::Input { key: key("later") })));
    let admitted = |name: &str| {
        events.iter().any(|event| matches!(&event.kind,
            EventKind::FaultAdmitted(record) if record.origin == FaultOrigin::Input { key: key(name) }))
    };
    let settled = |name: &str| {
        events.iter().any(|event| matches!(&event.kind,
            EventKind::FaultSettled { record, .. } if record.origin == FaultOrigin::Input { key: key(name) }))
    };
    assert!(admitted("after") && !settled("after"));
    assert_eq!(
        driver.step(Timestamp::from_nanos(100)).unwrap(),
        StepOutcome::Finished {
            at: Timestamp::from_nanos(0),
            reason: ExitReason::Crashed
        }
    );
    driver
        .with_barrier(|b| {
            assert!(!b.paused(ids[1])?);
            Ok(())
        })
        .unwrap();
    assert!(driver.drain_events().is_empty());
    let (mut driver, calls, ids) = build(1, ExitReason::Shutdown, false);
    driver
        .with_barrier(|b| {
            b.schedule(
                Timestamp::from_nanos(0),
                auto_key(),
                Action::Pause(Target::Machine(ids[0])),
            )
        })
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
    ready: bool,
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
        let now = Timestamp::from_nanos(context.clock.now());
        if !self.ready {
            self.ready = true;
            context
                .outbox
                .emit(Output {
                    at: now,
                    payload: Response::Ready,
                })
                .unwrap();
        }
        if self.consume.load(std::sync::atomic::Ordering::Relaxed) {
            while context.inbox.take().is_some() {}
        }
        if let Some((id, result)) = self.reply.lock().unwrap().take() {
            context
                .outbox
                .emit(Output {
                    at: now,
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
fn only_a_consumed_stop_with_its_successful_acknowledgment_finishes() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let consume = Arc::new(AtomicBool::new(false));
    let reply = Arc::new(Mutex::new(None));
    let machine = Acknowledgments {
        consume: consume.clone(),
        reply: reply.clone(),
        ready: false,
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
        seedbox_driver::driver::NetworkConfig::default(),
        move |_| Ok(machine.clone()),
    )
    .unwrap();
    driver
        .with_barrier(|b| {
            b.schedule(
                Timestamp::from_nanos(0),
                key("stop"),
                Action::Stop {
                    target: Target::Name("oracle".into()),
                    deadline: Duration::from_nanos(1000),
                },
            )
        })
        .unwrap();
    // Dispatched at the Ready commit, but the guest leaves it unconsumed.
    assert_eq!(
        driver.step(Timestamp::from_nanos(10)).unwrap(),
        StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(10)
        }
    );
    consume.store(true, Ordering::Relaxed);
    // Consumed, but not yet acknowledged.
    assert_eq!(
        driver.step(Timestamp::from_nanos(20)).unwrap(),
        StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(20)
        }
    );
    *reply.lock().unwrap() = Some((NonZeroU32::new(1).unwrap(), CommandResult::Completed));
    assert_eq!(
        driver.step(Timestamp::from_nanos(30)).unwrap(),
        StepOutcome::Finished {
            at: Timestamp::from_nanos(30),
            reason: ExitReason::Shutdown
        }
    );
    assert!(driver
        .drain_events()
        .iter()
        .any(|event| matches!(&event.kind,
        EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. }
            if record.origin == FaultOrigin::Input { key: key("stop") })));
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
