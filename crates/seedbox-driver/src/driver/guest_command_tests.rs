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

fn stop(nanos: u64) -> Action {
    Action::Stop {
        target: target(),
        deadline: deadline(nanos),
    }
}
fn restart(nanos: u64) -> Action {
    Action::Restart {
        target: target(),
        deadline: deadline(nanos),
    }
}
fn application(d: &Driver<Mock>) -> Vec<crate::pv::ApplicationCommand> {
    commands(d)
        .into_iter()
        .filter_map(|request| match request {
            Request::Application { command, .. } => Some(command),
            _ => None,
        })
        .collect()
}
fn rejected(events: &[Event], name: &str) -> Option<InputRejection> {
    events.iter().find_map(|event| match &event.kind {
        EventKind::InputRejected { key, rejection } if key.as_str() == name => {
            Some(rejection.clone())
        }
        _ => None,
    })
}
fn admitted(events: &[Event], name: &str) -> FaultId {
    events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::FaultAdmitted(FaultRecord {
                fault,
                origin: FaultOrigin::Input { key },
                ..
            }) if key.as_str() == name => Some(*fault),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{name} was not admitted"))
}

/// A restart requested while its stop is still being reaped is admitted and
/// follows the stop, as the supervisor carries out commands in order; a
/// process pause requested behind that stop is refused.
#[test]
fn a_restart_requested_before_its_stop_lands_follows_it() {
    use crate::pv::ApplicationCommand::{Start, Stop};
    let mut d = driver(vec![]);
    input(&mut d, 10, "stop", stop(100));
    let pause = Action::PauseProcess {
        target: target(),
        deadline: deadline(100),
    };
    input(&mut d, 11, "pause", pause);
    // The guest acknowledges the stop at 13.
    input(&mut d, 12, "restart", restart(100));
    let events = advance(&mut d, 100);
    assert_eq!(rejected(&events, "restart"), None);
    assert_eq!(settlement(&events, "stop").0, SettlementStatus::Applied);
    assert_eq!(settlement(&events, "restart").0, SettlementStatus::Applied);
    assert_eq!(
        rejected(&events, "pause").map(|r| r.reason),
        Some(RejectionReason::InvalidState)
    );
    assert_eq!(application(&d), [Stop, Start]);
    assert_eq!(
        d.execution.machines[1].lifecycle.facts().unwrap().process,
        crate::oracle::service::Process::Running
    );
}

/// A restart queued behind a stop that never reaches a rebooted machine is
/// never sent alone: it is unavailable with the stop, and readiness
/// afterwards sends nothing.
#[test]
fn a_restart_behind_an_undelivered_stop_is_unavailable_with_it() {
    let mut d = driver(vec![(200, Response::Ready)]);
    reboot(&mut d);
    input(&mut d, 30, "stop", stop(100));
    input(&mut d, 50, "restart", restart(300));
    let events = advance(&mut d, 400);
    let fault = admitted(&events, "stop");
    assert_eq!(settlement(&events, "stop").0, SettlementStatus::Unavailable);
    assert_eq!(
        settlement(&events, "restart"),
        (
            SettlementStatus::Unavailable,
            format!(
                "sequenced behind fault {}, which was not delivered",
                fault.0
            )
        )
    );
    assert!(commands(&d).is_empty());
}

fn skew(nanos: i64) -> Action {
    Action::StepRealtime {
        target: target(),
        nanoseconds: std::num::NonZeroI64::new(nanos).unwrap(),
    }
}
fn heal(name: &str) -> Action {
    Action::Heal(HealTarget::Input(key(name)))
}
fn steps(d: &Driver<Mock>) -> Vec<i64> {
    commands(d)
        .into_iter()
        .filter_map(|request| match request {
            Request::StepRealtime { nanoseconds, .. } => Some(nanoseconds.get()),
            _ => None,
        })
        .collect()
}
/// A heal requested before its skew is acknowledged is admitted and sent
/// once the skew lands, so the guest steps and then steps back; a second
/// heal of that skew is refused while the first is outstanding.
#[test]
fn a_heal_requested_before_its_skew_lands_follows_it() {
    let mut d = driver(vec![]);
    input(&mut d, 10, "skew", skew(7));
    // The guest acknowledges the skew at 13.
    input(&mut d, 11, "heal", heal("skew"));
    input(&mut d, 12, "again", heal("skew"));
    let events = advance(&mut d, 100);
    assert_eq!(rejected(&events, "heal"), None);
    assert_eq!(settlement(&events, "skew").0, SettlementStatus::Applied);
    assert_eq!(settlement(&events, "heal").0, SettlementStatus::Applied);
    assert_eq!(
        rejected(&events, "again").map(|r| r.reason),
        Some(RejectionReason::InvalidState)
    );
    assert_eq!(steps(&d), [7, -7]);
    // The skew is healed: no handle remains to heal.
    input(&mut d, 150, "late", heal("skew"));
    let events = advance(&mut d, 200);
    assert_eq!(
        rejected(&events, "late").map(|r| r.reason),
        Some(RejectionReason::InvalidState)
    );
}

