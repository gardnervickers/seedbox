//! Internal timer ownership: retiring policy work cannot delete external FIFO entries.
use super::fixture::auto_key;
use super::*;
use crate::oracle::service::{Effect, FaultAction, Plan, Settlement, SettlementStatus};
#[derive(Clone)]
struct Park;
impl Machine for Park {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(&mut self, _: &mut RunContext<'_, ()>, _: Timestamp) -> Result<StopReason, Self::Error> {
        unreachable!()
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn driver() -> Driver<Park> {
    fixture::driver(
        at(0),
        Duration::from_nanos(100),
        SeededRng::new(0),
        || Ok(()),
        || Ok(vec![Park]),
    )
    .unwrap()
}
#[test]
fn settlement_retires_its_deadline_without_deleting_other_policy_or_external_work() {
    let mut driver = driver();
    driver
        .control_plans(vec![
            Plan::Deadline { fault: 7, at: 100 },
            Plan::Deadline { fault: 8, at: 100 },
        ])
        .unwrap();
    driver
        .schedule(
            at(100),
            auto_key(),
            Action::Pause(Target::Machine(MachineId(0))),
        )
        .unwrap();
    driver
        .control_plans(vec![Plan::Settled(
            Settlement {
                record_id: None,
                network: None,
                counts: None,
                fault: 7,
                requester: crate::oracle::service::Requester::Input("stop".into()),
                scope: crate::oracle::service::Scope::Machine {
                    name: "fixture-0".into(),
                    boot: 0,
                },
                action: FaultAction::Effect(Effect::Process(crate::pv::ApplicationCommand::Stop)),
                requested_at: 0,
                admitted_at: 0,
                outcome_at: Some(1),
                committed_at: 1,
                status: SettlementStatus::Applied,
                handle: None,
                command: NonZeroU32::new(1),
                detail: "confirmed".into(),
            }
            .into(),
        )])
        .unwrap();
    let events = &driver.execution.events[&at(100)];
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        Scheduled::Control(control::Event::Plan(Plan::Deadline { fault: 8, .. }))
    ));
    assert!(matches!(
        events[1],
        Scheduled::Control(control::Event::Input {
            action: Action::Pause(_),
            ..
        })
    ));
}
