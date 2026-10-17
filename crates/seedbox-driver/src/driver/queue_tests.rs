use super::fixture::auto_key;
use super::*;
use crate::pv::{ApplicationCommand, CommandResult, Request, Response};
use seedbox_test_time::{handshake, HangGuard, Participant};
use std::{num::NonZeroU32, sync::Mutex};

#[derive(Clone)]
struct QueueMachine {
    consume: bool,
    boot: u32,
    calls: usize,
    accepted: Vec<(u32, u32)>,
    emit: bool,
    invalid_output: bool,
    fail_second: bool,
    entered: Option<Arc<Mutex<Option<Participant>>>>,
    exit: bool,
}
impl QueueMachine {
    fn new() -> Self {
        Self {
            consume: true,
            boot: 0,
            calls: 0,
            accepted: Vec::new(),
            emit: false,
            invalid_output: false,
            fail_second: false,
            entered: None,
            exit: false,
        }
    }
}
impl Machine for QueueMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while let Some(input) = context.inbox.front() {
            match input.payload {
                crate::pv::Request::Crash => {
                    context.inbox.take();
                    self.crash()?;
                    return Ok(StopReason::Exited {
                        reached: Timestamp::from_nanos(context.clock.now()),
                        reason: crate::machine::ExitReason::Crashed,
                    });
                }
                crate::pv::Request::Reboot => {
                    context.inbox.take();
                    self.reboot(Timestamp::from_nanos(context.clock.now()))?;
                }
                crate::pv::Request::Shutdown => {
                    context.inbox.take();
                    self.shutdown()?;
                }
                crate::pv::Request::Application { .. }
                | crate::pv::Request::StepRealtime { .. }
                | crate::pv::Request::SimulationReply { .. }
                | crate::pv::Request::ReceiveFrame { .. } => break,
                _ => return Err(std::io::Error::other("unexpected fixture request")),
            }
        }

        self.calls += 1;
        if self.fail_second && self.calls == 2 {
            return Err(std::io::Error::other("second batch failure"));
        }
        if let Some(entered) = &self.entered {
            if let Some(participant) = entered.lock().unwrap().take() {
                participant
                    .enter_and_wait(&HangGuard::new(
                        "slow worker awaits actual fast result publication",
                        Duration::from_secs(30),
                    ))
                    .unwrap();
            }
        }
        if self.consume {
            while let Some(input) = context.inbox.take() {
                let Request::Application { id, .. } = input.payload else {
                    unreachable!()
                };
                self.accepted.push((self.boot, id.get()));
                context
                    .outbox
                    .emit(Output {
                        at: Timestamp::from_nanos(context.clock.now()),
                        payload: Response::CommandAcknowledged {
                            id,
                            result: CommandResult::Completed,
                        },
                    })
                    .unwrap();
            }
        }
        let now = Timestamp::from_nanos(context.clock.now());
        if self.exit {
            return Ok(StopReason::Exited {
                reached: now,
                reason: crate::machine::ExitReason::Shutdown,
            });
        }
        if self.emit {
            for code in [1, 2] {
                context
                    .outbox
                    .emit(Output {
                        at: upto,
                        payload: Response::ApplicationOutcome { code },
                    })
                    .unwrap();
            }
        }
        if self.invalid_output {
            return Ok(StopReason::Blocked {
                reached: now,
                deadline: None,
            });
        }
        if self.fail_second && self.calls == 1 {
            return Ok(StopReason::Blocked {
                reached: now,
                deadline: Some(Timestamp::from_nanos(now.as_nanos() + 1)),
            });
        }
        Ok(if self.emit {
            StopReason::BoundaryReached
        } else {
            StopReason::Blocked {
                reached: now,
                deadline: None,
            }
        })
    }

    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn driver(machines: Vec<QueueMachine>) -> Driver<QueueMachine> {
    super::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        2,
        SeededRng::new(3),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap()
}
fn enqueue(driver: &mut Driver<QueueMachine>, at: Timestamp, id: u32) {
    enqueue_to(driver, at, MachineId(0), id);
}
/// Raw queue entries exercise private timeline mechanics below the control
/// policy; public scheduling only creates them from admitted faults.
fn raw(driver: &mut Driver<QueueMachine>, at: Timestamp, target: MachineId, payload: Request) {
    driver
        .execution
        .events
        .entry(at)
        .or_default()
        .push_back(Scheduled::Input {
            id: target,
            input: Input { at, payload },
        });
}
fn enqueue_to(driver: &mut Driver<QueueMachine>, at: Timestamp, target: MachineId, id: u32) {
    raw(
        driver,
        at,
        target,
        Request::Application {
            id: NonZeroU32::new(id).unwrap(),
            command: ApplicationCommand::Start,
        },
    );
}
#[test]
fn unconsumed_due_input_survives_jobs_without_perpetual_runnable_work() {
    let mut machine = QueueMachine::new();
    machine.consume = false;
    let mut driver = driver(vec![machine]);
    enqueue(&mut driver, at(0), 1);
    driver.step(at(10)).unwrap();
    let calls = driver.execution.machines[0].machine.as_ref().unwrap().calls;
    assert_eq!(driver.execution.machines[0].inbox.len(), 1);
    assert!(matches!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Blocked { .. }
    ));
    assert_eq!(
        driver.execution.machines[0].machine.as_ref().unwrap().calls,
        calls
    );
}
#[test]
fn ordered_reboot_discards_only_admitted_work_and_preserves_future_input() {
    let mut machine = QueueMachine::new();
    machine.consume = false;
    let mut driver = driver(vec![machine]);
    enqueue(&mut driver, at(0), 1);
    raw(&mut driver, at(0), MachineId(0), Request::Reboot);
    enqueue(&mut driver, at(0), 2);
    enqueue(&mut driver, at(5), 3);
    driver.step(at(0)).unwrap();
    assert_eq!(
        driver.execution.machines[0]
            .machine
            .as_ref()
            .unwrap()
            .accepted,
        [(1, 2)]
    );
    assert!(driver.execution.events.contains_key(&at(5)));
    driver.step(at(10)).unwrap();
    assert_eq!(
        driver.execution.machines[0]
            .machine
            .as_ref()
            .unwrap()
            .accepted,
        [(1, 2), (1, 3)]
    );
}
#[test]
fn common_barrier_sorts_worker_outputs_by_identity_then_emission_order() {
    let mut first = QueueMachine::new();
    first.emit = true;
    let second = first.clone();
    let (gate, entered) = handshake();
    first.entered = Some(Arc::new(Mutex::new(Some(entered))));
    let gate = Arc::new(Mutex::new(Some(gate)));
    let (published, completion) = mpsc::channel();
    let mut driver = driver(vec![first, second]);
    driver.workers.result_observer = Some(Arc::new(move |machine| {
        if machine == 1 {
            let gate = gate.lock().unwrap().take().unwrap();
            gate.wait_entered(&HangGuard::new(
                "slow worker must enter before fast result releases it",
                Duration::from_secs(30),
            ))
            .unwrap();
            published.send(machine).unwrap();
            gate.proceed().unwrap();
        } else {
            published.send(machine).unwrap();
        }
    }));
    driver.step(at(10)).unwrap();
    let guard = HangGuard::new(
        "both published worker results must be witnessed",
        Duration::from_secs(30),
    );
    assert_eq!(
        [
            guard.recv(&completion).unwrap(),
            guard.recv(&completion).unwrap()
        ],
        [1, 0],
        "actual result publication must be reverse identity order"
    );
    assert_eq!(
        fixture::outputs(&driver.drain_events())
            .iter()
            .map(|output| (output.machine.0, output.at, output.payload.clone()))
            .collect::<Vec<_>>(),
        vec![
            (0, at(10), Response::ApplicationOutcome { code: 1 }),
            (0, at(10), Response::ApplicationOutcome { code: 2 }),
            (1, at(10), Response::ApplicationOutcome { code: 1 }),
            (1, at(10), Response::ApplicationOutcome { code: 2 })
        ]
    );
}
#[test]
fn output_after_returned_stop_poison_discards_entire_batch() {
    let mut invalid = QueueMachine::new();
    invalid.emit = true;
    invalid.invalid_output = true;
    let mut valid = QueueMachine::new();
    valid.emit = true;
    let mut driver = driver(vec![invalid, valid]);
    assert!(
        matches!(driver.step(at(10)), Err(Error::InvalidOutput { id: MachineId(0), at: emitted, reached }) if emitted == at(10) && reached == at(0))
    );
    assert!(fixture::outputs(&driver.drain_events()).is_empty());
    assert!(driver
        .execution
        .machines
        .iter()
        .all(|entry| entry.staged.is_empty()));
    assert!(matches!(driver.step(at(10)), Err(Error::Failed)));
}
#[test]
fn later_batch_failure_discards_earlier_uncommitted_outputs() {
    let mut machine = QueueMachine::new();
    machine.fail_second = true;
    let mut driver = driver(vec![machine]);
    // First local run acknowledges an input at zero, then sleeps until one;
    // the second run fails before any common boundary is committed.
    driver
        .execution
        .control
        .offered(
            MachineId(0),
            NonZeroU32::new(1).unwrap(),
            crate::oracle::service::CommandKind::Application(ApplicationCommand::Start),
            crate::oracle::observation::Origin::External,
        )
        .unwrap();
    driver.execution.machines[0].inbox.push_back(Input {
        at: at(0),
        payload: Request::Application {
            id: NonZeroU32::new(1).unwrap(),
            command: ApplicationCommand::Start,
        },
    });
    assert!(matches!(driver.step(at(10)), Err(Error::Machine { .. })));
    assert!(fixture::outputs(&driver.drain_events()).is_empty());
    assert!(driver.execution.machines[0].staged.is_empty());
}