/// A skew held for readiness and a heal requested before readiness: when
/// readiness comes in time the skew is sent first and the heal follows it.
#[test]
fn a_skew_held_for_readiness_is_sent_before_its_queued_heal() {
    let mut d = driver(vec![(60, Response::Ready)]);
    reboot(&mut d);
    input(&mut d, 30, "skew", skew(7));
    input(&mut d, 31, "heal", heal("skew"));
    let events = advance(&mut d, 200);
    assert_eq!(rejected(&events, "heal"), None);
    assert_eq!(settlement(&events, "skew").0, SettlementStatus::Applied);
    assert_eq!(settlement(&events, "heal").0, SettlementStatus::Applied);
    assert_eq!(steps(&d), [7, -7]);
}

/// A crash before the skew lands ends the skew and the heal queued behind
/// it; the inverse step is never sent.
#[test]
fn a_crash_before_the_skew_lands_ends_its_queued_heal() {
    let mut d = driver(vec![]);
    input(&mut d, 10, "skew", skew(7));
    input(&mut d, 11, "heal", heal("skew"));
    input(&mut d, 12, "crash", Action::Crash(target()));
    let events = advance(&mut d, 100);
    assert_eq!(settlement(&events, "skew").0, SettlementStatus::Unavailable);
    assert_eq!(settlement(&events, "heal").0, SettlementStatus::Unavailable);
    assert_eq!(steps(&d), [7]);
}

/// Saved with a heal queued behind its unacknowledged skew, a restored
/// Driver sends the undo once the skew lands.
#[test]
fn a_heal_queued_behind_its_skew_survives_a_checkpoint() {
    let mut d = driver(vec![]);
    input(&mut d, 10, "skew", skew(7));
    input(&mut d, 11, "heal", heal("skew"));
    advance(&mut d, 12);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    drop(d);
    let mut d = restore(&decode(&bytes, &store), 1);
    let events = advance(&mut d, 100);
    assert_eq!(settlement(&events, "skew").0, SettlementStatus::Applied);
    assert_eq!(settlement(&events, "heal").0, SettlementStatus::Applied);
    assert_eq!(steps(&d), [7, -7]);
}

/// A skew and the heal queued behind it, both for a rebooted machine that
/// is not ready in time: the skew is unavailable at its deadline and takes
/// the heal with it, and readiness afterwards sends nothing.
#[test]
fn an_undelivered_skew_takes_its_queued_heal_with_it() {
    let deadline = 30 + crate::oracle::PROCESS_EFFECT_TIMEOUT_NS;
    // A long quantum reaches the default deadline in few steps.
    let mut d = super::fixture::driver(
        at(0),
        Duration::from_millis(100),
        1,
        SeededRng::new(51),
        || Ok(Context(None)),
        || {
            Ok(vec![
                Mock::new(spec(0, true, vec![(0, Response::Ready)])),
                Mock::new(spec(
                    1,
                    true,
                    vec![(0, Response::Ready), (deadline + 100, Response::Ready)],
                )),
            ])
        },
    )
    .unwrap();
    reboot(&mut d);
    input(&mut d, 30, "skew", skew(7));
    input(&mut d, 40, "heal", heal("skew"));
    let events = advance(&mut d, deadline + 500);
    let fault = admitted(&events, "skew");
    assert_eq!(
        settlement(&events, "skew"),
        (
            SettlementStatus::Unavailable,
            "target machine was not ready again in time".into()
        )
    );
    assert_eq!(
        settlement(&events, "heal"),
        (
            SettlementStatus::Unavailable,
            format!("sequenced behind fault {}, which was not applied", fault.0)
        )
    );
    assert!(steps(&d).is_empty());
}
