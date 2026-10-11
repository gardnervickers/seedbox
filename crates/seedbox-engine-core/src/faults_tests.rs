use crate::faults as engine;
use crate::links;
use serde_json::json;

fn fixture() -> engine::Faults {
    engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"a","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":11,"deadline_ns":21,"action":"partition","handle":"b","left":["n1"],"right":["n2"]}),
        json!({"id":3,"at_ns":12,"deadline_ns":22,"action":"heal","handle":"a"}),
        json!({"id":4,"at_ns":13,"deadline_ns":23,"action":"heal","handle":"b"}),
        json!({"id":5,"at_ns":14,"deadline_ns":24,"action":"stop","machine":"n1"}),
        json!({"id":6,"at_ns":15,"deadline_ns":25,"action":"restart","machine":"n1"}),
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
    assert_eq!(f.apply_due(14), vec![(0, b"stop 5\n".to_vec())]);
    assert!(!(f.error.is_none() && f.pending_boundaries() == 0));
    f.effect(0, 5, 14, true);
    assert_eq!(f.apply_due(15), vec![(0, b"start 6\n".to_vec())]);
    f.effect(0, 6, 16, true);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
    assert_eq!(f.log[5].json()["applied_at_ns"], 16);
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
    assert_eq!(f.log[4].json()["status"], "failed");
    let mut f = fixture();
    f.apply_due(14);
    f.apply_due(24);
    assert!(f.error.is_some());
    assert_eq!(f.log[4].json()["status"], "failed");
    let mut f = fixture();
    f.effect(0, 5, 14, true);
    assert!(f.error.is_some());
}
#[test]
fn malformed_world_faults_return_errors_instead_of_panicking() {
    for value in [
        json!({}),
        json!({"id":1,"at_ns":10,"deadline_ns":9,"action":"stop","machine":"n1"}),
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"stop","machine":"missing"}),
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"p","left":["n1"],"right":["n1"]}),
    ] {
        assert!(engine::Faults::new(vec![value], vec!["n1".into()]).is_err());
    }
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
        f.log.iter().map(|r| r.action.as_ref()).collect::<Vec<_>>(),
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
            machine: "n1".into(),
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
            machine: "n1".into(),
        },
    };
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan.clone()).unwrap();
    assert!(f.apply_due(10).is_empty());
    f.management_ready(0, 20, 5).unwrap();
    assert_eq!(f.next_boundary(), 25);
    assert!(f.apply_due(24).is_empty());
    assert_eq!(f.apply_due(25), vec![(0, b"stop 1\n".to_vec())]);
    f.effect(0, 1, 30, true);
    assert!((f.error.is_none() && f.pending_boundaries() == 0));
    assert_eq!(f.pending_boundaries(), 0);
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan.clone()).unwrap();
    f.apply_due(10);
    f.effect(0, 1, 15, true);
    assert!(f.error.is_some());
    assert_eq!(f.log[0].json()["status"], "failed");
    let mut f = engine::Faults::new(vec![], vec!["n1".into()]).unwrap();
    f.require_readiness();
    f.append(plan).unwrap();
    f.apply_due(10);
    f.apply_due(100);
    assert!(f.error.is_some());
    assert_eq!(f.log[0].json()["status"], "failed");
}

#[test]
fn same_time_overlapping_cuts_heal_by_handle_and_id_without_opening_other_links() {
    let mut faults = engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"a","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"b","left":["n1"],"right":["n2","n3"]}),
        json!({"id":3,"at_ns":10,"deadline_ns":20,"action":"heal","handle":"a"}),
        json!({"id":4,"at_ns":11,"deadline_ns":21,"action":"heal","handle":"b"}),
    ], vec!["n1".into(), "n2".into(), "n3".into()]).unwrap();
    assert!(faults.apply_due(9).is_empty());
    assert!(!faults.blocked(0, 1));
    assert!(faults.apply_due(10).is_empty());
    assert!(faults.error.is_none());
    assert_eq!(faults.active_handles(), vec!["b"]);
    for peer in [1, 2] {
        assert!(faults.blocked(0, peer));
        assert!(faults.blocked(peer, 0));
    }
    assert!(!faults.blocked(1, 2));
    assert!(!faults.blocked(2, 1));
    for log in &faults.log[..3] {
        assert_eq!(log.json()["status"], "applied");
        assert_eq!(log.json()["applied_at_ns"], 10);
    }
    assert!(faults.apply_due(11).is_empty());
    assert!(faults.error.is_none());
    assert!(faults.active_handles().is_empty());
    assert_eq!(faults.next_boundary(), u64::MAX);
    for peer in [1, 2] {
        assert!(!faults.blocked(0, peer));
        assert!(!faults.blocked(peer, 0));
    }
    assert_eq!(faults.log[3].json()["applied_at_ns"], 11);
}

