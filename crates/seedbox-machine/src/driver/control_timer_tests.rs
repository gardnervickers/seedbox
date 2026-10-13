//! Internal timer ownership: retiring policy work cannot delete external FIFO entries.
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
        1,
        SeededRng::new(0),
        || Ok(()),
        || Ok(vec![Park]),
    )
    .unwrap()
}
fn rpc(driver: &mut Driver<Park>, request: u32, fields: &str) {
    let payload = format!("{{\"version\":1,\"id\":{request},{fields}}}");
    driver
        .control
        .invoke(|service, view| {
            service.admit(
                view,
                "machine:0".into(),
                crate::pv::SimulationControlId {
                    session: NonZeroU32::new(1).unwrap(),
                    request: NonZeroU32::new(request).unwrap(),
                },
                0,
                0,
                payload.as_bytes(),
            )
        })
        .unwrap();
    driver.sync_trigger_marker();
}
fn arm(driver: &mut Driver<Park>, request: u32, timeout: u64) {
    rpc(
        driver,
        request,
        &format!(
            r#""op":"arm_trigger","source":{{"machine":"fixture-0"}},"marker":[120],"match_timeout_ns":{timeout},"fault":{{"action":"stop","target":{{"machine":"fixture-0"}}}}"#
        ),
    );
}
#[test]
fn cancellation_replaces_then_removes_only_obsolete_internal_markers() {
    let mut driver = driver();
    arm(&mut driver, 1, 10);
    arm(&mut driver, 2, 20);
    driver
        .schedule(
            at(10),
            Action::SetExecution {
                machine: MachineId(0),
                config: ExecutionConfig::default(),
            },
        )
        .unwrap();
    driver.schedule(at(20), Action::Wake(MachineId(0))).unwrap();
    rpc(&mut driver, 3, r#""op":"cancel_trigger","trigger_id":1"#);
    assert_eq!(driver.trigger_marker, Some(at(20)));
    assert!(matches!(
        driver.events[&at(10)].front(),
        Some(Scheduled::Driver(DriverEvent::SetExecution { .. }))
    ));
    assert_eq!(driver.events[&at(10)].len(), 1);
    rpc(&mut driver, 4, r#""op":"cancel_trigger","trigger_id":2"#);
    assert_eq!(driver.trigger_marker, None);
    assert_eq!(driver.events[&at(20)].len(), 1);
    assert!(matches!(
        driver.events[&at(20)].front(),
        Some(Scheduled::Driver(DriverEvent::Wake(_)))
    ));
}
#[test]
fn firing_and_expiring_a_trigger_retire_old_marker_and_schedule_next_deadline() {
    let mut driver = driver();
    arm(&mut driver, 1, 10);
    arm(&mut driver, 2, 20);
    driver.control.service.expire_triggers(10);
    driver.sync_trigger_marker();
    assert!(!driver.events.contains_key(&at(10)));
    assert_eq!(driver.trigger_marker, Some(at(20)));
    driver
        .control
        .service
        .committed_bytes("fixture-0", 11, b"x", 11)
        .unwrap();
    driver.clock.set(11);
    let plans = driver
        .control
        .invoke_plans(|service, view| service.trigger_effects(view, 11))
        .unwrap();
    driver.control_plans(plans).unwrap();
    driver.sync_trigger_marker();
    assert_eq!(driver.trigger_marker, None);
    assert!(!driver.events.contains_key(&at(20)));
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
        .schedule(at(100), Action::Wake(MachineId(0)))
        .unwrap();
    driver
        .control_plans(vec![Plan::Settled(Settlement {
            fault: 7,
            target: "fixture-0".into(),
            boot: 0,
            action: FaultAction::Effect(Effect::Process(crate::pv::ApplicationCommand::Stop)),
            requested_at: 0,
            admitted_at: 0,
            outcome_at: Some(1),
            committed_at: 1,
            status: SettlementStatus::Applied,
            handle: None,
            command: NonZeroU32::new(1),
            detail: "confirmed".into(),
        })])
        .unwrap();
    let events = &driver.events[&at(100)];
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0],
        Scheduled::Control(control::Event::Plan(Plan::Deadline { fault: 8, .. }))
    ));
    assert!(matches!(events[1], Scheduled::Driver(DriverEvent::Wake(_))));
}
