#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/connections.rs"]
mod connections;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/links.rs"]
mod links;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/rng.rs"]
mod rng;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/triggers.rs"]
mod triggers;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/faults.rs"]
mod faults;
use seedbox_control::{Action, MAX_TRIGGERS};
use triggers::Triggers;

fn action(id: &str) -> Action {
    Action::Partition {
        handle: id.into(),
        left: vec!["a".into()],
        right: vec!["b".into()],
    }
}
fn faults() -> faults::Faults {
    faults::Faults::new(vec![], vec!["a".into(), "b".into()]).unwrap()
}
fn bytes(t: &mut Triggers, source: &str, at: u64, bytes: &[u8]) {
    for b in bytes {
        t.committed(source, at, *b, 20, at + 15).unwrap();
    }
}

#[test]
fn split_overlapping_markers_duplicates_old_bytes_and_exactly_once() {
    let mut t = Triggers::default();
    t.arm("a".into(), b"ababac".to_vec(), 100, 1000, action("p"))
        .unwrap();
    bytes(&mut t, "a", 99, b"ababacabab"); // No old complete or partial prefix.
    bytes(&mut t, "b", 105, b"ababac"); // Only the selected console.
    bytes(&mut t, "a", 110, b"ab");
    assert_eq!(t.records[0].prefix, 2);
    bytes(&mut t, "a", 111, b"abab"); // KMP fallback across writes.
    assert_eq!(t.records[0].prefix, 4);
    bytes(&mut t, "a", 112, b"acababacababac");
    assert_eq!(t.records[0].matched_at_ns, Some(112));
    assert_eq!(t.records[0].seen, 8);
    assert_eq!(t.boundary(), 132);
    let mut f = faults();
    t.apply_boundary(&mut f, 132).unwrap();
    t.apply_boundary(&mut f, 132).unwrap();
    bytes(&mut t, "a", 133, b"ababac");
    assert_eq!(f.log.len(), 1);
    assert_eq!(f.log[0]["request"]["at_ns"], 132);
    f.apply_due(132);
    assert!(f.blocked(0, 1));
    assert_eq!(t.records[0].status, "fired");
}

#[test]
fn cancel_expire_and_cutoff_keep_partial_and_pending_effect_state() {
    let mut t = Triggers::default();
    let id = t
        .arm("a".into(), b"abc".to_vec(), 100, 10, action("p"))
        .unwrap();
    bytes(&mut t, "a", 105, b"ab");
    let snapshot = t.clone();
    t.cancel(id, 106).unwrap();
    assert!(t.cancel(id, 106).is_err());
    bytes(&mut t, "a", 107, b"c");
    assert_eq!(t.records[0].prefix, 2);
    assert_eq!(t.boundary(), u64::MAX);
    let mut restored = snapshot.clone();
    bytes(&mut restored, "a", 110, b"c"); // Exclusive deadline.
    restored.apply_boundary(&mut faults(), 110).unwrap();
    assert_eq!(restored.records[0].status, "expired");
    let mut cutoff = snapshot.clone();
    cutoff.cut_off(109);
    assert_eq!(cutoff.records[0].status, "cut_off");
    assert_eq!(cutoff.records[0].prefix, 2);
    let mut matched = snapshot;
    bytes(&mut matched, "a", 109, b"c");
    assert!(matched.cancel(id, 109).is_err());
    matched.cut_off(115);
    assert_eq!(matched.records[0].matched_at_ns, Some(109));
    assert_eq!(matched.records[0].effect_at_ns, Some(129));
    assert_eq!(matched.records[0].fault_id, None);
}

