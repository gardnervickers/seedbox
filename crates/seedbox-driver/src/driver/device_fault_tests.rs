//! Disk and scheduler faults: Driver admission, handles, machine requests,
//! heal counts and continuation, with owned mock machines.
use super::fixture::owned::*;
use super::*;
use crate::pv::{DeviceFault, DiskEffect, DiskOp, DiskScope, FaultCounts};

fn target(index: usize) -> Target {
    Target::Name(format!("fixture-{index}"))
}
fn disk_error() -> DeviceFault {
    DeviceFault::DiskError {
        op: DiskOp::Write,
        ppm: 1_000,
        effect: DiskEffect::Eio,
        scope: DiskScope::All,
    }
}
fn handle(name: &str) -> FaultHandle {
    FaultHandle::named(name.to_owned()).unwrap()
}
fn fault(d: &mut Driver<Mock>, when: u64, name: &str, machine: usize, fault: DeviceFault) {
    d.with_barrier(|b| {
        b.schedule_with_handle(
            at(when),
            key(name),
            Action::DeviceFault {
                target: target(machine),
                fault,
            },
            handle(name),
        )
    })
    .unwrap();
}
fn heal(d: &mut Driver<Mock>, when: u64, name: &str) {
    d.schedule(
        at(when),
        key(&format!("heal-{name}")),
        Action::Heal(HealTarget::Handle(handle(name))),
    )
    .unwrap();
}
fn machines() -> Vec<State> {
    vec![
        spec(0, true, vec![(0, Response::Ready)]),
        spec(1, true, vec![(0, Response::Ready)]),
    ]
}
/// Settled faults among events: (fault, action, status, counts).
fn settled(events: &[Event]) -> Vec<(u32, FaultAction, SettlementStatus, Option<FaultCounts>)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultSettled {
                record,
                status,
                counts,
                ..
            } => Some((record.fault.get(), record.action, *status, *counts)),
            _ => None,
        })
        .collect()
}
fn rejected(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::InputRejected { key, .. } => Some(key.as_str().to_owned()),
            _ => None,
        })
        .collect()
}
/// Device fault requests a machine received, in order.
fn requests(d: &Driver<Mock>, machine: usize) -> Vec<Request> {
    states(d)[machine]
        .received
        .iter()
        .map(|input| input.payload.clone())
        .filter(|payload| {
            matches!(
                payload,
                Request::InstallFault { .. } | Request::HealFault { .. }
            )
        })
        .collect()
}

#[test]
fn a_device_fault_installs_in_its_machine_and_its_heal_settles_with_counts() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "eio", 1, disk_error());
    heal(&mut d, 15, "eio");
    let events = advance(&mut d, 30);
    let settled = settled(&events);
    let [(installed, FaultAction::DeviceFault(action), SettlementStatus::Applied, None), (_, FaultAction::Heal, SettlementStatus::Applied, Some(counts))] =
        settled.as_slice()
    else {
        panic!("{settled:?}")
    };
    assert_eq!(*action, disk_error());
    let requests = requests(&d, 1);
    let [Request::InstallFault { id, seed, fault }, Request::HealFault { id: healed }] =
        requests.as_slice()
    else {
        panic!("{requests:?}")
    };
    assert_eq!((*fault, healed), (disk_error(), id));
    assert_eq!(
        *seed,
        d.execution.control.fault_seed(MachineId(1), *installed)
    );
    assert_eq!(
        *counts,
        FaultCounts {
            matched: *id,
            affected: id / 2
        }
    );
    assert!(requests_of(&d, 0).is_empty(), "only the target receives it");
    // The heal released the handle and its input key.
    assert!(d.execution.inputs.is_empty());
}
fn requests_of(d: &Driver<Mock>, machine: usize) -> Vec<Request> {
    requests(d, machine)
}