#[test]
fn lifecycle_transitions_follow_the_machine_state_and_reject_the_rest() {
    use engine::{Life, Lifecycle};
    let mut f = lifecycle(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"pause","machine":"a"}),
        json!({"id":2,"at_ns":20,"deadline_ns":20,"action":"resume","machine":"a"}),
        json!({"id":3,"at_ns":30,"deadline_ns":30,"action":"pause","machine":"a"}),
        json!({"id":4,"at_ns":40,"deadline_ns":40,"action":"crash","machine":"a"}),
        json!({"id":5,"at_ns":50,"deadline_ns":50,"action":"reboot","machine":"a"}),
    ]);
    // Only the target's own requests bound its early window.
    assert_eq!((f.next_lifecycle(0), f.next_lifecycle(1)), (10, u64::MAX));
    let steps = [
        (10, Lifecycle::Pause, Life::Paused, "machine paused"),
        (20, Lifecycle::Resume, Life::Up, "machine running"),
        (30, Lifecycle::Pause, Life::Paused, "machine paused"),
        // A paused machine can crash: its frozen VM is destroyed.
        (40, Lifecycle::Crash, Life::Down, "machine down"),
        (50, Lifecycle::Reboot, Life::Up, "machine running"),
    ];
    for (i, (at, op, life, detail)) in steps.into_iter().enumerate() {
        assert!(f.apply_due(at - 1).is_empty());
        assert!(f.take_lifecycle().is_empty());
        assert!(f.apply_due(at).is_empty());
        assert_eq!(f.take_lifecycle(), vec![(0, op)]);
        assert_eq!(f.life(0), life);
        assert_eq!(f.log[i].json()["status"], "applied");
        assert_eq!(f.log[i].json()["applied_at_ns"], at);
        assert_eq!(f.log[i].json()["detail"], detail);
    }
    assert!(f.error.is_none());
    assert_eq!(f.next_lifecycle(0), u64::MAX);
    assert_eq!(f.pending_boundaries(), 0);

    for (first, second, error) in [
        ("resume", "pause", "resume of a running machine"),
        ("reboot", "pause", "reboot of a running machine"),
        ("pause", "pause", "pause of a paused machine"),
        ("pause", "reboot", "reboot of a paused machine"),
    ] {
        let mut f = lifecycle(vec![
            json!({"id":1,"at_ns":10,"deadline_ns":10,"action":first,"machine":"b"}),
            json!({"id":2,"at_ns":10,"deadline_ns":10,"action":second,"machine":"b"}),
        ]);
        f.apply_due(10);
        let failed = if f.log[0].json()["status"] == "failed" {
            0
        } else {
            1
        };
        assert_eq!(f.log[failed].json()["detail"], error);
        assert_eq!(f.error.as_deref(), Some(error));
        assert_eq!(f.take_lifecycle().len(), 1, "{first} then {second}");
    }
    let mut f = lifecycle(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"crash","machine":"b"}),
        json!({"id":2,"at_ns":11,"deadline_ns":11,"action":"crash","machine":"b"}),
    ]);
    f.apply_due(11);
    assert_eq!(f.error.as_deref(), Some("crash of a down machine"));
    assert_eq!(f.take_lifecycle(), vec![(1, Lifecycle::Crash)]);
}

/// Process control belongs to an incarnation: a crash makes its outstanding
/// stop unavailable, its acknowledgement cannot arrive later, and the next
/// incarnation must report readiness again before it is sent anything.
#[test]
fn a_crash_ends_the_incarnations_process_faults_and_readiness() {
    use engine::Lifecycle;
    let mut f = lifecycle(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":100,"action":"stop","machine":"a"}),
        json!({"id":2,"at_ns":20,"deadline_ns":20,"action":"crash","machine":"a"}),
        json!({"id":3,"at_ns":25,"deadline_ns":200,"action":"restart","machine":"a"}),
        json!({"id":4,"at_ns":30,"deadline_ns":30,"action":"reboot","machine":"a"}),
        json!({"id":5,"at_ns":40,"deadline_ns":300,"action":"stop","machine":"a"}),
    ]);
    f.require_readiness();
    f.management_ready(0, 5, 1).unwrap();
    assert_eq!(f.apply_due(10), vec![(0, b"stop 1\n".to_vec())]);
    assert!(f.apply_due(20).is_empty());
    assert_eq!(f.take_lifecycle(), vec![(0, Lifecycle::Crash)]);
    assert_eq!(f.log[0].json()["status"], "unavailable");
    assert_eq!(
        f.log[0].json()["detail"],
        "target machine crashed before the process effect"
    );
    assert_eq!(f.process_state(0), "Down");
    // A process fault for a machine that is down is unavailable at once.
    assert!(f.apply_due(25).is_empty());
    assert_eq!(f.log[2].json()["status"], "unavailable");
    assert_eq!(f.log[2].json()["detail"], "target machine is down");
    f.apply_due(30);
    assert_eq!(f.take_lifecycle(), vec![(0, Lifecycle::Reboot)]);
    assert_eq!(f.process_state(0), "Running");
    // The old acknowledgement, arriving now, matches nothing outstanding.
    f.effect(0, 1, 31, true);
    assert_eq!(
        f.error.as_deref(),
        Some("stale/duplicate/miscorrelated fault effect 1")
    );
    f.error = None;
    // The new incarnation is not ready yet, so the next stop waits for it.
    assert!(f.apply_due(40).is_empty());
    f.management_ready(0, 50, 1).unwrap();
    assert_eq!(f.next_boundary(), 51);
    assert_eq!(f.apply_due(51), vec![(0, b"stop 5\n".to_vec())]);
    f.effect(0, 5, 60, true);
    assert!(f.error.is_none());
    assert_eq!(f.process_state(0), "Stopped");
    assert_eq!(f.readiness.len(), 2);
}

