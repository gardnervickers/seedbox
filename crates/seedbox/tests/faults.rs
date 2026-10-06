#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/faults.rs"]
mod engine;
use serde_json::json;

fn fixture() -> engine::Faults {
    engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"a","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":11,"deadline_ns":21,"action":"partition","handle":"b","left":["n1"],"right":["n2"]}),
        json!({"id":3,"at_ns":12,"deadline_ns":22,"action":"heal","handle":"a"}),
        json!({"id":4,"at_ns":13,"deadline_ns":23,"action":"heal","handle":"b"}),
        json!({"id":5,"at_ns":14,"deadline_ns":24,"action":"stop","service":"n1"}),
        json!({"id":6,"at_ns":15,"deadline_ns":25,"action":"restart","service":"n1"}),
    ], vec!["n1".into(),"n2".into(),"client".into()]).unwrap()
}
#[test]
fn overlapping_handles_only_heal_their_own_links_and_order_is_stable() {
    let mut f = fixture();
    assert_eq!(f.next_boundary(), 10);
    f.apply_due(10);
    assert!(f.blocked(0, 1));
    assert!(f.blocked(1, 0));
    assert!(!f.blocked(0, 2));
    f.apply_due(11);
    f.apply_due(12);
    assert!(f.blocked(0, 1));
    f.apply_due(13);
    assert!(!f.blocked(0, 1));
    assert_eq!(f.apply_due(14), vec![(0, b"stop 5 n1\n".to_vec())]);
    assert!(!(f.error.is_none() && f.pending_boundaries() == 0));
    f.effect(0, 5, 14, true);
    assert_eq!(f.apply_due(15), vec![(0, b"start 6 n1\n".to_vec())]);
    f.effect(0, 6, 16, true);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
    assert_eq!(f.log[5]["applied_at_ns"], 16);
    assert_eq!(f.next_boundary(), u64::MAX);
}
#[test]
fn ack_identity_machine_effect_time_deadline_and_state_fail_closed() {
    for (machine, id, time, success) in [
        (1, 5, 14, true),
        (0, 99, 14, true),
        (0, 5, 13, true),
        (0, 5, 25, true),
        (0, 5, 14, false),
    ] {
        let mut f = fixture();
        f.apply_due(14);
        f.effect(machine, id, time, success);
        assert!(f.error.is_some());
        assert!(!(f.error.is_none() && f.pending_boundaries() == 0));
    }
    let mut f = fixture();
    f.apply_due(14);
    f.effect(0, 5, 14, true);
    f.effect(0, 5, 14, true);
    assert!(f.error.is_some());
    assert_eq!(f.log[4]["status"], "failed");
    let mut f = fixture();
    f.apply_due(14);
    f.apply_due(24);
    assert!(f.error.is_some());
    assert_eq!(f.log[4]["status"], "failed");
    let mut f = fixture();
    f.effect(0, 5, 14, true);
    assert!(f.error.is_some());
}
#[test]
fn malformed_world_faults_return_errors_instead_of_panicking() {
    for value in [
        json!({}),
        json!({"id":1,"at_ns":10,"deadline_ns":9,"action":"stop","service":"n1"}),
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"stop","service":"missing"}),
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"p","left":["n1"],"right":["n1"]}),
    ] {
        assert!(engine::Faults::new(vec![value], vec!["n1".into()]).is_err());
    }
}