#[test]
fn device_faults_apply_in_every_state_and_outlive_crash_and_reboot() {
    let mut d = build(machines(), vec![], true);
    d.schedule(at(2), key("crash"), Action::Crash(target(1)))
        .unwrap();
    fault(
        &mut d,
        4,
        "slow",
        1,
        DeviceFault::DiskLatency {
            op: DiskOp::Any,
            latency_ns: 1_000,
            jitter_ns: 10,
        },
    );
    d.schedule(at(6), key("reboot"), Action::Reboot(target(1)))
        .unwrap();
    heal(&mut d, 12, "slow");
    let events = advance(&mut d, 30);
    let settled = settled(&events);
    assert!(
        settled
            .iter()
            .all(|(_, _, status, _)| *status == SettlementStatus::Applied),
        "{settled:?}"
    );
    assert!(matches!(
        settled.last(),
        Some((_, FaultAction::Heal, _, Some(_)))
    ));
    assert_eq!(requests(&d, 1).len(), 2);
}

#[test]
fn one_preempt_fault_per_machine_while_disk_faults_stack() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "race", 1, DeviceFault::Preempt { ppm: 10 });
    fault(&mut d, 6, "again", 1, DeviceFault::Preempt { ppm: 20 });
    fault(&mut d, 6, "other", 0, DeviceFault::Preempt { ppm: 20 });
    fault(&mut d, 7, "first", 1, disk_error());
    fault(&mut d, 7, "second", 1, disk_error());
    let events = advance(&mut d, 20);
    assert_eq!(rejected(&events), ["again"]);
    assert_eq!(requests(&d, 1).len(), 3);
    assert_eq!(requests(&d, 0).len(), 1);
    // After a heal the machine may be preempted again.
    heal(&mut d, 25, "race");
    fault(&mut d, 26, "later", 1, DeviceFault::Preempt { ppm: 30 });
    let events = advance(&mut d, 40);
    assert!(rejected(&events).is_empty());
}

#[test]
fn an_active_device_fault_continues_from_a_checkpoint() {
    let mut d = build(machines(), vec![], true);
    fault(&mut d, 5, "eio", 1, disk_error());
    heal(&mut d, 25, "eio");
    advance(&mut d, 12);
    let checkpoint = d.checkpoint().unwrap();
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 40);
    let expected_states = states(&d);
    assert!(matches!(
        settled(&expected).as_slice(),
        [(_, FaultAction::Heal, SettlementStatus::Applied, Some(_))]
    ));
    for workers in [1, 3] {
        let mut branch = restore(&checkpoint, workers);
        assert_eq!(advance(&mut branch, 40), expected);
        assert_eq!(states(&branch), expected_states);
    }
}

/// The oracle (machine 0) applying device faults to machine 1 through its
/// control calls, and healing them by the handles it was given.
mod oracle {
    use super::*;
    use crate::pv::SimulationControlId;
    use seedbox_spec::{backend::CpuBackend, definition::Architecture};

    const fn engine(backend: CpuBackend, architecture: Architecture) -> faults::Engine {
        faults::Engine {
            backend,
            architecture,
        }
    }
    const ARM_PV: faults::Engine = engine(CpuBackend::Pv, Architecture::Aarch64);
    const X86_PV: faults::Engine = engine(CpuBackend::Pv, Architecture::X86_64);

    const DISK_ERROR: &str =
        r#""op":"disk_error","target":{"machine":"fixture-1"},"disk_op":"write","ppm":1000"#;
    const DISK_LATENCY: &str = r#""op":"disk_latency","target":{"machine":"fixture-1"},"disk_op":"flush","latency_ns":20000000,"jitter_ns":5"#;
    const PREEMPT: &str = r#""op":"preempt","target":{"service":"fixture-1"},"ppm":100000"#;