#[test]
fn reversed_source_ties_still_dispatch_by_arm_id_and_lagging_peer_has_no_past_effect() {
    let mut expected = None;
    for reverse in [false, true] {
        let mut t = Triggers::default();
        t.arm("a".into(), vec![1], 100, 100, action("first"))
            .unwrap();
        t.arm("b".into(), vec![1], 100, 100, action("second"))
            .unwrap();
        for source in if reverse { ["b", "a"] } else { ["a", "b"] } {
            bytes(&mut t, source, 110, &[1]);
        }
        let mut f = faults();
        t.apply_boundary(&mut f, 129).unwrap();
        assert!(f.log.is_empty()); // Nothing changes a peer before effect time.
        t.apply_boundary(&mut f, 130).unwrap();
        assert_eq!(f.log[0]["request"]["handle"], "first");
        assert_eq!(f.log[1]["request"]["handle"], "second");
        let evidence = (t.evidence().unwrap(), f.log);
        if let Some(expected) = &expected {
            assert_eq!(&evidence, expected)
        } else {
            expected = Some(evidence);
        }
    }
    let mut t = Triggers::default();
    t.arm("a".into(), vec![1], 100, 100, action("p")).unwrap();
    assert!(t.committed("a", 110, 1, 20, 131).is_err());
}

#[test]
fn matcher_storage_is_bounded_and_unused_evidence_is_absent() {
    let mut t = Triggers::default();
    for _ in 0..1000 {
        bytes(&mut t, "a", 100, b"ignored");
    }
    assert_eq!(t.evidence().unwrap(), None);
    assert_eq!(t.boundary(), u64::MAX);
    for marker in [vec![], vec![1; 257]] {
        assert!(t.arm("a".into(), marker, 100, 100, action("p")).is_err());
    }
    assert!(t.arm("a".into(), vec![1], 100, 0, action("p")).is_err());
    assert!(t
        .arm("a".into(), vec![1], u64::MAX, 1, action("p"))
        .is_err());
    for _ in 0..MAX_TRIGGERS {
        t.arm("a".into(), vec![1; 256], 100, 100, action("p"))
            .unwrap();
    }
    assert!(t.arm("a".into(), vec![1], 100, 100, action("p")).is_err());
    for _ in 0..10_000 {
        bytes(&mut t, "a", 101, &[0]);
    }
    assert_eq!(t.records.len(), MAX_TRIGGERS);
    assert!(t.records.iter().all(|r| r.prefix == 0 && r.seen == 10_000));
}

#[test]
fn snapshot_restores_prefix_and_pending_effect_and_crash_discards_partial_marker() {
    let mut t = Triggers::default();
    t.arm("a".into(), b"abc".to_vec(), 100, 100, action("p"))
        .unwrap();
    bytes(&mut t, "a", 101, b"ab");
    let snapshot = t.clone();
    t.reset_source("a");
    bytes(&mut t, "a", 102, b"c");
    assert!(t.records[0].matched_at_ns.is_none());
    assert_eq!(t.records[0].resets, 1);
    let mut restored = snapshot.clone();
    bytes(&mut restored, "a", 102, b"c");
    let matched = restored.clone();
    let mut f = faults();
    restored.apply_boundary(&mut f, 122).unwrap();
    let want = (restored.evidence().unwrap(), f.log.clone());
    let mut restored = matched;
    let mut f = faults();
    restored.apply_boundary(&mut f, 122).unwrap();
    assert_eq!((restored.evidence().unwrap(), f.log), want);
}

#[test]
fn fired_stop_uses_existing_effect_deadline_and_cutoff_does_not_fabricate_ack() {
    let mut t = Triggers::default();
    t.arm(
        "a".into(),
        vec![1],
        100,
        100,
        Action::Stop {
            machine: "b".into(),
        },
    )
    .unwrap();
    bytes(&mut t, "a", 110, &[1]);
    let mut f = faults();
    t.apply_boundary(&mut f, 130).unwrap();
    assert_eq!(f.apply_due(130), vec![(1, b"stop 1\n".to_vec())]);
    let snapshot = f.clone();
    t.cut_off(140);
    assert_eq!(f.log[0]["applied_at_ns"], serde_json::Value::Null);
    assert_eq!(t.records[0].status, "fired");
    f.apply_due(5_000_000_130);
    assert!(f.error.is_some());
    assert_eq!(f.log[0]["status"], "failed");
    let mut restored = snapshot;
    restored.effect(1, 1, 140, true);
    assert!(restored.error.is_none());
    assert_eq!(restored.log[0]["status"], "applied");
}