#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/events.rs"]
mod events;
#[test]
fn already_routed_frames_survive_same_time_partition_and_events_keep_sequence() {
    use std::collections::BTreeMap;
    let mut f = engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"cut","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":11,"deadline_ns":21,"action":"heal","handle":"cut"}),
    ],vec!["n1".into(),"n2".into()]).unwrap();
    // These frames passed the production send-time partition gate before10.
    assert!(!f.blocked(0, 1));
    let mut pending = BTreeMap::from([
        ((10, 2), b"second".as_slice()),
        ((10, 1), b"first".as_slice()),
        ((12, 3), b"after".as_slice()),
    ]);
    assert!(events::pop_due(&mut pending, 9).is_none());
    f.apply_due(10);
    assert!(f.blocked(0, 1)); // New same-time sends are dropped after the fault.
    assert_eq!(events::pop_due(&mut pending, 10), Some(b"first".as_slice()));
    assert_eq!(
        events::pop_due(&mut pending, 10),
        Some(b"second".as_slice())
    );
    assert!(events::pop_due(&mut pending, 10).is_none());
    f.apply_due(11);
    assert!(!f.blocked(0, 1));
    assert_eq!(
        events::pop_due_at(&mut pending, 14),
        Some((12, b"after".as_slice()))
    );
    assert!(pending.is_empty());
}
#[test]
fn same_time_partition_then_heal_completes_in_author_order_before_new_sends() {
    let mut f = engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"cut","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":10,"deadline_ns":20,"action":"heal","handle":"cut"}),
    ],vec!["n1".into(),"n2".into()]).unwrap();
    f.apply_due(10);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
    assert!(!f.blocked(0, 1));
    assert_eq!(
        f.log
            .iter()
            .map(|r| r["action"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["partition", "heal"]
    );
}

#[test]
fn scheduling_cost_tracks_only_outstanding_work_not_completed_history() {
    use seedbox_control::{Action, Plan};
    let mut f = engine::Faults::new(vec![], vec!["n1".into(), "n2".into()]).unwrap();
    for at in 0..1000 {
        let handle = format!("p{at}");
        f.append(Plan {
            id: 0,
            at_ns: at * 2,
            deadline_ns: at * 2 + 10,
            action: Action::Partition {
                handle: handle.clone(),
                left: vec!["n1".into()],
                right: vec!["n2".into()],
            },
        })
        .unwrap();
        assert_eq!(f.pending_boundaries(), 1);
        f.apply_due(at * 2);
        f.append(Plan {
            id: 0,
            at_ns: at * 2 + 1,
            deadline_ns: at * 2 + 11,
            action: Action::Heal { handle },
        })
        .unwrap();
        f.apply_due(at * 2 + 1);
        assert_eq!(f.pending_boundaries(), 0);
        assert_eq!(f.next_boundary(), u64::MAX);
    }
    assert_eq!(f.log.len(), 2000);
    f.append(Plan {
        id: 0,
        at_ns: 2000,
        deadline_ns: 2010,
        action: Action::Stop {
            service: "n1".into(),
        },
    })
    .unwrap();
    assert_eq!(f.pending_boundaries(), 1);
    f.apply_due(2000);
    assert_eq!(f.pending_boundaries(), 1);
    f.effect(0, 2001, 2001, true);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
}
#[test]
fn process_commands_wait_for_ready_and_undispatched_acks_cannot_fulfill_them() {
    use seedbox_control::{Action, Plan};
    let plan = Plan {
        id: 0,
        at_ns: 10,
        deadline_ns: 100,
        action: Action::Stop {
            service: "n1".into(),
        },
    };
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan.clone()).unwrap();
    assert!(f.apply_due(10).is_empty());
    f.management_ready(0, 20, 5).unwrap();
    assert_eq!(f.next_boundary(), 25);
    assert!(f.apply_due(24).is_empty());
    assert_eq!(f.apply_due(25), vec![(0, b"stop 1 n1\n".to_vec())]);
    f.effect(0, 1, 30, true);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
    assert_eq!(f.pending_boundaries(), 0);
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan.clone()).unwrap();
    f.apply_due(10);
    f.effect(0, 1, 15, true);
    assert!(f.error.is_some());
    assert_eq!(f.log[0]["status"], "failed");
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan).unwrap();
    f.apply_due(10);
    f.apply_due(100);
    assert!(f.error.is_some());
    assert_eq!(f.log[0]["status"], "failed");
}