    /// The oracle's call `id`, sent at `at`.
    fn call(at: u64, id: u32, fields: &str) -> (u64, Response) {
        (
            at,
            Response::SimulationControl {
                id: SimulationControlId {
                    session: std::num::NonZeroU32::new(1).unwrap(),
                    request: std::num::NonZeroU32::new(id).unwrap(),
                },
                payload: format!(r#"{{"version":1,"id":{id},{fields}}}"#).into_bytes(),
            },
        )
    }
    fn calls(calls: Vec<(u64, Response)>, engine: Option<faults::Engine>) -> Driver<Mock> {
        let mut sends = vec![(0, Response::Ready)];
        sends.extend(calls);
        let mut d = build(
            vec![
                spec(0, true, sends),
                spec(1, true, vec![(0, Response::Ready)]),
            ],
            vec![],
            true,
        );
        if let Some(engine) = engine {
            d.set_engine(engine);
        }
        d
    }
    /// The replies the oracle received, as JSON, in call order. A refusal
    /// replies at admission, ahead of effects admitted at the same commit.
    fn replies(d: &Driver<Mock>) -> Vec<serde_json::Value> {
        let mut replies: Vec<serde_json::Value> = states(d)[0]
            .received
            .iter()
            .filter_map(|input| match &input.payload {
                Request::SimulationReply { payload, .. } => serde_json::from_slice(payload).ok(),
                _ => None,
            })
            .collect();
        replies.sort_by_key(|reply| reply["id"].as_u64());
        replies
    }

    #[test]
    fn the_oracle_applies_each_device_fault_and_heals_it_by_its_handle() {
        let mut d = calls(
            vec![
                call(0, 1, DISK_ERROR),
                call(1, 2, DISK_LATENCY),
                call(2, 3, PREEMPT),
                call(20, 4, r#""op":"heal","handle":"p1""#),
                call(21, 5, r#""op":"heal","handle":"p2""#),
                call(22, 6, r#""op":"heal","handle":"p3""#),
            ],
            Some(ARM_PV),
        );
        let events = advance(&mut d, 60);
        let replies = replies(&d);
        let handles: Vec<_> = replies[..3]
            .iter()
            .map(|reply| reply["result"]["handle"].clone())
            .collect();
        assert_eq!(handles, ["p1", "p2", "p3"], "{replies:?}");
        for reply in &replies[3..] {
            assert_eq!(reply["ok"], true, "{reply}");
            assert_eq!(reply["result"], serde_json::json!({}), "{reply}");
        }
        let faults = [
            DeviceFault::DiskError {
                op: DiskOp::Write,
                ppm: 1_000,
                effect: DiskEffect::Eio,
                scope: DiskScope::All,
            },
            DeviceFault::DiskLatency {
                op: DiskOp::Flush,
                latency_ns: 20_000_000,
                jitter_ns: 5,
            },
            DeviceFault::Preempt { ppm: 100_000 },
        ];
        let requests = requests(&d, 1);
        let installed: Vec<_> = requests
            .iter()
            .filter_map(|request| match request {
                Request::InstallFault { fault, .. } => Some(*fault),
                _ => None,
            })
            .collect();
        assert_eq!(installed, faults);
        assert_eq!(
            requests
                .iter()
                .filter(|request| matches!(request, Request::HealFault { .. }))
                .count(),
            3
        );
        assert!(requests_of(&d, 0).is_empty(), "only the target receives it");
        // Each settles as the same fault a program input applies, and a disk
        // fault's heal carries its counts.
        let settled = settled(&events);
        let actions: Vec<_> = settled
            .iter()
            .map(|(_, action, status, _)| (*action, *status))
            .collect();
        assert_eq!(
            actions,
            [
                (
                    FaultAction::DeviceFault(faults[0]),
                    SettlementStatus::Applied
                ),
                (
                    FaultAction::DeviceFault(faults[1]),
                    SettlementStatus::Applied
                ),
                (
                    FaultAction::DeviceFault(faults[2]),
                    SettlementStatus::Applied
                ),
                (FaultAction::Heal, SettlementStatus::Applied),
                (FaultAction::Heal, SettlementStatus::Applied),
                (FaultAction::Heal, SettlementStatus::Applied),
            ]
        );
        assert!(settled[3].3.is_some() && settled[4].3.is_some());
    }

    #[test]
    fn a_second_preempt_on_one_machine_is_refused_while_disk_faults_stack() {
        let mut d = calls(
            vec![
                call(0, 1, PREEMPT),
                call(1, 2, PREEMPT),
                call(2, 3, DISK_ERROR),
                call(3, 4, DISK_ERROR),
            ],
            Some(ARM_PV),
        );
        advance(&mut d, 40);
        let replies = replies(&d);
        assert_eq!(replies[1]["error"]["code"], "invalid_state", "{replies:?}");
        assert_eq!(replies[1]["error"]["message"], "preempt: AlreadyPreempted");
        for index in [0, 2, 3] {
            assert_eq!(replies[index]["ok"], true, "{replies:?}");
        }
        assert_eq!(requests(&d, 1).len(), 3);
    }

    #[test]
    fn engines_without_a_device_fault_refuse_it_naming_what_they_lack() {
        let tcg = |architecture| engine(CpuBackend::Tcg, architecture);
        let cases = [
            (
                Some(X86_PV),
                PREEMPT,
                "preempt needs the guest yield hook, which x86_64 PV does not provide",
            ),
            (
                Some(tcg(Architecture::X86_64)),
                DISK_ERROR,
                "disk_error needs disk request selection, which x86_64 TCG does not provide",
            ),
            (
                Some(tcg(Architecture::Aarch64)),
                DISK_LATENCY,
                "disk_latency needs delayed block completion, which aarch64 TCG does not provide",
            ),
            (
                Some(tcg(Architecture::X86_64)),
                PREEMPT,
                "preempt needs the guest yield hook, which x86_64 TCG does not provide",
            ),
            (
                None,
                DISK_ERROR,
                "disk_error needs disk request selection, and this Driver was told no engine",
            ),
        ];
        for (engine, fields, message) in cases {
            let mut d = calls(vec![call(0, 1, fields)], engine);
            let events = advance(&mut d, 30);
            let reply = &replies(&d)[0];
            assert_eq!(reply["error"]["code"], "unsupported", "{reply}");
            assert_eq!(reply["error"]["message"], message);
            assert!(requests(&d, 1).is_empty());
            assert!(settled(&events).is_empty());
            assert!(!events
                .iter()
                .any(|e| matches!(e.kind, EventKind::FaultAdmitted(_))));
        }
        // x86_64 PV carries out the disk faults.
        let mut d = calls(
            vec![call(0, 1, DISK_ERROR), call(1, 2, DISK_LATENCY)],
            Some(X86_PV),
        );
        advance(&mut d, 30);
        assert!(replies(&d).iter().all(|reply| reply["ok"] == true));
        assert_eq!(requests(&d, 1).len(), 2);
    }

    #[test]
    fn invalid_arguments_are_refused_before_the_engine_is_consulted() {
        // TCG would refuse every one as unsupported; the arguments come first.
        let mut d = calls(
            vec![
                call(
                    0,
                    1,
                    r#""op":"disk_error","target":{"machine":"fixture-1"},"disk_op":"flush","ppm":10,"effect":"corrupt""#,
                ),
                call(
                    1,
                    2,
                    r#""op":"disk_latency","target":{"machine":"fixture-1"},"disk_op":"read","latency_ns":1"#,
                ),
                call(
                    2,
                    3,
                    r#""op":"preempt","target":{"machine":"fixture-1"},"ppm":0"#,
                ),
            ],
            Some(engine(CpuBackend::Tcg, Architecture::Aarch64)),
        );
        advance(&mut d, 30);
        let messages: Vec<_> = replies(&d)
            .iter()
            .map(|reply| {
                assert_eq!(reply["error"]["code"], "invalid_request", "{reply}");
                reply["error"]["message"].as_str().unwrap().to_owned()
            })
            .collect();
        assert_eq!(
            messages,
            [
                "disk_error on flushes takes neither the corrupt effect nor the data scope",
                "disk_latency latency_ns must be 1000 to 1000000000 and jitter_ns at most 1000000000",
                "preempt ppm must be 1 to 1000000",
            ]
        );
        assert!(requests(&d, 1).is_empty());
    }
}
