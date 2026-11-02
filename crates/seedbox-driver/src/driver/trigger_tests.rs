//! The oracle's block_request trigger: arming installs the machine's hold,
//! the reported hold crashes the machine at its commit, and expiry or
//! cancellation releases the hold. Owned mock machines stand in for PV.
use super::fixture::owned::*;
use super::*;
use crate::{
    oracle::triggers::Status,
    pv::{DiskOp, SimulationControlId},
};
use seedbox_spec::{backend::CpuBackend, definition::Architecture};

const PV: faults::Engine = faults::Engine {
    backend: CpuBackend::Pv,
    architecture: Architecture::Aarch64,
};

/// The oracle's call `id`, sent at `at`.
fn call(at: u64, id: u32, fields: &str) -> (u64, Response) {
    let payload = format!(r#"{{"version":1,"id":{id},{fields}}}"#);
    (
        at,
        Response::SimulationControl {
            id: SimulationControlId {
                session: std::num::NonZeroU32::new(1).unwrap(),
                request: std::num::NonZeroU32::new(id).unwrap(),
            },
            payload: payload.into_bytes(),
        },
    )
}
fn arm(at: u64, id: u32, op: &str, nth: u64, timeout: u64) -> (u64, Response) {
    call(
        at,
        id,
        &format!(
            r#""op":"arm_trigger","source":{{"machine":"fixture-1"}},"block_request":{{"op":"{op}","nth":{nth}}},"match_timeout_ns":{timeout},"fault":{{"action":"crash","target":{{"machine":"fixture-1"}}}}"#
        ),
    )
}
fn scenario(oracle: Vec<(u64, Response)>, held: Vec<(u64, Response)>) -> Driver<Mock> {
    let mut sends = vec![(0, Response::Ready)];
    sends.extend(oracle);
    let mut machine = vec![(0, Response::Ready)];
    machine.extend(held);
    build(
        vec![spec(0, true, sends), spec(1, true, machine)],
        vec![],
        true,
    )
}
fn received(d: &Driver<Mock>, machine: usize) -> Vec<Request> {
    states(d)[machine]
        .received
        .iter()
        .map(|input| input.payload.clone())
        .filter(|request| {
            matches!(
                request,
                Request::HoldBlockRequest { .. }
                    | Request::ReleaseBlockRequest { .. }
                    | Request::Crash
            )
        })
        .collect()
}
/// The replies the oracle received, as JSON.
fn replies(d: &Driver<Mock>) -> Vec<serde_json::Value> {
    states(d)[0]
        .received
        .iter()
        .filter_map(|input| match &input.payload {
            Request::SimulationReply { payload, .. } => serde_json::from_slice(payload).ok(),
            _ => None,
        })
        .collect()
}
fn resolutions(events: &[Event]) -> Vec<(u32, TriggerResult, Timestamp)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::TriggerResolved {
                trigger, result, ..
            } => Some((*trigger, *result, e.at)),
            _ => None,
        })
        .collect()
}
/// Faults a trigger admitted: (trigger, action, admission time).
fn caused(events: &[Event]) -> Vec<(u32, FaultAction, Timestamp)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(FaultRecord {
                origin: FaultOrigin::Trigger { trigger },
                action,
                ..
            }) => Some((*trigger, *action, e.at)),
            _ => None,
        })
        .collect()
}

#[test]
fn a_held_block_request_crashes_its_machine_at_the_commit_that_reports_it() {
    let mut d = scenario(
        vec![arm(0, 1, "flush", 3, 1_000)],
        vec![(13, Response::BlockRequestHeld { trigger: 1 })],
    );
    d.set_engine(PV);
    let events = advance(&mut d, 40);
    assert_eq!(
        received(&d, 1),
        [
            Request::HoldBlockRequest {
                trigger: 1,
                op: DiskOp::Flush,
                nth: 3,
                until: Some(at(1_010)),
            },
            Request::Crash
        ]
    );
    // The call is admitted at the first commit, 10, so the hold lasts until
    // 1010. Rounds end every 10 ns: the hold at 13 commits, and crashes, at
    // 20.
    assert_eq!(caused(&events), [(1, FaultAction::Crash, at(20))]);
    assert_eq!(resolutions(&events), [(1, TriggerResult::Fired, at(20))]);
    let record = &d.execution.control.service.triggers().records()[0];
    assert_eq!(
        (record.status, record.matched_at, record.effect_at),
        (Status::Fired, Some(13), Some(20))
    );
    assert_eq!(replies(&d)[0]["result"]["trigger_id"], 1);
}

#[test]
fn an_expired_or_cancelled_block_request_trigger_releases_its_hold() {
    let mut d = scenario(
        vec![
            arm(0, 1, "any", 1, 15),
            arm(0, 2, "write", 2, 1_000),
            call(40, 3, r#""op":"cancel_trigger","trigger_id":2"#),
        ],
        vec![],
    );
    d.set_engine(PV);
    let events = advance(&mut d, 60);
    assert_eq!(
        received(&d, 1),
        [
            Request::HoldBlockRequest {
                trigger: 1,
                op: DiskOp::Any,
                nth: 1,
                until: Some(at(25)),
            },
            Request::HoldBlockRequest {
                trigger: 2,
                op: DiskOp::Write,
                nth: 2,
                until: Some(at(1_010)),
            },
            Request::ReleaseBlockRequest { trigger: 1 },
            Request::ReleaseBlockRequest { trigger: 2 },
        ]
    );
    assert_eq!(
        resolutions(&events)
            .into_iter()
            .map(|(id, result, _)| (id, result))
            .collect::<Vec<_>>(),
        [(1, TriggerResult::Expired), (2, TriggerResult::Cancelled)]
    );
    assert!(caused(&events).is_empty());
}

#[test]
fn engines_without_held_requests_refuse_the_trigger_naming_what_they_lack() {
    for engine in [
        None,
        Some(faults::Engine {
            backend: CpuBackend::Tcg,
            architecture: Architecture::X86_64,
        }),
    ] {
        let mut d = scenario(vec![arm(0, 1, "flush", 3, 1_000)], vec![]);
        if let Some(engine) = engine {
            d.set_engine(engine);
        }
        let events = advance(&mut d, 30);
        assert!(received(&d, 1).is_empty());
        assert!(d.execution.control.service.triggers().records().is_empty());
        assert!(!events
            .iter()
            .any(|e| matches!(e.kind, EventKind::TriggerArmed { .. })));
        let reply = &replies(&d)[0];
        assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("a block_request trigger needs held block requests"),
            "{message}"
        );
        if engine.is_some() {
            assert!(message.ends_with("which x86_64 TCG does not provide"));
        }
    }
}

#[test]
fn a_block_request_trigger_crashes_only_its_own_machine() {
    let mut d = scenario(
        vec![call(
            0,
            1,
            r#""op":"arm_trigger","source":{"machine":"fixture-1"},"block_request":{"op":"flush","nth":1},"match_timeout_ns":100,"fault":{"action":"crash","target":{"machine":"fixture-0"}}"#,
        )],
        vec![],
    );
    d.set_engine(PV);
    advance(&mut d, 30);
    assert!(received(&d, 1).is_empty());
    let reply = &replies(&d)[0];
    assert_eq!(reply["error"]["code"], "invalid_request", "{reply}");
    assert_eq!(
        reply["error"]["message"],
        "a block_request trigger crashes its own machine"
    );
}