#[test]
fn witness_omits_unused_state_and_covers_prefix_cutoff_and_future_effect() {
    use sha2::{Digest, Sha256};
    let hash = |t: &Triggers| {
        let mut h = Sha256::new();
        h.update(b"existing witness bytes");
        t.hash_into(&mut h).unwrap();
        h.finalize()
    };
    let mut unchanged = Sha256::new();
    unchanged.update(b"existing witness bytes");
    let mut t = Triggers::default();
    assert_eq!(hash(&t), unchanged.finalize());
    t.arm("a".into(), b"abc".to_vec(), 100, 100, action("p"))
        .unwrap();
    let armed = hash(&t);
    bytes(&mut t, "a", 110, b"ab");
    let partial = hash(&t);
    assert_ne!(partial, armed);
    let snapshot = t.clone();
    bytes(&mut t, "a", 111, b"c");
    assert_ne!(hash(&t), partial);
    let matched = t.clone();
    t.apply_boundary(&mut faults(), 131).unwrap();
    assert_ne!(hash(&t), hash(&matched));
    let mut cutoff = snapshot.clone();
    cutoff.cut_off(112);
    assert_ne!(hash(&cutoff), partial);
    assert_eq!(hash(&snapshot), partial);
}

#[test]
fn stop_trigger_checks_paused_and_down_machine_admission() {
    use serde_json::json;
    for (op, status) in [("pause", "rejected"), ("crash", "fired")] {
        let mut f = faults::Faults::new(
            vec![json!({"id":1,"at_ns":100,"deadline_ns":100,"action":op,"machine":"b"})],
            vec!["a".into(), "b".into()],
        )
        .unwrap();
        f.apply_due(100);
        let mut t = Triggers::default();
        t.arm(
            "a".into(),
            vec![1],
            100,
            100,
            Action::Stop {
                machine: "b".into(),
            },
        )
        .unwrap();
        bytes(&mut t, "a", 110, &[1]);
        t.apply_boundary(&mut f, 130).unwrap();
        assert_eq!(t.records[0].status, status);
        if op == "pause" {
            assert_eq!(
                t.records[0].rejection_reason.as_deref(),
                Some("target machine is paused")
            );
            assert_eq!(t.records[0].fault_id, None);
            assert_eq!(f.log.len(), 1);
        } else {
            assert_eq!(t.records[0].fault_id, Some(2));
            assert!(f.apply_due(130).is_empty());
            assert_eq!(f.outcome(2).unwrap().0, faults::Status::Unavailable(130));
        }
        assert!(f.error.is_none());
    }
}

#[test]
fn triggered_partition_dominates_live_shape_and_heal_reveals_it() {
    use seedbox_control::Plan;
    let mut f = faults();
    f.append(Plan {
        id: 0,
        at_ns: 100,
        deadline_ns: 100,
        action: Action::Shape {
            handle: "shape".into(),
            from: vec!["a".into()],
            to: vec!["b".into()],
            latency_ns: 30,
            jitter_ns: 0,
            loss_ppm: 0,
        },
    })
    .unwrap();
    f.apply_due(100);
    let mut t = Triggers::default();
    t.arm("a".into(), vec![1], 100, 100, action("cut")).unwrap();
    bytes(&mut t, "a", 110, &[1]);
    t.apply_boundary(&mut f, 130).unwrap();
    f.apply_due(130);
    assert_eq!(t.records[0].fault_id, Some(2));
    let mut reference = f.links.clone();
    assert_eq!(
        f.links.decide(0, 1, 130, f.blocked(0, 1)),
        links::Outcome::Cut
    );
    f.append(Plan {
        id: 0,
        at_ns: 140,
        deadline_ns: 140,
        action: Action::Heal {
            handle: "cut".into(),
        },
    })
    .unwrap();
    f.apply_due(140);
    assert_eq!(f.active_handles(), vec!["shape"]);
    assert_eq!(
        f.links.decide(0, 1, 140, f.blocked(0, 1)),
        reference.decide(0, 1, 140, false)
    );
    assert_eq!(f.links.policy((0, 1)).latency_ns, 30);
    assert!(f.error.is_none());
}
