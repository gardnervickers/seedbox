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
    assert!(!f.complete());
    f.effect(0, 5, 14, true);
    assert_eq!(f.apply_due(15), vec![(0, b"start 6\n".to_vec())]);
    f.effect(0, 6, 16, true);
    assert!(f.complete());
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
        assert!(!f.complete());
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
        json!({"id":1,"at_ns":10,"deadline_ns":9,"action":"stop","machine":"n1"}),
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"stop","machine":"missing"}),
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
    assert_eq!(events::pop_due(&mut pending, 12), Some(b"after".as_slice()));
    assert!(pending.is_empty());
}
#[test]
fn same_time_partition_then_heal_completes_in_author_order_before_new_sends() {
    let mut f = engine::Faults::new(vec![
        json!({"id":1,"at_ns":10,"deadline_ns":20,"action":"partition","handle":"cut","left":["n1"],"right":["n2"]}),
        json!({"id":2,"at_ns":10,"deadline_ns":20,"action":"heal","handle":"cut"}),
    ],vec!["n1".into(),"n2".into()]).unwrap();
    f.apply_due(10);
    assert!(f.complete());
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
            machine: "n1".into(),
        },
    })
    .unwrap();
    assert_eq!(f.pending_boundaries(), 1);
    f.apply_due(2000);
    assert_eq!(f.pending_boundaries(), 1);
    f.effect(0, 2001, 2001, true);
    assert!(f.complete());
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
    assert!(f.complete());
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