#[test]
fn normal_exit_discards_ended_boot_inbox_but_keeps_future_timeline() {
    let mut machine = QueueMachine::new();
    machine.consume = false;
    machine.exit = true;
    let mut driver = driver(vec![QueueMachine::new(), machine]);
    enqueue_to(&mut driver, at(0), MachineId(1), 1);
    enqueue_to(&mut driver, at(0), MachineId(1), 2);
    enqueue_to(&mut driver, at(4), MachineId(1), 3);
    raw(&mut driver, at(5), MachineId(1), Request::Reboot);
    enqueue_to(&mut driver, at(5), MachineId(1), 4);
    driver.step(at(0)).unwrap();
    assert!(driver.execution.machines[1].inbox.is_empty());
    assert!(matches!(
        driver.execution.machines[1].stopped,
        StopReason::Exited { .. }
    ));
    assert!(driver.execution.events.contains_key(&at(5)));
    driver.step(at(4)).unwrap();
    assert!(driver.execution.machines[1].inbox.is_empty());
    driver.step(at(10)).unwrap();
    assert_eq!(
        driver.execution.machines[1]
            .machine
            .as_ref()
            .unwrap()
            .accepted,
        [(1, 4)]
    );
    assert!(driver.execution.machines[1].inbox.is_empty());
}

