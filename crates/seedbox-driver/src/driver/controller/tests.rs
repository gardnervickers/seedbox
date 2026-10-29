//! Controller authorization and deterministic timeline tests, with mock machines.
use crate::driver::{self, fixture::owned::*, *};
use crate::pv::Request;
use serde_json::Value;
use std::collections::BTreeMap;

pub(super) fn configured(
    specs: Vec<State>,
    setting: Option<Option<MachineId>>,
    workers: usize,
) -> Driver<Mock> {
    let network = NetworkConfig::new(Duration::from_nanos(2), Duration::ZERO, 0).unwrap();
    let mut setup: NetworkSetup = network.into();
    if let Some(controller) = setting {
        setup = setup
            .with_controller(controller, Duration::from_nanos(5))
            .unwrap();
    }
    let definitions: Vec<_> = (1..specs.len()).map(driver::fixture::definition).collect();
    let specs: BTreeMap<_, _> = specs.into_iter().map(|s| (s.name.clone(), s)).collect();
    Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        workers,
        51,
        || Ok(Context(None)),
        driver::fixture::definition(0),
        definitions,
        vec![],
        setup,
        move |d| Ok(Mock::new(specs[&d.name].clone())),
    )
    .unwrap()
}
pub(super) fn replies(d: &Driver<Mock>) -> Vec<(u64, Value)> {
    d.execution.machines[0]
        .machine
        .as_ref()
        .unwrap()
        .state
        .received
        .iter()
        .filter_map(|input| {
            if let Request::SimulationReply { payload, .. } = &input.payload {
                Some((
                    input.at.as_nanos(),
                    serde_json::from_slice(payload).unwrap(),
                ))
            } else {
                None
            }
        })
        .collect()
}
#[test]
fn explicit_disabled_and_noncontroller_calls_poison_without_admission() {
    for (setting, source) in [(None, 0), (Some(MachineId(0)), 1)] {
        let mut specs = vec![spec(0, true, vec![]), spec(1, true, vec![])];
        specs[source].sends.push_back((2, call(1, r#""op":"now""#)));
        let mut d = configured(specs, Some(setting), 4);
        assert!(
            matches!(d.step(at(10)), Err(Error::Control(message)) if message.contains("unauthorized"))
        );
        assert!(matches!(d.step(at(20)), Err(Error::Failed)));
        assert!(!d
            .drain_observations()
            .events
            .iter()
            .any(|event| matches!(event.kind, EventKind::FaultAdmitted(_))));
    }
}
#[test]
fn controller_target_is_invalid_but_host_authority_and_timing_remain() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![(
                    2,
                    call(1, r#""op":"crash","target":{"machine":"fixture-0"}"#),
                )],
            ),
            spec(1, true, vec![]),
        ],
        Some(Some(MachineId(0))),
        1,
    );
    d.schedule(
        at(3),
        key("host-pause-controller"),
        Action::Pause(Target::Machine(MachineId(0))),
    )
    .unwrap();
    d.schedule(
        at(4),
        key("host-resume-controller"),
        Action::Resume(Target::Machine(MachineId(0))),
    )
    .unwrap();
    let events = advance(&mut d, 20);
    let r = replies(&d);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, 12);
    assert_eq!(r[0].1["error"]["code"], "invalid_request");
    assert_eq!(
        r[0].1["error"]["message"],
        "controller is outside failure model"
    );
    assert!(events.iter().any(|event| event.at == at(3) && matches!(&event.kind, EventKind::FaultAdmitted(record) if matches!(record.origin, FaultOrigin::Input { .. }))));
    assert!(!d.execution.failed);
}
#[test]
fn observed_admission_effect_and_reply_are_exact_not_round_delayed() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![(
                    2,
                    call(1, r#""op":"crash","target":{"machine":"fixture-1"}"#),
                )],
            ),
            spec(1, true, vec![]),
        ],
        Some(Some(MachineId(0))),
        4,
    );
    advance(&mut d, 25);
    let r = replies(&d);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, 12);
    assert_eq!(r[0].1["requested_at_ns"], 2);
    assert_eq!(r[0].1["admitted_at_ns"], 7);
    assert_eq!(r[0].1["effect_at_ns"], 7);
    assert_eq!(r[0].1["replied_at_ns"], 12);
}
#[test]
fn same_time_host_input_precedes_later_enqueued_guest_admission() {
    let mut d = configured(
        vec![
            spec(
                0,
                true,
                vec![(
                    2,
                    call(1, r#""op":"crash","target":{"machine":"fixture-1"}"#),
                )],
            ),
            spec(1, true, vec![]),
        ],
        Some(Some(MachineId(0))),
        1,
    );
    d.schedule(
        at(7),
        key("prequeued-host"),
        Action::Pause(Target::Machine(MachineId(1))),
    )
    .unwrap();
    let events = advance(&mut d, 20);
    let origins: Vec<_> = events
        .iter()
        .filter_map(|e| {
            if let EventKind::FaultAdmitted(record) = &e.kind {
                Some((e.at, record.origin.clone()))
            } else {
                None
            }
        })
        .collect();
    assert!(matches!(&origins[0], (t, FaultOrigin::Input { .. }) if *t == at(7)));
    assert!(matches!(&origins[1], (t, FaultOrigin::Control { .. }) if *t == at(7)));
}
#[test]
fn configured_reply_never_enters_successor_boot() {
    let mut d = configured(
        vec![
            spec(0, true, vec![(2, call(1, r#""op":"now""#))]),
            spec(1, true, vec![]),
        ],
        Some(Some(MachineId(0))),
        1,
    );
    advance(&mut d, 8);
    assert!(d.execution.events.values().flatten().any(|e| matches!(
        e,
        driver::Scheduled::Control(driver::control::Event::Reply { .. })
    )));
    // Fold a confirmed mock reboot through the same lifecycle commit used
    // after a physical reboot; the queued reply must retain its original boot.
    let boot = d.execution.machines[0].lifecycle.boot();
    let plans = d
        .execution
        .control
        .admission(
            &mut d.execution.machines,
            MachineId(0),
            at(8),
            crate::oracle::observation::Transition::Reboot { new_boot: boot + 1 },
        )
        .unwrap();
    d.control_plans(plans).unwrap();
    advance(&mut d, 20);
    assert!(replies(&d).is_empty());
}
#[test]
fn omission_preserves_sdk_events_streams_and_commit_timing() {
    let specs = vec![
        spec(0, true, vec![(2, call(1, r#""op":"now""#))]),
        spec(1, true, vec![]),
    ];
    let mut original_sdk = build(specs.clone(), vec![], true);
    let mut sdk = configured(specs.clone(), None, 1);
    let mut other = configured(specs, None, 4);
    let original_events = advance(&mut original_sdk, 25);
    let configured_events = advance(&mut sdk, 25);
    assert_eq!(original_events, configured_events);
    assert_eq!(states(&original_sdk), states(&sdk));
    assert_eq!(configured_events, advance(&mut other, 25));
    assert_eq!(states(&sdk), states(&other));
    let r = replies(&sdk);
    assert_eq!(r[0].1["admitted_at_ns"], 10);
    assert_eq!(r[0].1["replied_at_ns"], 10);
}
#[test]
fn both_due_queue_phases_roundtrip_codec_and_workers_without_readmission_or_draws() {
    for boundary in [5, 8] {
        let specs = vec![
            spec(
                0,
                true,
                vec![
                    (0, Response::Ready),
                    (
                        2,
                        call(
                            1,
                            r#""op":"clock_skew","target":{"machine":"fixture-1"},"max_offset_ns":100"#,
                        ),
                    ),
                ],
            ),
            spec(1, true, vec![(0, Response::Ready)]),
        ];
        let mut source = configured(specs, Some(Some(MachineId(0))), 1);
        advance(&mut source, boundary);
        let (bytes, store) = artifact(source.checkpoint().unwrap());
        assert_eq!(&bytes[8..12], &6u32.to_le_bytes());
        reference_observer_session(&mut source);
        let expected = advance(&mut source, 35);
        let expected_states = states(&source);
        drop(source);
        for workers in [1, 4] {
            let checkpoint = decode(&bytes, &store);
            let mut restored = restore(&checkpoint, workers);
            assert_eq!(advance(&mut restored, 35), expected);
            assert_eq!(states(&restored), expected_states);
            assert_eq!(replies(&restored).len(), 1, "{:?}", states(&restored));
        }
    }
}
#[test]
fn low_latency_overflow_and_wrong_identity_refuse_before_execution() {
    let config = NetworkConfig::new(Duration::from_nanos(2), Duration::ZERO, 0).unwrap();
    assert!(NetworkSetup::from(config)
        .with_controller(Some(MachineId(0)), Duration::from_nanos(1))
        .is_err());
    assert!(NetworkSetup::from(config)
        .with_controller(Some(MachineId(0)), Duration::MAX)
        .is_err());
    let setup = NetworkSetup::from(config)
        .with_controller(Some(MachineId(1)), Duration::from_nanos(5))
        .unwrap();
    let result = Driver::<Mock>::new_seeded(
        at(0),
        Duration::from_nanos(10),
        1,
        51,
        || panic!("context must not run"),
        driver::fixture::definition(0),
        vec![driver::fixture::definition(1)],
        vec![],
        setup,
        |_| panic!("factory must not run"),
    );
    assert!(matches!(result, Err(Error::Control(message)) if message.contains("named oracle")));
}

#[test]
fn guest_cannot_heal_host_network_handle_targeting_controller() {
    let definition = |index| {
        let mut definition = fixture::definition(index);
        definition.network = Some(
            crate::definition::NetworkInterface::new(
                crate::definition::MacAddress::new(mac(index)).unwrap(),
                std::net::Ipv4Addr::new(10, 0, 0, (index + 1) as u8),
                24,
            )
            .unwrap(),
        );
        definition
    };
    let specs: BTreeMap<_, _> = [
        spec(
            0,
            true,
            vec![(
                2,
                call(1, r#""op":"heal","handle":"host-controller-links""#),
            )],
        ),
        spec(1, true, vec![]),
    ]
    .into_iter()
    .map(|state| (state.name.clone(), state))
    .collect();
    let setup =
        NetworkSetup::from(NetworkConfig::new(Duration::from_nanos(2), Duration::ZERO, 0).unwrap())
            .with_controller(Some(MachineId::ORACLE), Duration::from_nanos(5))
            .unwrap();
    let mut d = Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        1,
        51,
        || Ok(Context(None)),
        definition(0),
        vec![definition(1)],
        vec![],
        setup,
        move |definition| Ok(Mock::new(specs[&definition.name].clone())),
    )
    .unwrap();
    d.with_barrier(|barrier| {
        barrier.schedule_with_handle(
            at(0),
            key("host-partition"),
            Action::Partition {
                left: vec![Target::Machine(MachineId::ORACLE)],
                right: vec![Target::Machine(MachineId(1))],
            },
            FaultHandle::named("host-controller-links").unwrap(),
        )
    })
    .unwrap();
    d.schedule(
        at(18),
        key("host-heal-controller-links"),
        Action::Heal(HealTarget::Handle(
            FaultHandle::named("host-controller-links").unwrap(),
        )),
    )
    .unwrap();
    let events = advance(&mut d, 20);
    let r = replies(&d);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].1["error"]["code"], "invalid_request");
    assert_eq!(
        r[0].1["error"]["message"],
        "controller is outside failure model"
    );
    assert!(events.iter().any(|event| event.at == at(18) && matches!(&event.kind, EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.action == FaultAction::Heal && matches!(record.origin, FaultOrigin::Input { .. }))));
    assert!(!d.execution.failed);
}
