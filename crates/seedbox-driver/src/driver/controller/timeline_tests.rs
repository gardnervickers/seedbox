//! Queue ownership, overflow, and acknowledgement timing at the Driver boundary.
use super::tests::{configured, replies};
use crate::driver::{fixture::owned::*, *};
use std::{collections::BTreeMap, time::Duration};
#[test]
fn call_and_boot_pinned_reply_are_both_closed_codec_state() {
    for boundary in [5, 8] {
        let specs = vec![
            spec(0, true, vec![(2, call(1, r#""op":"now""#))]),
            spec(1, true, vec![]),
        ];
        let mut d = configured(specs.clone(), Some(Some(MachineId::ORACLE)), 1);
        advance(&mut d, boundary);
        assert_eq!(
            d.execution
                .events
                .values()
                .flatten()
                .filter(|event| match event {
                    Scheduled::Control(control::Event::Call { .. }) => boundary == 5,
                    Scheduled::Control(control::Event::Reply { .. }) => boundary == 8,
                    _ => false,
                })
                .count(),
            1
        );
        let c = d.checkpoint().unwrap();
        reference_observer_session(&mut d);
        assert_eq!(c.controller_setup(), d.controller_setup());
        let (bytes, store) = artifact(c);
        let expected = advance(&mut d, 25);
        let state = states(&d);
        for workers in [1, 4] {
            let c = decode(&bytes, &store);
            let mut branch = restore(&c, workers);
            assert_eq!(advance(&mut branch, 25), expected);
            assert_eq!(states(&branch), state);
            assert_eq!(replies(&branch).len(), 1);
        }
        let mut fresh_four = configured(specs, Some(Some(MachineId::ORACLE)), 4);
        advance(&mut fresh_four, boundary);
        reference_observer_session(&mut fresh_four);
        assert_eq!(advance(&mut fresh_four, 25), expected);
        assert_eq!(states(&fresh_four), state);
    }
}
#[test]
fn acknowledgement_effect_time_not_commit_time_sets_reply_deadline() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (
                        3,
                        call(
                            1,
                            r#""op":"clock_skew","target":{"machine":"fixture-1"},"offset_ns":1"#,
                        ),
                    ),
                ],
            ),
            spec(1, true, vec![(0, Response::Ready)]),
        ],
        Some(Some(MachineId::ORACLE)),
        4,
    );
    let events = advance(&mut d, 30);
    let r = replies(&d);
    assert_eq!(r.len(), 1);
    let effect = r[0].1["effect_at_ns"].as_u64().unwrap();
    assert_eq!(effect, 11);
    assert_eq!(r[0].0, effect + 5);
    assert!(events.iter().any(|event| matches!(event.kind, EventKind::FaultSettled { outcome_at: Some(time), .. } if time == at(effect)) && event.at > at(effect)));
    assert_eq!(r[0].1["replied_at_ns"], effect + 5);
}
#[test]
fn checked_admission_and_reply_overflow_poison_without_wrapping() {
    for (observed, reply_overflow) in [(u64::MAX - 3, false), (u64::MAX - 9, true)] {
        let specs: BTreeMap<_, _> = [spec(0, true, vec![(observed, call(1, r#""op":"now""#))])]
            .into_iter()
            .map(|s| (s.name.clone(), s))
            .collect();
        let setup = NetworkSetup::from(
            NetworkConfig::new(Duration::from_nanos(2), Duration::ZERO, 0).unwrap(),
        )
        .with_controller(Some(MachineId::ORACLE), Duration::from_nanos(5))
        .unwrap();
        let mut d = Driver::new_seeded_with_network(
            at(u64::MAX - 10),
            Duration::from_nanos(1),
            1,
            51,
            || Ok(Context(None)),
            fixture::definition(0),
            vec![],
            vec![],
            setup,
            move |definition| Ok(Mock::new(specs[&definition.name].clone())),
        )
        .unwrap();
        let error = loop {
            match d.step(at(u64::MAX - 2)) {
                Err(error) => break error,
                Ok(_) => assert!(d.now() < at(u64::MAX - 2), "overflow call was lost"),
            }
        };
        if reply_overflow {
            assert!(
                matches!(error, Error::Control(message) if message.contains("reply time overflow"))
            );
        } else {
            assert!(matches!(error, Error::TimeOverflow));
        }
        assert!(matches!(d.step(at(u64::MAX - 1)), Err(Error::Failed)));
    }
}

#[test]
fn same_time_input_call_and_trigger_keep_existing_fifo_rules() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (
                        0,
                        call(
                            1,
                            r#""op":"arm_trigger","source":{"machine":"fixture-1"},"marker":[120],"match_timeout_ns":20,"fault":{"action":"stop","target":{"machine":"fixture-1"}}"#,
                        ),
                    ),
                    (5, call(2, r#""op":"now""#)),
                ],
            ),
            spec(1, true, vec![(0, Response::Ready), (10, output(b"x"))]),
        ],
        Some(Some(MachineId::ORACLE)),
        4,
    );
    d.schedule(
        at(10),
        key("host-at-trigger-and-call"),
        Action::Pause(Target::Machine(MachineId(0))),
    )
    .unwrap();
    d.schedule(
        at(11),
        key("resume-host-controller"),
        Action::Resume(Target::Machine(MachineId(0))),
    )
    .unwrap();
    let events = advance(&mut d, 25);
    let faults: Vec<_> = events
        .iter()
        .filter_map(|event| {
            if let EventKind::FaultAdmitted(record) = &event.kind {
                (event.at == at(10)).then_some((event.at, &record.origin))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(faults.len(), 2);
    // Committed bytes admit the trigger before timeline processing. Host input
    // admission then takes its prequeued FIFO turn, but its effect plan appends
    // behind an already-queued reply. Admission is not effect dispatch.
    assert!(matches!(faults[0], (time, FaultOrigin::Trigger { .. }) if time == at(10)));
    assert!(matches!(faults[1], (time, FaultOrigin::Input { .. }) if time == at(10)));
    let pause = events
        .iter()
        .position(|event| {
            event.at == at(10)
                && matches!(
                    event.kind,
                    EventKind::Transition {
                        machine: MachineId(0),
                        change: Transition::Paused,
                        ..
                    }
                )
        })
        .unwrap();
    let reply = events
        .iter()
        .position(|event| {
            event.at == at(10) && matches!(event.kind, EventKind::ReplyAdmitted { .. })
        })
        .unwrap();
    let host_admission = events.iter().position(|event| event.at == at(10) && matches!(&event.kind, EventKind::FaultAdmitted(record) if matches!(record.origin, FaultOrigin::Input { .. }))).unwrap();
    assert!(host_admission < reply && reply < pause);
    assert!(states(&d)[1].received.iter().any(|input| input.at == at(10)
        && matches!(
            input.payload,
            crate::pv::Request::Application {
                command: crate::pv::ApplicationCommand::Stop,
                ..
            }
        )));
    let r = replies(&d);
    assert_eq!(r.len(), 2);
    assert_eq!(r[0].1["admitted_at_ns"], 5);
    assert_eq!(r[0].0, 10);
    assert_eq!(r[1].1["admitted_at_ns"], 10);
    assert_eq!(r[1].0, 15);
}

#[test]
fn pending_old_boot_call_cannot_steal_reused_successor_rpc_identity() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![(2, call(1, r#""op":"sleep","duration_ns":20"#))],
            ),
            spec(1, true, vec![]),
        ],
        Some(Some(MachineId::ORACLE)),
        1,
    );
    advance(&mut d, 8);
    let boot = d.execution.machines[0].lifecycle.boot();
    let plans = d
        .execution
        .control
        .admission(
            &mut d.execution.machines,
            MachineId::ORACLE,
            at(8),
            crate::oracle::observation::Transition::Reboot { new_boot: boot + 1 },
        )
        .unwrap();
    d.control_plans(plans).unwrap();
    d.execution.machines[0]
        .machine
        .as_mut()
        .unwrap()
        .state
        .sends
        .push_back((9, call(1, r#""op":"now""#)));
    advance(&mut d, 15);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    let expected = advance(&mut d, 40);
    let expected_states = states(&d);
    let result = replies(&d);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].0, 19);
    assert_eq!(result[0].1["requested_at_ns"], 9);
    for workers in [1, 4] {
        let c = decode(&bytes, &store);
        let mut branch = restore(&c, workers);
        assert_eq!(advance(&mut branch, 40), expected);
        assert_eq!(states(&branch), expected_states);
        assert_eq!(replies(&branch).len(), 1);
    }
}