/// Equal-time requests apply in plan order, and a command dispatched just
/// before a crash at the same time is made unavailable with it.
#[test]
fn equal_time_lifecycle_and_process_requests_apply_in_plan_order() {
    use engine::Lifecycle;
    let mut f = lifecycle(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":100,"action":"stop","machine":"b"}),
        json!({"id":2,"at_ns":10,"deadline_ns":10,"action":"crash","machine":"b"}),
        json!({"id":3,"at_ns":10,"deadline_ns":10,"action":"reboot","machine":"b"}),
        json!({"id":4,"at_ns":10,"deadline_ns":10,"action":"pause","machine":"a"}),
    ]);
    // The command is returned, but the world applies the crash after it and
    // discards what was queued for the dead incarnation.
    assert_eq!(f.apply_due(10), vec![(1, b"stop 1\n".to_vec())]);
    assert_eq!(
        f.take_lifecycle(),
        vec![
            (1, Lifecycle::Crash),
            (1, Lifecycle::Reboot),
            (0, Lifecycle::Pause)
        ]
    );
    assert_eq!(f.log[0].json()["status"], "unavailable");
    assert_eq!(
        (f.process_state(0), f.process_state(1)),
        ("Paused", "Running")
    );
}

#[test]
fn link_layers_and_cuts_survive_machine_lifecycle_and_heal_independently() {
    use engine::{Life, Lifecycle};
    let names = vec!["a".into(), "b".into()];
    let links = links::Links::new(
        1009,
        &names,
        links::Policy {
            latency_ns: 10,
            jitter_ns: 0,
            loss_ppm: 0,
            bandwidth_bps: None,
            queue_bound_ns: None,
        },
        std::collections::BTreeMap::new(),
    )
    .unwrap();
    let mut f = engine::Faults::with_links(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":10,"action":"shape","handle":"shape","from":["a"],"to":["b"],"latency_ns":30,"jitter_ns":0,"loss_ppm":0}),
        json!({"id":2,"at_ns":10,"deadline_ns":10,"action":"partition","handle":"cut","left":["a"],"right":["b"]}),
        json!({"id":3,"at_ns":10,"deadline_ns":10,"action":"pause","machine":"b"}),
        json!({"id":4,"at_ns":20,"deadline_ns":20,"action":"crash","machine":"b"}),
        json!({"id":5,"at_ns":30,"deadline_ns":30,"action":"reboot","machine":"b"}),
        json!({"id":6,"at_ns":40,"deadline_ns":40,"action":"heal","handle":"cut"}),
        json!({"id":7,"at_ns":50,"deadline_ns":50,"action":"heal","handle":"shape"}),
    ], names, links).unwrap();
    for (at, op, life) in [
        (10, Lifecycle::Pause, Life::Paused),
        (20, Lifecycle::Crash, Life::Down),
        (30, Lifecycle::Reboot, Life::Up),
    ] {
        f.apply_due(at);
        assert_eq!(f.take_lifecycle(), vec![(1, op)]);
        assert_eq!(f.life(1), life);
        assert_eq!(f.active_handles(), vec!["cut", "shape"]);
        assert!(f.blocked(0, 1));
        assert_eq!(f.links.policy((0, 1)).latency_ns, 30);
    }
    f.apply_due(40);
    assert!(!f.blocked(0, 1));
    assert_eq!(
        f.links.decide(0, 1, 40, 1500, false),
        links::Outcome::Deliver(70)
    );
    assert_eq!(f.links.policy((1, 0)).latency_ns, 10);
    f.apply_due(50);
    assert!(f.active_handles().is_empty());
    assert_eq!(f.links.policy((0, 1)).latency_ns, 10);
    assert!(f.error.is_none());
}

fn lifecycle(plans: Vec<serde_json::Value>) -> engine::Faults {
    engine::Faults::new(plans, vec!["a".into(), "b".into()]).unwrap()
}
