//! Pure owned-machine network/control mocks, including transient FIFO states.
use crate::driver::fixture::owned::network_fixture::{initial, net, policy, setup};
use crate::driver::fixture::owned::*;
use crate::driver::network_test_frames as frames;
use crate::driver::*;
use crate::pv::SimulationControlId;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
fn guest(n: u32, args: Value) -> Response {
    let text = args.to_string();
    call(n, &text[1..text.len() - 1])
}
fn tx(bytes: Vec<u8>) -> Response {
    Response::TransmitFrame { bytes }
}
fn partition() -> Action {
    Action::Partition {
        left: vec![Target::Machine(MachineId(1))],
        right: vec![Target::Machine(MachineId(2))],
    }
}
fn shape(policy: NetworkPolicy, network_faults: NetworkFaults) -> Action {
    Action::Shape {
        from: vec![Target::Machine(MachineId(1))],
        to: vec![Target::Machine(MachineId(2))],
        policy,
        network_faults,
    }
}
fn name(text: &str) -> FaultHandle {
    FaultHandle::named(text).unwrap()
}
fn schedule(d: &mut Driver<Mock>, k: &str, a: Action, n: Option<&str>) {
    let now = d.now();
    d.with_barrier(|b| match n {
        Some(n) => b.schedule_with_handle(now, key(k), a, name(n)),
        None => b.schedule(now, key(k), a),
    })
    .unwrap();
    d.with_barrier(|b| b.apply_due_now()).unwrap();
}
fn handles(d: &Driver<Mock>) -> Vec<String> {
    let mut control = d.execution.control.clone();
    let now = d.now().as_nanos();
    let plans = control
        .invoke_with_network(&d.execution.machines, Some(&d.execution.network), |s, v| {
            s.admit(
                v,
                control::Control::source(MachineId(0)),
                SimulationControlId {
                    session: nz(1),
                    request: nz(999),
                },
                now,
                now,
                br#"{"version":1,"id":999,"op":"inspect"}"#,
            )
        })
        .unwrap();
    let payload = plans
        .into_iter()
        .find_map(|p| match p {
            crate::oracle::service::Plan::Reply { payload, .. } => Some(payload),
            _ => None,
        })
        .unwrap();
    let value: Value = serde_json::from_slice(&payload).unwrap();
    value["result"]["active_handles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect()
}
fn replies(d: &Driver<Mock>) -> BTreeMap<u64, Value> {
    d.execution.machines[0]
        .machine
        .as_ref()
        .unwrap()
        .state
        .received
        .iter()
        .filter_map(|i| match &i.payload {
            Request::SimulationReply { payload, .. } => {
                let v: Value = serde_json::from_slice(payload).unwrap();
                Some((v["id"].as_u64().unwrap(), v))
            }
            _ => None,
        })
        .collect()
}
fn settled(
    events: &[Event],
    k: &str,
) -> (FaultRecord, SettlementStatus, Option<NetworkSettlement>) {
    events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultSettled {
                record,
                status,
                network,
                ..
            } if record.origin == (FaultOrigin::Input { key: key(k) }) => {
                Some((record.clone(), *status, network.clone()))
            }
            _ => None,
        })
        .unwrap()
}
fn staged_tx(d: &mut Driver<Mock>, from: usize, bytes: Vec<u8>) {
    let response = MachineResponse {
        machine: MachineId(from),
        boot: d.execution.machines[from].lifecycle.boot(),
        at: d.now(),
        payload: tx(bytes),
    };
    d.execution.machines[from].staged.push(response);
    d.commit_boundary().unwrap();
}
fn direct_admit(d: &mut Driver<Mock>, k: &str, a: Action, n: Option<&str>) {
    d.execution.inputs.insert(key(k), LiveInput::Pending);
    d.admit_input(key(k), a, n.map(name), d.now()).unwrap();
    d.commit_boundary().unwrap();
}
fn flow(d: &Driver<Mock>) -> Connection {
    d.execution
        .network
        .connections("fixture-1", "fixture-2", 0)
        .remove(0)
}
#[test]
fn named_host_guest_cross_heals_inspect_numbering_and_typed_counts() {
    let mut specs = initial();
    specs[0].sends.extend([(6,guest(1,json!({"op":"partition","left":[{"machine":"fixture-1"}],"right":[{"machine":"fixture-2"}]}))),(8,guest(2,json!({"op":"inspect"}))),(9,guest(3,json!({"op":"heal","handle":"net"}))),(11,guest(4,json!({"op":"inspect"})))]);
    specs[1].sends.extend([
        (5, tx(frames::plain(1, 2, 1))),
        (7, tx(frames::plain(1, 2, 2))),
    ]);
    let mut d = net(specs, setup(32), 2);
    advance(&mut d, 3);
    let mut p = policy(10);
    p.bandwidth_bps = Some(8_000_000_000);
    p.queue_bound_ns = Some(120);
    schedule(
        &mut d,
        "host",
        shape(
            p,
            NetworkFaults {
                duplicate_ppm: 1_000_000,
                ..NetworkFaults::default()
            },
        ),
        Some("net"),
    );
    let mut events = d.drain_events();
    // Explicit host cuts separate these calls' admission/effect turns; calls
    // emitted together in one positive-budget batch intentionally admit first.
    for end in [6, 8, 9, 12] {
        events.extend(advance(&mut d, end));
    }
    let replies = replies(&d);
    assert_eq!(replies[&1]["result"]["handle"], "p1");
    assert_eq!(
        replies[&2]["result"]["active_handles"],
        json!(["net", "p1"])
    );
    assert_eq!(replies[&4]["result"]["active_handles"], json!(["p1"]));
    let counts = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultSettled {
                record,
                network: Some(c),
                ..
            } if record
                .handle
                .as_ref()
                .is_some_and(|h| h.to_string() == "net")
                && record.action == FaultAction::Heal =>
            {
                Some(c.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        serde_json::to_value(&counts).unwrap(),
        replies[&3]["result"]
    );
    assert_eq!(
        counts.queue.unwrap(),
        QueueCounts {
            matched: 1,
            queue_dropped: 0,
            delayed_max_ns: 60
        }
    );
    assert_eq!(counts.faults.unwrap().duplicated, 1);
    schedule(
        &mut d,
        "host-heal-guest",
        Action::Heal(HealTarget::Handle(name("p1"))),
        None,
    );
    events.extend(d.drain_events());
    assert_eq!(
        settled(&events, "host-heal-guest").1,
        SettlementStatus::Applied
    );
    advance(&mut d, 100);
    let counts = d.execution.network.counts();
    assert_eq!(
        counts[&NetworkPair {
            from: MachineId(1),
            to: MachineId(2)
        }]
            .delivered,
        2
    );
    assert_eq!(
        counts[&NetworkPair {
            from: MachineId(1),
            to: MachineId(2)
        }]
            .partition_dropped,
        1
    );
}
#[test]
fn observations_are_bounded_not_persisted_and_never_change_events_entropy_or_counts() {
    let mut specs = initial();
    specs[1]
        .sends
        .extend((1..=3).map(|t| (t, tx(frames::plain(1, 2, t as u8)))));
    let mut off = net(specs.clone(), setup(0), 1);
    let mut on = net(specs, setup(1), 4);
    let expected = advance(&mut off, 40);
    assert_eq!(advance(&mut on, 40), expected);
    assert_eq!(states(&off), states(&on));
    assert_eq!(
        off.execution.network.counts(),
        on.execution.network.counts()
    );
    assert_eq!(off.drain_observations().frames, FrameFacts::default());
    for (i, e) in expected.iter().enumerate() {
        assert_eq!(e.sequence, i as u64);
    }
    let (bytes, store) = artifact(on.checkpoint().unwrap());
    let c = decode(&bytes, &store);
    let source_facts = on.drain_observations().frames;
    assert_eq!(source_facts.facts.len(), 1);
    assert_eq!(source_facts.omitted, 2);
    drop(on);
    let mut a = restore(&c, 1);
    let mut b = restore(&c, 4);
    let facts = a.drain_observations().frames;
    assert_eq!(facts, FrameFacts::default());
    assert_eq!(facts, b.drain_observations().frames);
    assert_eq!(a.drain_observations().frames, FrameFacts::default());
    assert_eq!(a.execution.network.counts(), off.execution.network.counts());
    assert_eq!(advance(&mut a, 60), advance(&mut b, 60));
}
#[test]
fn every_frame_fate_including_exited_receiver_has_independent_authority() {
    for (case, expected) in [
        (0, FrameFate::PartitionDropped),
        (1, FrameFate::LossDropped),
        (2, FrameFate::QueueDropped),
        (3, FrameFate::ReceiverDropped),
    ] {
        let mut specs = initial();
        specs[1].sends.push_back((3, tx(frames::plain(1, 2, 1))));
        let s = if case == 1 {
            NetworkSetup::from(
                NetworkConfig::new(Duration::from_nanos(10), Duration::ZERO, 1_000_000).unwrap(),
            )
            .with_frame_capacity(8)
        } else {
            setup(8)
        };
        let mut d = net(specs, s, 2);
        advance(&mut d, 1);
        if case == 0 {
            schedule(&mut d, "cut", partition(), Some("cut"));
        }
        if case == 2 {
            let mut p = policy(10);
            p.bandwidth_bps = Some(1);
            p.queue_bound_ns = Some(1);
            schedule(&mut d, "q", shape(p, NetworkFaults::default()), Some("q"));
        }
        advance(&mut d, 5);
        if case == 3 {
            schedule(
                &mut d,
                "exit",
                Action::Crash(Target::Machine(MachineId(2))),
                None,
            );
        }
        advance(&mut d, 30);
        let facts = d.drain_observations().frames;
        assert_eq!(facts.omitted, 0);
        assert_eq!(facts.facts.last().unwrap().fate, expected);
        let c = d.execution.network.counts()[&NetworkPair {
            from: MachineId(1),
            to: MachineId(2),
        }];
        assert_eq!(
            (
                c.partition_dropped,
                c.loss_dropped,
                c.queue_dropped,
                c.receiver_dropped
            ),
            match case {
                0 => (1, 0, 0, 0),
                1 => (0, 1, 0, 0),
                2 => (0, 0, 1, 0),
                _ => (0, 0, 0, 1),
            }
        );
        assert_eq!(c.delivered, 0);
        if case == 3 {
            let drop = facts.facts.last().unwrap();
            assert_eq!(drop.position.at, at(13));
            assert_eq!(drop.due, Some(at(13)));
            assert_eq!(facts.facts[0].fate, FrameFate::Scheduled);
        }
        artifact(d.checkpoint().unwrap());
    }
}
#[test]
fn guest_connection_pages_are_read_only_and_legacy_shaped() {
    let mut specs = initial();
    specs[1]
        .sends
        .extend((0..35).map(|i| (1, tx(frames::tcp(1, 2, (1000 + i, 80), 7, 0, 2)))));
    specs[0].sends.extend([(3,guest(1,json!({"op":"connections","from":{"machine":"fixture-1"},"to":{"machine":"fixture-2"}}))),(5,guest(2,json!({"op":"connections","from":{"machine":"fixture-1"},"to":{"machine":"fixture-2"},"after":32})))]);
    let mut d = net(specs, setup(0), 2);
    advance(&mut d, 8);
    let replies = replies(&d);
    assert_eq!(
        replies[&1]["result"]["connections"]
            .as_array()
            .unwrap()
            .len(),
        32
    );
    assert_eq!(replies[&1]["result"]["next_after"], 32);
    assert_eq!(
        replies[&2]["result"]["connections"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(replies[&2]["result"]["next_after"], 35);
    let (bytes, _) = artifact(d.checkpoint().unwrap());
    assert!(bytes.len() > 20);
    assert_eq!(handles(&d), Vec::<String>::new());
}
#[test]
fn guest_queries_pagination_reincarnation_and_heal_ignore_frame_retention() {
    let mut reference = None;
    for workers in [1, 4] {
        for (capacity, resize, drain) in [
            (0, false, false),
            (1, false, false),
            (1024, false, false),
            (1024, true, false),
            (1, false, true),
        ] {
            let mut specs = initial();
            specs[1]
                .sends
                .extend((0..35).map(|i| (1, tx(frames::tcp(1, 2, (1000 + i, 80), 7, 0, 2)))));
            specs[1].sends.extend([
                (6, tx(frames::tcp(1, 2, (1000, 80), 8, 1, 0x11))),
                (8, tx(frames::tcp(1, 2, (1000, 80), 9, 0, 2))),
            ]);
            specs[2]
                .sends
                .push_back((7, tx(frames::tcp(2, 1, (80, 1000), 1, 9, 0x11))));
            specs[0].sends.extend([
                (3, guest(1, json!({"op":"connections","from":{"machine":"fixture-1"},"to":{"machine":"fixture-2"}}))),
                (4, guest(2, json!({"op":"inspect"}))),
                (5, guest(3, json!({"op":"connections","from":{"machine":"fixture-1"},"to":{"machine":"fixture-2"},"after":32}))),
                (9, guest(4, json!({"op":"connections","from":{"machine":"fixture-1"},"to":{"machine":"fixture-2"},"after":35}))),
                (11, guest(5, json!({"op":"heal","handle":"query-shape"}))),
                (12, guest(6, json!({"op":"inspect"}))),
            ]);
            let mut d = net(specs, setup(capacity), workers);
            advance(&mut d, 2);
            if resize {
                d.set_frame_capacity(1);
            }
            if drain {
                d.drain_observations();
            }
            schedule(
                &mut d,
                "shape",
                shape(
                    policy(10),
                    NetworkFaults {
                        duplicate_ppm: 1_000_000,
                        ..Default::default()
                    },
                ),
                Some("query-shape"),
            );
            for end in [3, 4, 5, 6, 7, 8, 9, 11, 12, 30] {
                advance(&mut d, end);
                if drain {
                    d.drain_observations();
                }
            }
            let actual_replies = replies(&d);
            assert_eq!(
                actual_replies[&1]["result"]["connections"]
                    .as_array()
                    .unwrap()
                    .len(),
                32
            );
            assert_eq!(actual_replies[&1]["result"]["next_after"], 32);
            assert_eq!(
                actual_replies[&3]["result"]["connections"]
                    .as_array()
                    .unwrap()
                    .len(),
                3
            );
            assert_eq!(actual_replies[&3]["result"]["next_after"], 35);
            assert_eq!(
                actual_replies[&4]["result"]["connections"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(actual_replies[&4]["result"]["next_after"], 36);
            assert_eq!(
                actual_replies[&2]["result"]["active_handles"],
                json!(["query-shape"])
            );
            assert_eq!(actual_replies[&6]["result"]["active_handles"], json!([]));
            let actual = (
                actual_replies,
                d.execution.network.counts(),
                states(&d),
                artifact(d.checkpoint().unwrap()).0,
            );
            if let Some(reference) = &reference {
                assert_eq!(&actual, reference);
            } else {
                reference = Some(actual);
            }
            let facts = d.drain_observations().frames;
            if capacity == 1 && !drain {
                assert!(facts.omitted > 0);
            }
        }
    }
}
#[test]
fn host_selection_guest_heal_fin_close_reincarnation_and_stale_pn_consumption() {
    let mut specs = initial();
    specs[1].sends.extend([
        (1, tx(frames::tcp(1, 2, (1234, 80), 7, 0, 2))),
        (5, tx(frames::tcp(1, 2, (1234, 80), 8, 1, 0x10))),
        (7, tx(frames::tcp(1, 2, (1234, 80), 8, 1, 0x10))),
        (12, tx(frames::tcp(1, 2, (1234, 80), 8, 1, 0x11))),
        (16, tx(frames::tcp(1, 2, (1234, 80), 9, 0, 2))),
    ]);
    specs[2]
        .sends
        .push_back((13, tx(frames::tcp(2, 1, (80, 1234), 1, 9, 0x11))));
    specs[0]
        .sends
        .push_back((8, guest(1, json!({"op":"heal","handle":"tcp"}))));
    let mut d = net(specs, setup(32), 2);
    advance(&mut d, 4);
    let old = flow(&d);
    schedule(
        &mut d,
        "select",
        Action::DropConnection {
            connection: old.clone(),
        },
        Some("tcp"),
    );
    let mut events = d.drain_events();
    events.extend(advance(&mut d, 15));
    assert!(!d.execution.network.selection_available(&old));
    assert!(d
        .execution
        .network
        .connections("fixture-1", "fixture-2", 0)
        .is_empty());
    let counts = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultSettled {
                network: Some(c),
                record,
                ..
            } if record.action == FaultAction::Heal => Some(c),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        counts.selection,
        Some(SelectionCounts {
            matched: 2,
            dropped: 2
        })
    );
    assert_eq!(replies(&d)[&1]["result"], json!({"matched":2,"dropped":2}));
    advance(&mut d, 18);
    let fresh = flow(&d);
    assert!(fresh.incarnation > old.incarnation);
    schedule(
        &mut d,
        "stale",
        Action::DropConnection { connection: old },
        None,
    );
    let rejected = d.drain_events();
    assert!(rejected
        .iter()
        .any(|e| matches!(&e.kind,EventKind::InputRejected{key:k,..}if k==&key("stale"))));
    schedule(&mut d, "next", partition(), None);
    assert_eq!(
        settled(&d.drain_events(), "next")
            .0
            .handle
            .unwrap()
            .to_string(),
        "p2"
    );
    artifact(d.checkpoint().unwrap());
}
#[test]
fn apply_rechecks_connection_and_failed_without_effect_does_not_poison() {
    let mut specs = initial();
    specs[1]
        .sends
        .push_back((1, tx(frames::tcp(1, 2, (1234, 80), 7, 0, 2))));
    let mut d = net(specs, setup(8), 1);
    advance(&mut d, 4);
    let c = flow(&d);
    // Ordinary admission, then a second committed output before its FIFO effect.
    direct_admit(
        &mut d,
        "late",
        Action::DropConnection { connection: c },
        None,
    );
    staged_tx(&mut d, 1, frames::tcp(1, 2, (1234, 80), 8, 1, 0x14));
    d.with_barrier(|b| b.apply_due_now()).unwrap();
    assert_eq!(
        settled(&d.drain_events(), "late").1,
        SettlementStatus::Failed
    );
    schedule(&mut d, "after", partition(), None);
    assert_eq!(
        settled(&d.drain_events(), "after")
            .0
            .handle
            .unwrap()
            .to_string(),
        "p2"
    );
    artifact(d.checkpoint().unwrap());
}
#[test]
fn selection_heal_freezes_counts_at_admission_not_application() {
    let mut specs = initial();
    specs[1]
        .sends
        .push_back((1, tx(frames::tcp(1, 2, (1234, 80), 7, 0, 2))));
    let mut d = net(specs, setup(16), 1);
    advance(&mut d, 4);
    let c = flow(&d);
    schedule(
        &mut d,
        "select",
        Action::DropConnection { connection: c },
        Some("f"),
    );
    staged_tx(&mut d, 1, frames::tcp(1, 2, (1234, 80), 8, 1, 0x10));
    direct_admit(
        &mut d,
        "heal",
        Action::Heal(HealTarget::Handle(name("f"))),
        None,
    );
    staged_tx(&mut d, 1, frames::tcp(1, 2, (1234, 80), 8, 1, 0x10));
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    let mut restored = restore(&decode(&bytes, &store), 4);
    for d in [&mut d, &mut restored] {
        d.with_barrier(|b| b.apply_due_now()).unwrap();
        let (_, status, counts) = settled(&d.drain_events(), "heal");
        assert_eq!(status, SettlementStatus::Applied);
        assert_eq!(
            counts.unwrap().selection,
            Some(SelectionCounts {
                matched: 1,
                dropped: 1
            })
        );
    }
}
#[test]
fn two_pending_aliases_roundtrip_then_one_fails_without_poison_or_wrong_key_retirement() {
    let mut d = net(initial(), setup(8), 1);
    advance(&mut d, 1);
    let now = d.now();
    d.with_barrier(|b| {
        b.pause_before_next_fault(PlaybackRate::Unlimited)?;
        b.schedule_with_handle(now, key("a"), partition(), name("same"))?;
        b.schedule_with_handle(now, key("b"), partition(), name("same"))
    })
    .unwrap();
    assert!(matches!(
        d.with_barrier(|b| b.apply_due_now()).unwrap(),
        DueOutcome::BeforeFault { .. }
    ));
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    let c = decode(&bytes, &store);
    drop(d);
    for workers in [1, 4] {
        let mut d = restore(&c, workers);
        assert!(matches!(
            d.step(d.now()).unwrap(),
            StepOutcome::BeforeFault { .. }
        ));
        d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        d.step(d.now()).unwrap();
        let events = d.drain_events();
        assert_eq!(settled(&events, "a").1, SettlementStatus::Applied);
        let loser = settled(&events, "b");
        assert_eq!(loser.1, SettlementStatus::Failed);
        assert!(!d.execution.inputs.contains_key(&key("b")));
        assert!(d.execution.inputs.contains_key(&key("a")));
        assert_eq!(handles(&d), ["same"]);
        schedule(
            &mut d,
            "heal",
            Action::Heal(HealTarget::Input(key("a"))),
            None,
        );
        assert!(handles(&d).is_empty());
        assert!(!d.execution.inputs.contains_key(&key("a")));
        schedule(&mut d, "next", partition(), None);
        assert_eq!(
            settled(&d.drain_events(), "next")
                .0
                .handle
                .unwrap()
                .to_string(),
            "p1"
        );
    }
}
#[test]
fn mixed_origin_apply_collision_is_failed_and_next_guest_number_is_not_reused() {
    let mut d = net(initial(), setup(0), 1);
    advance(&mut d, 1);
    let now = d.now();
    d.with_barrier(|b| {
        b.schedule_with_handle(now, key("named"), partition(), name("p1"))?;
        b.schedule(now, key("allocated"), partition())
    })
    .unwrap();
    d.with_barrier(|b| b.apply_due_now()).unwrap();
    let events = d.drain_events();
    assert_eq!(settled(&events, "named").1, SettlementStatus::Applied);
    assert_eq!(settled(&events, "allocated").1, SettlementStatus::Failed);
    schedule(&mut d, "next", partition(), None);
    assert_eq!(
        settled(&d.drain_events(), "next")
            .0
            .handle
            .unwrap()
            .to_string(),
        "p2"
    );
    let projection = d
        .with_barrier(|b| b.prepare_input_with_handle(key("duplicate"), partition(), name("p2")))
        .unwrap();
    assert!(
        matches!(projection.outcome(),PreparedOutcome::Rejected(r)if r.detail=="duplicate active handle")
    );
    artifact(d.checkpoint().unwrap());
}
#[test]
fn own_input_heal_refuses_pending_neighbor_even_with_active_same_text() {
    let mut d = net(initial(), setup(0), 1);
    advance(&mut d, 1);
    direct_admit(&mut d, "a", partition(), Some("same"));
    direct_admit(&mut d, "b", partition(), Some("same"));
    let now = d.now();
    // Isolate the transient between the two same-time effects, not a public API.
    let pending = d
        .execution
        .events
        .get_mut(&now)
        .unwrap()
        .pop_back()
        .unwrap();
    d.with_barrier(|b| b.apply_due_now()).unwrap();
    d.execution
        .events
        .entry(now)
        .or_default()
        .push_back(pending);
    assert!(matches!(
            d.with_barrier(
                |b| b.prepare_input(key("wrong"), Action::Heal(HealTarget::Input(key("b"))))
            ),
            Err(Error::DueWorkPending)
        ));
    // Public prepare intentionally refuses transient due work. Exercise ordinary
    // admission at that FIFO state on a clone, rather than weakening its guard.
    let project = |action| {
        let mut control = d.execution.control.clone();
        Driver::<Mock>::input_admission_with_handle(
            &mut control,
            &d.execution.machines,
            Some(&d.execution.network),
            &d.execution.inputs,
            &key("projection"),
            &action,
            None,
            now,
        )
        .unwrap()
    };
    assert!(
        matches!(project(Action::Heal(HealTarget::Input(key("b")))),Err(r)if r.detail.contains("owns no active handle"))
    );
    assert_eq!(handles(&d), ["same"]);
    assert!(project(Action::Heal(HealTarget::Handle(name("same")))).is_ok());
    d.with_barrier(|b| b.apply_due_now()).unwrap();
    assert_eq!(settled(&d.drain_events(), "b").1, SettlementStatus::Failed);
}
#[test]
fn named_prepare_exact_commit_authored_and_held_paths_preserve_alias_and_identity() {
    let mut d = net(initial(), setup(8), 1);
    advance(&mut d, 1);
    let p = d
        .with_barrier(|b| {
            b.set_rate(PlaybackRate::Paused)?;
            b.prepare_input_with_handle(
                key("shape"),
                shape(policy(20), NetworkFaults::default()),
                name("net"),
            )
        })
        .unwrap();
    assert_eq!(p.handle(), Some(&name("net")));
    let prepared = match p.outcome() {
        PreparedOutcome::Admitted(r) => r.clone(),
        _ => panic!("shape rejected"),
    };
    let before = artifact(d.checkpoint().unwrap()).0;
    let changed = d
        .with_barrier(|b| {
            b.prepare_input_with_handle(
                key("shape"),
                shape(policy(20), NetworkFaults::default()),
                name("changed"),
            )
        })
        .unwrap();
    assert_ne!(changed, p);
    assert_eq!(artifact(d.checkpoint().unwrap()).0, before);
    let outcome = d.with_barrier(|b| b.commit_input(&p)).unwrap();
    assert!(matches!(outcome,DueOutcome::BeforeFault{fault,..}if fault==prepared));
    assert!(handles(&d).is_empty());
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    let mut restored = restore(&decode(&bytes, &store), 4);
    assert!(matches!(
        restored.step(restored.now()).unwrap(),
        StepOutcome::BeforeFault { .. }
    ));
    for d in [&mut d, &mut restored] {
        d.with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
            .unwrap();
        d.step(d.now()).unwrap();
        assert_eq!(settled(&d.drain_events(), "shape").0, *prepared);
        assert_eq!(handles(d), ["net"]);
        schedule(
            d,
            "heal",
            Action::Heal(HealTarget::Input(key("shape"))),
            None,
        );
    }
    // A future authored input persists its explicit alias before admission.
    let future = at(d.now().as_nanos() + 2);
    d.with_barrier(|b| {
        b.schedule_with_handle(future, key("authored"), partition(), name("future"))
    })
    .unwrap();
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    let mut restored = restore(&decode(&bytes, &store), 2);
    assert_eq!(advance(&mut d, 10), advance(&mut restored, 10));
    assert_eq!(handles(&d), ["future"]);
}
#[test]
fn pending_network_shape_is_codec_closed_and_continues_with_sessions_burst_and_queue() {
    let mut specs = initial();
    specs[1].sends.extend([
        (1, tx(frames::tcp(1, 2, (1234, 80), 7, 0, 2))),
        (7, tx(frames::plain(1, 2, 1))),
        (8, tx(frames::plain(1, 2, 2))),
        (9, tx(frames::plain(1, 2, 3))),
        (12, tx(frames::plain(1, 2, 4))),
    ]);
    specs[2].sends.extend([
        (5, tx(frames::plain(2, 1, 1))),
        (6, tx(frames::plain(2, 1, 2))),
        (15, tx(frames::plain(2, 1, 3))),
    ]);
    let mut d = net(specs, setup(64), 1);
    advance(&mut d, 3);
    let c = flow(&d);
    schedule(
        &mut d,
        "selected",
        Action::DropConnection { connection: c },
        Some("flow"),
    );
    let mut p = policy(10);
    p.bandwidth_bps = Some(8_000_000_000);
    let mut forward = shape(
        p,
        NetworkFaults {
            burst_loss_ppm: 1_000_000,
            burst_length: 3,
            ..NetworkFaults::default()
        },
    );
    if let Action::Shape { from, to, .. } = &mut forward {
        std::mem::swap(from, to);
    }
    schedule(&mut d, "burst", forward, Some("burst"));
    schedule(
        &mut d,
        "queue",
        shape(
            p,
            NetworkFaults {
                duplicate_ppm: 1_000_000,
                reorder_ppm: 1_000_000,
                reorder_delay_ns: 3,
                latency_spike_ppm: 1_000_000,
                latency_spike_ns: 5,
                ..NetworkFaults::default()
            },
        ),
        Some("queue"),
    );
    advance(&mut d, 8);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    d.set_frame_capacity(64);
    let expected_events = advance(&mut d, 120);
    let expected_frames = d.drain_observations().frames;
    let expected_counts = d.execution.network.counts();
    let expected_states = states(&d);
    drop(d);
    for workers in [1, 4] {
        let mut restored = restore(&decode(&bytes, &store), workers);
        assert_eq!(restored.drain_observations().frames, FrameFacts::default());
        restored.set_frame_capacity(64);
        assert_eq!(advance(&mut restored, 120), expected_events);
        assert_eq!(restored.drain_observations().frames, expected_frames);
        assert_eq!(restored.execution.network.counts(), expected_counts);
        assert_eq!(states(&restored), expected_states);
    }
}
#[test]
fn real_network_fault_records_project_losslessly_to_canonical_control_actions() {
    let project = |r: &FaultRecord| {
        let handle = r.handle.as_ref().unwrap().to_string();
        match (&r.action, &r.target) {
            (
                FaultAction::Shape {
                    policy: p,
                    network_faults,
                },
                FaultTarget::DirectedLinks { from, to },
            ) => seedbox_engine_protocol::control::Action::Shape {
                handle,
                from: from.iter().map(|i| format!("fixture-{}", i.0)).collect(),
                to: to.iter().map(|i| format!("fixture-{}", i.0)).collect(),
                latency_ns: p.latency_ns,
                jitter_ns: p.jitter_ns,
                loss_ppm: p.loss_ppm,
                bandwidth_bps: p.bandwidth_bps,
                queue_bound_ns: p.queue_bound_ns,
                network_faults: *network_faults,
            },
            (FaultAction::DropConnection, FaultTarget::Connection { connection }) => {
                seedbox_engine_protocol::control::Action::DropConnection {
                    handle,
                    connection: connection.clone(),
                }
            }
            (FaultAction::Heal, _) => seedbox_engine_protocol::control::Action::Heal { handle },
            _ => panic!("not a network fixture record"),
        }
    };
    let mut specs = initial();
    specs[1]
        .sends
        .push_back((1, tx(frames::tcp(1, 2, (1234, 80), 7, 0, 2))));
    let mut d = net(specs, setup(0), 1);
    advance(&mut d, 3);
    let c = flow(&d);
    schedule(
        &mut d,
        "shape",
        shape(policy(20), NetworkFaults::default()),
        Some("net"),
    );
    let record = settled(&d.drain_events(), "shape").0;
    assert_eq!(
        project(&record),
        seedbox_engine_protocol::control::Action::Shape {
            handle: "net".into(),
            from: vec!["fixture-1".into()],
            to: vec!["fixture-2".into()],
            latency_ns: 20,
            jitter_ns: 0,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
            network_faults: NetworkFaults::default()
        }
    );
    schedule(
        &mut d,
        "drop",
        Action::DropConnection {
            connection: c.clone(),
        },
        Some("tcp"),
    );
    assert_eq!(
        project(&settled(&d.drain_events(), "drop").0),
        seedbox_engine_protocol::control::Action::DropConnection {
            handle: "tcp".into(),
            connection: c
        }
    );
    schedule(
        &mut d,
        "heal",
        Action::Heal(HealTarget::Input(key("shape"))),
        None,
    );
    assert_eq!(
        project(&settled(&d.drain_events(), "heal").0),
        seedbox_engine_protocol::control::Action::Heal {
            handle: "net".into()
        }
    );
}
#[test]
fn baseline_unknown_and_unattached_members_fail_before_factory_or_context_work() {
    for to in ["absent", "fixture-1"] {
        let mut oracle = fixture::definition(0);
        oracle.network = Some(
            crate::definition::NetworkInterface::new(
                crate::definition::MacAddress::new(frames::mac(0)).unwrap(),
                [10, 0, 0, 1].into(),
                24,
            )
            .unwrap(),
        );
        let setup = NetworkSetup::new(
            NetworkConfig::new(Duration::from_nanos(10), Duration::ZERO, 0).unwrap(),
            None,
            None,
            vec![LinkPolicy {
                from: oracle.name.clone(),
                to: to.into(),
                policy: policy(20),
            }],
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let contexts = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let cx = contexts.clone();
        let result = Driver::<Mock>::new_seeded(
            at(0),
            Duration::from_nanos(10),
            1,
            51,
            move || {
                cx.fetch_add(1, Ordering::SeqCst);
                Ok(Context(None))
            },
            oracle,
            vec![fixture::definition(1)],
            vec![],
            setup,
            move |_| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(Mock::new(spec(0, true, vec![])))
            },
        );
        assert!(matches!(result, Err(Error::NetworkConfiguration(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(contexts.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn structured_counter_pairs_survive_colliding_display_names_and_restore() {
    let mut specs: Vec<_> = (0..4)
        .map(|i| spec(i, true, vec![(0, Response::Ready)]))
        .collect();
    for (s, name) in specs.iter_mut().zip(["a->b", "c", "a", "b->c"]) {
        s.name = name.into();
    }
    specs[0].sends.extend([
        (1, tx(frames::plain(0, 1, 1))),
        (2, tx(frames::plain(0, 1, 2))),
        (21, tx(frames::plain(0, 1, 3))),
    ]);
    specs[2].sends.extend([
        (1, tx(frames::plain(2, 3, 4))),
        (22, tx(frames::plain(2, 3, 5))),
        (23, tx(frames::plain(2, 3, 6))),
    ]);
    let mut d = net(specs, setup(8), 1);
    advance(&mut d, 25);
    let before = d.execution.network.counts();
    let a = NetworkPair {
        from: MachineId(0),
        to: MachineId(1),
    };
    let b = NetworkPair {
        from: MachineId(2),
        to: MachineId(3),
    };
    assert_eq!(before.len(), 2);
    assert_eq!(before[&a].delivered, 2);
    assert_eq!(before[&b].delivered, 1);
    let (bytes, store) = artifact(d.checkpoint().unwrap());
    reference_observer_session(&mut d);
    d.set_frame_capacity(64);
    let events = advance(&mut d, 50);
    let after = d.execution.network.counts();
    let facts = d.drain_observations().frames;
    assert_eq!(after[&a].delivered, 3);
    assert_eq!(after[&b].delivered, 3);
    drop(d);
    for workers in [1, 4] {
        let mut restored = restore(&decode(&bytes, &store), workers);
        assert_eq!(restored.execution.network.counts(), before);
        assert_eq!(restored.drain_observations().frames, FrameFacts::default());
        restored.set_frame_capacity(64);
        assert_eq!(advance(&mut restored, 50), events);
        assert_eq!(restored.execution.network.counts(), after);
        assert_eq!(restored.drain_observations().frames, facts);
    }
}
