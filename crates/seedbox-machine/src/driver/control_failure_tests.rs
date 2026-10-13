use super::fixture::auto_key;
use super::*;

#[derive(Clone)]
struct Probe {
    role: usize,
    sent: bool,
    fail: bool,
}
impl Machine for Probe {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = Timestamp::from_nanos(c.clock.now());
        while let Some(input) = c.inbox.take() {
            if matches!(input.payload, Request::Crash | Request::Reboot) && self.fail {
                return Err(std::io::Error::other("uncertain native reset"));
            }
        }
        if !self.sent && upto > now && (self.role == 1 || self.role == 2) {
            self.sent = true;
            let payload = if self.role == 1 {
                r#"{"version":1,"id":1,"op":"throttle","target":{"machine":"fixture-3"},"factor_permille":2000}"#
            } else {
                r#"{"version":1,"id":1,"op":"crash","target":{"machine":"fixture-3"}}"#
            };
            c.outbox
                .emit(Output {
                    at: now,
                    payload: Response::SimulationControl {
                        id: crate::pv::SimulationControlId {
                            session: NonZeroU32::new(1).unwrap(),
                            request: NonZeroU32::new(1).unwrap(),
                        },
                        payload: payload.as_bytes().to_vec(),
                    },
                })
                .unwrap();
        }
        Ok(StopReason::Blocked {
            reached: upto,
            deadline: None,
        })
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
#[test]
fn uncertain_effect_retains_prior_settlement_and_never_claims_applied() {
    let mut driver = fixture::driver(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        2,
        SeededRng::new(0),
        || Ok(()),
        || {
            Ok((0..4)
                .map(|role| Probe {
                    role,
                    sent: false,
                    fail: role == 3,
                })
                .collect())
        },
    )
    .unwrap();
    driver
        .schedule(
            Timestamp::from_nanos(0),
            auto_key(),
            Action::Pause(Target::Machine(MachineId(0))),
        )
        .unwrap();
    assert!(matches!(
        driver.step(Timestamp::from_nanos(10)),
        Err(Error::Machine {
            id: MachineId(3),
            ..
        })
    ));
    // Drainable after failure, including the failure-path settlement.
    let events = driver.drain_events();
    let settlements = fixture::settlements(&events);
    assert_eq!(settlements.len(), 3);
    // The host pause is itself an admitted, applied fault.
    assert!(matches!(settlements[0].0.origin, FaultOrigin::Input { .. }));
    assert_eq!(settlements[0].1, SettlementStatus::Applied);
    assert_eq!(settlements[1].1, SettlementStatus::Applied);
    assert_eq!(settlements[2].1, SettlementStatus::Failed);
    assert_eq!(settlements[2].2, None);
    assert_eq!(driver.machines[3].execution.factor_permille(), 2000);
    assert_eq!(
        fixture::outputs(&events)
            .iter()
            .filter(|output| matches!(output.payload, Response::SimulationControl { .. }))
            .count(),
        2
    );
    assert!(matches!(
        driver.step(Timestamp::from_nanos(10)),
        Err(Error::Failed)
    ));
}

#[derive(Clone)]
struct AckBatchProbe {
    role: usize,
    ready: bool,
    called: bool,
    pending: Option<NonZeroU32>,
    final_batch: bool,
    bad_ack: bool,
}
impl Machine for AckBatchProbe {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = Timestamp::from_nanos(c.clock.now());
        while let Some(input) = c.inbox.take() {
            if let Request::Application {
                id,
                command: crate::pv::ApplicationCommand::Stop,
            } = input.payload
            {
                assert_eq!(self.role, 2);
                self.pending = Some(id);
            }
        }
        if upto > now {
            if !self.ready {
                self.ready = true;
                c.outbox
                    .emit(Output {
                        at: now,
                        payload: Response::Ready,
                    })
                    .unwrap();
            }
            if self.role == 1 && !self.called {
                self.called = true;
                c.outbox.emit(Output { at: now, payload: Response::SimulationControl {
                    id: crate::pv::SimulationControlId { session: NonZeroU32::new(1).unwrap(), request: NonZeroU32::new(1).unwrap() },
                    payload: br#"{"version":1,"id":1,"op":"stop","target":{"machine":"fixture-2"}}"#.to_vec(),
                }}).unwrap();
            } else if self.role == 1 && self.final_batch && now.as_nanos() == 10 {
                c.outbox
                    .emit(Output {
                        at: Timestamp::from_nanos(13),
                        payload: Response::SimulationControl {
                            id: crate::pv::SimulationControlId {
                                session: NonZeroU32::new(1).unwrap(),
                                request: NonZeroU32::new(2).unwrap(),
                            },
                            payload: br#"{"version":1,"id":2,"op":"now"}"#.to_vec(),
                        },
                    })
                    .unwrap();
            }
            if let Some(id) = self.pending.take() {
                c.outbox
                    .emit(Output {
                        at: Timestamp::from_nanos(11),
                        payload: Response::CommandAcknowledged {
                            id,
                            result: crate::pv::CommandResult::Completed,
                        },
                    })
                    .unwrap();
                if self.bad_ack {
                    c.outbox
                        .emit(Output {
                            at: Timestamp::from_nanos(12),
                            payload: Response::CommandAcknowledged {
                                id: NonZeroU32::new(id.get() + 100).unwrap(),
                                result: crate::pv::CommandResult::Completed,
                            },
                        })
                        .unwrap();
                }
            }
            if self.role == 0 && self.final_batch && now.as_nanos() == 10 {
                c.outbox
                    .emit(Output {
                        at: Timestamp::from_nanos(15),
                        payload: Response::ApplicationOutcome { code: 0 },
                    })
                    .unwrap();
            }
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
fn ack_batch(final_batch: bool, bad_ack: bool) -> Driver<AckBatchProbe> {
    let mut driver = fixture::driver(
        Timestamp::from_nanos(0),
        Duration::from_nanos(10),
        2,
        SeededRng::new(0),
        || Ok(()),
        move || {
            Ok((0..3)
                .map(|role| AckBatchProbe {
                    role,
                    ready: false,
                    called: false,
                    pending: None,
                    final_batch,
                    bad_ack,
                })
                .collect())
        },
    )
    .unwrap();
    assert_eq!(
        driver.step(Timestamp::from_nanos(10)).unwrap(),
        StepOutcome::BarrierReached {
            at: Timestamp::from_nanos(10)
        }
    );
    let events = driver.drain_events();
    assert!(fixture::settlements(&events).is_empty());
    // The Stop fault was admitted at the first barrier.
    assert!(events.iter().any(|event| event.at == Timestamp::from_nanos(10)
        && matches!(&event.kind, EventKind::FaultAdmitted(record) if record.fault == FaultId(1))));
    assert!(driver.machines[2]
        .machine
        .as_ref()
        .unwrap()
        .pending
        .is_some());
    driver
}
fn assert_ack_settlement(driver: &Driver<AckBatchProbe>, events: &[Event]) {
    let settlements = fixture::settlements(events);
    assert_eq!(settlements.len(), 1);
    let (record, status, outcome_at, committed_at) = &settlements[0];
    assert_eq!(record.fault, FaultId(1));
    assert_eq!(record.target, MachineId(2));
    assert_eq!(record.boot, 0);
    assert_eq!(
        record.action,
        FaultAction::Application(crate::pv::ApplicationCommand::Stop)
    );
    assert_eq!(*status, SettlementStatus::Applied);
    assert_eq!(*outcome_at, Some(Timestamp::from_nanos(11)));
    assert_eq!(*committed_at, Timestamp::from_nanos(20));
    // An immutable receipt is not permission to enqueue the reply or another
    // effect before the entire committed batch finishes control processing.
    assert!(!driver.events.values().flatten().any(|event| matches!(
        event,
        Scheduled::Control(control::Event::Plan(
            crate::oracle::service::Plan::Reply { .. }
                | crate::oracle::service::Plan::Effect { .. }
        )) | Scheduled::Control(control::Event::Call { .. })
    )));
}
#[test]
fn earlier_ack_settlement_survives_later_bad_ack_in_same_committed_batch() {
    let mut driver = ack_batch(false, true);
    assert!(matches!(
        driver.step(Timestamp::from_nanos(20)),
        Err(Error::Control(_))
    ));
    let events = driver.drain_events();
    assert_ack_settlement(&driver, &events);
    assert_eq!(
        fixture::outputs(&events)
            .iter()
            .filter(|output| matches!(output.payload, Response::CommandAcknowledged { .. }))
            .count(),
        2
    );
    assert!(driver.machines.iter().all(|entry| entry.staged.is_empty()));
    assert!(matches!(
        driver.step(Timestamp::from_nanos(20)),
        Err(Error::Failed)
    ));
    assert!(driver.drain_events().is_empty());
}
#[test]
fn final_oracle_batch_retains_confirmed_peer_settlement_without_new_dispatch() {
    let mut driver = ack_batch(true, false);
    let finished = StepOutcome::Finished {
        at: Timestamp::from_nanos(20),
        reason: ExitReason::ProgramExit { code: 0 },
    };
    assert_eq!(driver.step(Timestamp::from_nanos(20)).unwrap(), finished);
    let events = driver.drain_events();
    assert_ack_settlement(&driver, &events);
    assert!(fixture::outputs(&events).iter().any(|output| matches!(output.payload, Response::SimulationControl { id, .. } if id.request.get() == 2)));
    assert_eq!(driver.step(Timestamp::from_nanos(30)).unwrap(), finished);
    assert!(driver.drain_events().is_empty());
}

#[test]
fn a_settlement_cannot_reenter_execution_and_duplicate_retention() {
    let mut driver = ack_batch(false, false);
    driver.step(Timestamp::from_nanos(20)).unwrap();
    assert_eq!(fixture::settlements(&driver.drain_events()).len(), 1);
    let settled = crate::oracle::service::Settlement {
        fault: 1,
        requester: crate::oracle::service::Requester::Trigger(1),
        target: "fixture-2".into(),
        boot: 0,
        action: crate::oracle::service::FaultAction::Heal,
        requested_at: 10,
        admitted_at: 10,
        outcome_at: Some(11),
        committed_at: 20,
        status: crate::oracle::service::SettlementStatus::Applied,
        handle: None,
        command: None,
        detail: String::new(),
    };
    driver
        .events
        .entry(Timestamp::from_nanos(20))
        .or_default()
        .push_back(Scheduled::Control(control::Event::Plan(
            crate::oracle::service::Plan::Settled(settled),
        )));
    assert!(
        matches!(driver.step(Timestamp::from_nanos(20)), Err(Error::Control(message)) if message == "settlement entered execution queue")
    );
    assert!(fixture::settlements(&driver.drain_events()).is_empty());
}
