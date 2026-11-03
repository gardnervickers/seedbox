//! Commit-time property reduction, reply delivery and continuation.
use super::*;
use crate::driver::fixture::owned::*;
use seedbox_engine_protocol::{
    crash,
    property::{AssessmentStatus, Frame, Reply, DEFAULT_NO_CRASH},
};

fn property(session: u32, seq: u64, record: &str) -> Response {
    Response::Property {
        frame: Frame {
            session,
            seq,
            payload: record.as_bytes().to_vec(),
        },
    }
}

fn abort(command: &str) -> Response {
    Response::Crash {
        report: crash::Report {
            message: format!("{command}: potentially unexpected fatal signal 6."),
            signal: 6,
        },
    }
}

fn replies(state: &State) -> Vec<(u32, Reply)> {
    state
        .received
        .iter()
        .filter_map(|input| match &input.payload {
            Request::PropertyReply { session, payload } => {
                Some((session.get(), serde_json::from_slice(payload).unwrap()))
            }
            _ => None,
        })
        .collect()
}

fn driver() -> Driver<Mock> {
    let violate = r#"{"assert":{"id":"p","kind":"always","condition":false}}"#;
    build(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (3, property(1, 1, violate)),
                    (4, property(1, 2, r#"{"flush":{}}"#)),
                    (6, property(2, 1, r#"{"require":{"id":"failover"}}"#)),
                    (7, property(2, 2, r#"{"flush":{}}"#)),
                ],
            ),
            spec(1, true, vec![(0, Response::Ready), (5, abort("watchdog"))]),
        ],
        vec![],
        true,
    )
}

fn allow_watchdog(d: &mut Driver<Mock>) {
    d.allow_crashes(["fixture-1".to_string()].into()).unwrap();
}

#[test]
fn committed_records_reduce_and_requests_are_answered_after_them() {
    let mut d = driver();
    assert_eq!(d.properties(), None);
    allow_watchdog(&mut d);
    advance(&mut d, 30);
    let assessment = d.properties().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    // The allowed crash is not a violation; the guest's is, and the
    // requirement nobody evaluated is unmet.
    assert_eq!(assessment.violations.len(), 1);
    assert_eq!(assessment.violations[0].id, "p");
    assert_eq!(assessment.violations[0].example.origin.machine, "fixture-0");
    assert_eq!(assessment.unmet_requirements, ["failover"]);
    // Each flush is answered on its own session, with its own sequence.
    let delivered = replies(&states(&d)[0]);
    assert_eq!(
        delivered,
        [(1, Reply::Flush { seq: 2 }), (2, Reply::Flush { seq: 2 })]
    );
    assert!(replies(&states(&d)[1]).is_empty());
    // Allowed crashes judge every piece of evidence alike, so they are
    // fixed before execution.
    assert!(matches!(
        d.allow_crashes(Default::default()),
        Err(Error::Properties(_))
    ));
}

#[test]
fn undeclared_machines_and_crashes_that_are_not_allowed() {
    let mut d = driver();
    assert!(matches!(
        d.allow_crashes(["nobody".to_string()].into()),
        Err(Error::UnknownName(_))
    ));
    advance(&mut d, 30);
    // Without allowCrashes the watchdog's abort violates the default property.
    let assessment = d.properties().unwrap();
    let crash = assessment
        .violations
        .iter()
        .find(|v| v.id == DEFAULT_NO_CRASH)
        .unwrap();
    assert_eq!(crash.example.origin.machine, "fixture-1");
}

#[test]
fn a_reply_for_an_ended_boot_is_dropped() {
    let mut d = driver();
    advance(&mut d, 10);
    let delivered = replies(&states(&d)[0]).len();
    let now = d.now();
    d.execution
        .events
        .entry(now)
        .or_default()
        .push_back(Scheduled::PropertyReply {
            machine: MachineId(0),
            boot: 9,
            session: nz(1),
            payload: br#"{"flush":{"seq":9}}"#.to_vec(),
        });
    advance(&mut d, 30);
    let after = replies(&states(&d)[0]);
    assert_eq!(after.len(), delivered, "{after:?}");
    assert!(!after.contains(&(1, Reply::Flush { seq: 9 })));
}

#[test]
fn checkpoints_retain_the_ledger_and_allowed_crashes() {
    let mut d = driver();
    allow_watchdog(&mut d);
    advance(&mut d, 4);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    advance(&mut d, 30);
    let expected = (d.properties(), states(&d));
    for workers in [1, 2] {
        let mut branch = restore(&decode(&bytes, &store), workers);
        advance(&mut branch, 30);
        assert_eq!((branch.properties(), states(&branch)), expected);
    }
}

/// The oracle's exit ends the run at the commit of its round: every
/// machine's records committed in that round are reduced, and nothing after
/// it is relayed at all, so no committed record is ever dropped.
#[test]
fn records_committed_with_the_oracles_exit_are_reduced() {
    let violate =
        |id: &str| format!(r#"{{"assert":{{"id":"{id}","kind":"never","condition":true}}}}"#);
    let mut d = build(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (5, Response::ApplicationOutcome { code: 0 }),
                ],
            ),
            spec(
                1,
                true,
                vec![
                    (0, Response::Ready),
                    (7, property(1, 1, &violate("same round"))),
                    (15, property(1, 2, &violate("after the run"))),
                ],
            ),
        ],
        vec![],
        true,
    );
    advance(&mut d, 30);
    assert!(d.execution.finished.is_some());
    let ids: Vec<_> = d
        .properties()
        .unwrap()
        .violations
        .into_iter()
        .map(|v| v.id)
        .collect();
    assert_eq!(ids, ["same round"]);
}

/// A real crash and reboot through the Driver's lifecycle: the new boot
/// numbers its sessions from 1 again, which is not a sequence error, and its
/// flush is answered on the new boot.
#[test]
fn a_rebooted_machine_starts_its_sessions_afresh() {
    let violate = r#"{"assert":{"id":"booted","kind":"never","condition":true}}"#;
    let mut d = build(
        vec![
            spec(0, true, vec![(0, Response::Ready)]),
            spec(
                1,
                true,
                vec![
                    (0, Response::Ready),
                    (3, property(1, 1, violate)),
                    // Delivered by the second boot: session 1 again.
                    (25, property(1, 1, violate)),
                    (26, property(1, 2, r#"{"flush":{}}"#)),
                ],
            ),
        ],
        vec![],
        true,
    );
    d.schedule(
        at(10),
        key("crash"),
        Action::Crash(Target::Machine(MachineId(1))),
    )
    .unwrap();
    d.schedule(
        at(12),
        key("reboot"),
        Action::Reboot(Target::Machine(MachineId(1))),
    )
    .unwrap();
    advance(&mut d, 40);
    assert_eq!(d.execution.machines[1].lifecycle.boot(), 1);
    let assessment = d.properties().unwrap();
    assert_eq!(assessment.lost_count, 0, "{assessment:?}");
    let booted = &assessment.violations[0];
    assert_eq!(booted.id, "booted");
    // The first violation, from the first boot, is the example.
    assert_eq!(booted.example.origin.incarnation, 0);
    assert_eq!(replies(&states(&d)[1]), [(1, Reply::Flush { seq: 2 })]);
}
