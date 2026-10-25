//! Guest commands (process and realtime-step faults) waiting for readiness,
//! their deadlines and their order on one machine.
use super::fixture::owned::*;
use super::*;

/// An oracle and target `fixture-1`, each ready at 0; `sends` are the
/// target's further scripted outputs.
fn driver(sends: Vec<(u64, Response)>) -> Driver<Mock> {
    let mut target = vec![(0, Response::Ready)];
    target.extend(sends);
    build(
        vec![
            spec(0, true, vec![(0, Response::Ready)]),
            spec(1, true, target),
        ],
        vec![],
        true,
    )
}
fn target() -> Target {
    Target::Machine(MachineId(1))
}
fn deadline(nanos: u64) -> Duration {
    Duration::from_nanos(nanos)
}
fn input(d: &mut Driver<Mock>, time: u64, name: &str, action: Action) {
    d.schedule(at(time), key(name), action).unwrap();
}
/// Crash the target at 10 and reboot it at 20.
fn reboot(d: &mut Driver<Mock>) {
    input(d, 10, "crash", Action::Crash(target()));
    input(d, 20, "reboot", Action::Reboot(target()));
}
/// Each host input's settlement status and detail, in settlement order.
fn settled(events: &[Event]) -> Vec<(String, SettlementStatus, String)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::FaultSettled {
                record:
                    FaultRecord {
                        origin: FaultOrigin::Input { key },
                        ..
                    },
                status,
                detail,
                ..
            } => Some((key.to_string(), *status, detail.clone())),
            _ => None,
        })
        .collect()
}
fn settlement(events: &[Event], name: &str) -> (SettlementStatus, String) {
    let (_, status, detail) = settled(events)
        .into_iter()
        .find(|(key, ..)| key == name)
        .unwrap_or_else(|| panic!("{name} did not settle"));
    (status, detail)
}
/// The guest commands the target received, in order.
fn commands(d: &Driver<Mock>) -> Vec<Request> {
    states(d)[1]
        .received
        .iter()
        .filter(|input| {
            matches!(
                input.payload,
                Request::Application { .. } | Request::StepRealtime { .. }
            )
        })
        .map(|input| input.payload.clone())
        .collect()
}

/// A process fault for a rebooted machine that is not ready again by its
/// deadline is unavailable, as for a down machine, and the run goes on.
/// Readiness after the deadline sends nothing; later faults apply.
#[test]
fn a_rebooted_machine_not_ready_in_time_leaves_process_faults_unavailable() {
    let mut d = driver(vec![(400, Response::Ready)]);
    reboot(&mut d);
    let pause = Action::PauseProcess {
        target: target(),
        deadline: deadline(100),
    };
    input(&mut d, 30, "pause", pause);
    let events = advance(&mut d, 300);
    assert_eq!(
        settlement(&events, "pause"),
        (
            SettlementStatus::Unavailable,
            "target machine was not ready again in time".into()
        )
    );
    let stop = Action::Stop {
        target: target(),
        deadline: deadline(100),
    };
    input(&mut d, 450, "stop", stop);
    let events = advance(&mut d, 500);
    assert_eq!(settlement(&events, "stop").0, SettlementStatus::Applied);
    assert!(matches!(
        commands(&d)[..],
        [Request::Application {
            command: crate::pv::ApplicationCommand::Stop,
            ..
        }]
    ));
}

/// A process fault waiting for a first boot that never becomes ready still
/// fails the run at its deadline.
#[test]
fn a_first_boot_that_is_never_ready_fails_a_process_fault_at_its_deadline() {
    let mut d = build(
        vec![
            spec(0, true, vec![(0, Response::Ready)]),
            spec(1, true, vec![]),
        ],
        vec![],
        true,
    );
    let pause = Action::PauseProcess {
        target: target(),
        deadline: deadline(100),
    };
    input(&mut d, 10, "pause", pause);
    let error = loop {
        match d.step(at(300)) {
            Ok(_) if d.now() < at(300) => {}
            Ok(outcome) => panic!("run outlived the deadline: {outcome:?}"),
            Err(error) => break error,
        }
    };
    assert!(
        matches!(&error, Error::Control(message) if message.contains("deadline expired")),
        "{error:?}"
    );
    assert_eq!(
        settlement(&d.drain_events(), "pause").0,
        SettlementStatus::Failed
    );
}