impl QueueMachine {
    fn crash(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
    fn reboot(&mut self, _: Timestamp) -> Result<(), std::io::Error> {
        self.boot += 1;
        self.consume = true;
        self.exit = false;
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}

#[test]
fn boot_generation_overflow_fails_before_reset_or_response_publication() {
    let mut driver = driver(vec![QueueMachine::new()]);
    driver.execution.machines[0]
        .lifecycle
        .admission(
            0,
            0,
            0,
            crate::oracle::observation::Admission::Applied(
                crate::oracle::observation::Transition::Reboot { new_boot: u64::MAX },
            ),
        )
        .unwrap();
    raw(&mut driver, at(0), MachineId(0), Request::Reboot);
    assert!(matches!(
        driver.step(at(0)),
        Err(Error::BootOverflow { id: MachineId(0) })
    ));
    assert_eq!(
        driver.execution.machines[0].machine.as_ref().unwrap().boot,
        0
    );
    assert!(fixture::outputs(&driver.drain_events()).is_empty());
    assert!(matches!(driver.step(at(0)), Err(Error::Failed)));
}

#[test]
fn public_inputs_enqueue_in_fifo_order_without_admission_or_reservation() {
    let mut driver = driver(vec![QueueMachine::new()]);
    let id = MachineId(0);
    let actions = [
        Action::Pause(Target::Machine(id)),
        Action::Resume(Target::Machine(id)),
        Action::Crash(Target::Machine(id)),
        Action::Reboot(Target::Machine(id)),
        Action::Shutdown(Target::Machine(id)),
        Action::Stop {
            target: Target::Machine(id),
            deadline: Duration::from_nanos(5),
        },
    ];
    for action in actions.clone() {
        driver.schedule(at(0), auto_key(), action).unwrap();
    }
    let queued: Vec<_> = driver.execution.events[&at(0)]
        .iter()
        .map(|event| match event {
            Scheduled::Control(control::Event::Input { action, .. }) => action.clone(),
            _ => panic!("host input must enqueue as a control input"),
        })
        .collect();
    assert_eq!(queued, actions);
    // Enqueue performs no machine admission, lifecycle transition or ID
    // reservation, and publishes nothing.
    assert!(driver.execution.machines[0].inbox.is_empty());
    assert_eq!(
        driver.execution.machines[0].machine.as_ref().unwrap().calls,
        0
    );
    let (_, events) = (
        driver.execution.control.service.take_notices(),
        driver.drain_events(),
    );
    assert!(events
        .iter()
        .all(|event| matches!(event.kind, EventKind::MachineAdded { .. })));
}