fn shared() -> engine::Faults {
    let placement = ["a", "b", "scenario"]
        .into_iter()
        .map(|s| (s.to_owned(), "kernel".to_owned()))
        .collect();
    engine::Faults::with_placement(vec![], vec!["kernel".into()], &placement).unwrap()
}
#[test]
fn co_located_partitions_are_applied_and_acknowledged_by_the_shared_kernel() {
    use seedbox_control::{Action, Plan};
    let mut f = shared();
    let partition = Action::Partition {
        handle: "p1".into(),
        left: vec!["a".into()],
        right: vec!["b".into(), "scenario".into()],
    };
    f.append(Plan {
        id: 0,
        at_ns: 10,
        deadline_ns: 100,
        action: partition,
    })
    .unwrap();
    assert_eq!(
        f.apply_due(10),
        vec![(0, b"partition 1 p1 a b,scenario\n".to_vec())]
    );
    // No fabric link exists inside one machine; the kernel owns the cut.
    assert!(!f.blocked(0, 0));
    assert_eq!(f.active_handles(), ["p1"]);
    assert_eq!(f.log[0]["status"], "requested");
    f.effect(0, 1, 12, true);
    assert_eq!(f.log[0]["status"], "applied");
    assert_eq!(f.log[0]["applied_at_ns"], 12);
    assert_eq!(f.log[0]["machine"], "kernel");
    f.append(Plan {
        id: 0,
        at_ns: 20,
        deadline_ns: 110,
        action: Action::Heal {
            handle: "p1".into(),
        },
    })
    .unwrap();
    assert_eq!(f.apply_due(20), vec![(0, b"heal 2 p1\n".to_vec())]);
    assert!(f.active_handles().is_empty());
    f.effect(0, 2, 21, true);
    f.append(Plan {
        id: 0,
        at_ns: 30,
        deadline_ns: 120,
        action: Action::Stop {
            service: "b".into(),
        },
    })
    .unwrap();
    assert_eq!(f.apply_due(30), vec![(0, b"stop 3 b\n".to_vec())]);
    f.effect(0, 3, 31, true);
    assert_eq!(f.process_state("b"), "Stopped");
    assert_eq!(f.process_state("a"), "Running");
    assert_eq!(f.log[2]["service"], "b");
    assert!(f.error.is_none() && f.pending_boundaries() == 0);
}
#[test]
fn co_located_cuts_need_an_acknowledgement_and_readiness() {
    use seedbox_control::{Action, Plan};
    let cut = |at| Plan {
        id: 0,
        at_ns: at,
        deadline_ns: at + 50,
        action: Action::Partition {
            handle: "p1".into(),
            left: vec!["a".into()],
            right: vec!["b".into()],
        },
    };
    let mut f = shared();
    f.require_readiness();
    f.append(cut(10)).unwrap();
    assert!(f.apply_due(10).is_empty());
    f.management_ready(0, 15, 5).unwrap();
    assert_eq!(f.apply_due(20), vec![(0, b"partition 1 p1 a b\n".to_vec())]);
    f.apply_due(60);
    assert!(f.error.is_some());
    assert_eq!(f.log[0]["status"], "failed");
    let mut f = shared();
    f.append(cut(10)).unwrap();
    f.apply_due(10);
    f.effect(0, 1, 11, false);
    assert_eq!(f.error.as_deref(), Some("guest rejected fault 1"));
}
#[test]
fn partitions_cannot_mix_shared_and_separate_machines() {
    let placement = [("a", "m1"), ("b", "m1"), ("c", "m2")]
        .into_iter()
        .map(|(s, m)| (s.to_owned(), m.to_owned()))
        .collect();
    let plan = json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"p","left":["a"],"right":["b","c"]});
    assert!(
        engine::Faults::with_placement(vec![plan], vec!["m1".into(), "m2".into()], &placement)
            .is_err()
    );
    let bad = [("a b", "m1")]
        .into_iter()
        .map(|(s, m)| (s.to_owned(), m.to_owned()))
        .collect();
    assert!(engine::Faults::with_placement(vec![], vec!["m1".into()], &bad).is_err());
}

#[test]
fn co_located_service_exits_are_tracked_for_inspection_and_restart() {
    use seedbox_control::{Action, Plan};
    let mut f = shared();
    f.service_exited(0, 5, b"SBX_EXIT1 b 17");
    assert!(f.error.is_none());
    assert_eq!(f.process_state("b"), "Exited(17)");
    assert_eq!(f.exits[0]["service"], "b");
    assert_eq!(f.exits[0]["status"], 17);
    let stop = Action::Stop {
        service: "b".into(),
    };
    let restart = Action::Restart {
        service: "b".into(),
    };
    assert!(f.admission_error(&stop).is_some());
    assert!(f.admission_error(&restart).is_none());
    f.append(Plan {
        id: 0,
        at_ns: 10,
        deadline_ns: 100,
        action: restart.clone(),
    })
    .unwrap();
    assert_eq!(f.apply_due(10), vec![(0, b"start 1 b\n".to_vec())]);
    f.effect(0, 1, 12, true);
    assert_eq!(f.process_state("b"), "Running");
    assert!(f.admission_error(&restart).is_some());
    for bad in [
        &b"SBX_EXIT1 unknown 1"[..],
        b"SBX_EXIT1 a",
        b"SBX_EXIT1 a 256",
    ] {
        let mut f = shared();
        f.service_exited(0, 5, bad);
        assert!(f.error.is_some());
    }
    let mut f = shared();
    f.service_exited(0, 5, b"SBX_EXIT1 a 0");
    f.service_exited(0, 6, b"SBX_EXIT1 a 0");
    assert!(
        f.error.is_some(),
        "a second exit needs a restart in between"
    );
}
