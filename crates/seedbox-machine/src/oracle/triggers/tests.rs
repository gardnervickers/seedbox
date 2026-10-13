use super::*;
fn arm(engine: &mut Triggers, source: &str, marker: &[u8], now: u64, timeout: u64) -> u32 {
    engine
        .arm(
            source.into(),
            marker.to_vec(),
            now,
            timeout,
            "target".into(),
        )
        .unwrap()
}

#[test]
fn limits_are_lifetime_bounds_and_failed_arm_does_not_allocate() {
    let mut engine = Triggers::default();
    for (marker, now, timeout) in [
        (vec![], 0, 1),
        (vec![0; 257], 0, 1),
        (vec![1], 0, 0),
        (vec![1], u64::MAX, 1),
        (vec![1], u64::MAX - PROCESS_EFFECT_TIMEOUT_NS, 1),
    ] {
        assert!(engine
            .arm("source".into(), marker, now, timeout, "target".into())
            .is_err());
        assert!(engine.records().is_empty());
    }
    assert_eq!(arm(&mut engine, "source", &[255; 256], 0, 1), 1);
    engine.cancel(1, 0).unwrap();
    for id in 2..=64 {
        assert_eq!(arm(&mut engine, "source", b"x", 0, 1), id);
    }
    engine.expire(1);
    assert!(engine
        .arm("source".into(), b"x".to_vec(), 1, 1, "target".into())
        .unwrap_err()
        .contains("budget"));
    assert_eq!(engine.records().len(), 64);
}

#[test]
fn streaming_overlap_matches_across_chunks_and_ignores_old_or_wrong_source_bytes() {
    let mut engine = Triggers::default();
    let id = arm(&mut engine, "source", b"ABABAC", 10, 100);
    assert!(engine
        .committed("source", 9, b"ABABA", 10)
        .unwrap()
        .is_empty());
    assert!(engine
        .committed("other", 11, b"ABABAC", 11)
        .unwrap()
        .is_empty());
    assert!(engine
        .committed("source", 11, b"ABA", 11)
        .unwrap()
        .is_empty());
    assert_eq!(engine.records()[0].prefix, 3);
    let mut branch = engine.clone();
    for e in [&mut engine, &mut branch] {
        let effects = e.committed("source", 12, b"BABACignored", 12).unwrap();
        assert_eq!(
            effects,
            [Effect {
                trigger_id: id,
                target: "target".into(),
                at: 12,
                deadline: 12 + PROCESS_EFFECT_TIMEOUT_NS
            }]
        );
        assert_eq!(e.records()[0].seen, 8);
        assert_eq!(e.records()[0].matched_at, Some(12));
        assert_eq!(e.boundary(), Some(12));
        assert!(e.committed("source", 13, b"ABABAC", 13).unwrap().is_empty());
    }
    assert_eq!(engine.records(), branch.records());
}

#[test]
fn exact_deadline_is_exclusive_and_cancel_expiry_tie_uses_caller_order() {
    let mut engine = Triggers::default();
    arm(&mut engine, "source", b"ab", 10, 10);
    engine.committed("source", 19, b"a", 19).unwrap();
    assert!(engine.committed("source", 20, b"b", 20).unwrap().is_empty());
    assert_eq!(engine.boundary(), Some(20));
    let mut cancelled = engine.clone();
    cancelled.cancel(1, 20).unwrap();
    cancelled.expire(20);
    assert_eq!(cancelled.records()[0].status, Status::Cancelled);
    engine.expire(20);
    assert_eq!(engine.records()[0].status, Status::Expired);
    assert!(engine.cancel(1, 20).is_err());
    assert!(engine.cancel(0, 20).is_err());
    assert!(engine.cancel(2, 20).is_err());
    assert_eq!(engine.boundary(), None);
}

#[test]
fn source_reboot_clears_prefix_without_cancelling_armed_trigger() {
    let mut engine = Triggers::default();
    arm(&mut engine, "source", b"abc", 0, 100);
    engine.committed("source", 1, b"ab", 1).unwrap();
    engine.reset_source("other").unwrap();
    assert_eq!(engine.records()[0].prefix, 2);
    engine.reset_source("source").unwrap();
    assert_eq!(engine.records()[0].prefix, 0);
    assert_eq!(engine.records()[0].resets, 1);
    assert_eq!(engine.records()[0].status, Status::Armed);
    assert!(engine.committed("source", 2, b"c", 2).unwrap().is_empty());
    engine.committed("source", 3, b"abc", 3).unwrap();
    engine.reset_source("source").unwrap();
    assert_eq!(engine.records()[0].resets, 1);
    assert_eq!(engine.records()[0].status, Status::Matched);
}

#[test]
fn different_observation_times_in_same_commit_admit_by_arm_id() {
    let mut engine = Triggers::default();
    arm(&mut engine, "first", b"x", 0, 100);
    arm(&mut engine, "second", b"x", 0, 100);
    let second = engine.committed("second", 9, b"x", 10).unwrap();
    let first = engine.committed("first", 3, b"x", 10).unwrap();
    assert_eq!(second[0].trigger_id, 2);
    assert_eq!(first[0].trigger_id, 1);
    assert_eq!((first[0].at, second[0].at), (10, 10));
    assert_eq!(engine.records()[0].matched_at, Some(3));
    assert_eq!(engine.records()[1].matched_at, Some(9));
    assert!(engine.due(3).is_empty());
    assert!(engine.due(9).is_empty());
    assert_eq!(
        engine
            .due(10)
            .iter()
            .map(|e| e.trigger_id)
            .collect::<Vec<_>>(),
        [1, 2]
    );
    assert!(engine.resolve(1, 9, Ok(7)).is_err());
    engine.resolve(1, 10, Ok(7)).unwrap();
    engine
        .resolve(2, 10, Err("target unavailable".into()))
        .unwrap();
    assert!(engine.due(10).is_empty());
    assert_eq!(engine.records()[0].status, Status::Fired);
    assert_eq!(engine.records()[0].fault_id, Some(7));
    assert_eq!(engine.records()[1].status, Status::Rejected);
    assert_eq!(
        engine.records()[1].rejection_reason.as_deref(),
        Some("target unavailable")
    );
    assert!(engine.resolve(1, 10, Ok(8)).is_err());
}

#[test]
fn near_end_round_match_uses_commit_barrier_and_keeps_observation_time() {
    let mut engine = Triggers::default();
    arm(&mut engine, "source", b"ab", 0, 10);
    arm(&mut engine, "source", b"c", 0, 100);
    engine.committed("source", 1, b"a", 5).unwrap();
    let effects = engine.committed("source", 9, b"b", 10).unwrap();
    assert_eq!(effects[0].at, 10);
    assert_eq!(effects[0].deadline, 10 + PROCESS_EFFECT_TIMEOUT_NS);
    assert_eq!(engine.records()[0].matched_at, Some(9));
    assert_eq!(engine.records()[0].effect_at, Some(10));
    assert!(engine.due(9).is_empty());
    engine.expire(10); // Match was before the exclusive deadline despite commit at it.
    assert_eq!(engine.records()[0].status, Status::Matched);
    engine.resolve(1, 10, Ok(1)).unwrap();
    let later = engine.committed("source", 11, b"c", 12).unwrap();
    assert_eq!(later[0].at, 12);
    assert_eq!(engine.records()[0].effect_at, Some(10));
    assert_eq!(engine.boundary(), Some(12));
}

#[test]
fn future_observation_and_commit_deadline_overflow_are_rejected_without_backdating() {
    for (observed, committed, expected) in [
        (4, 3, "observation exceeds commit"),
        (
            3,
            u64::MAX - PROCESS_EFFECT_TIMEOUT_NS + 1,
            "effect deadline overflow",
        ),
    ] {
        let mut engine = Triggers::default();
        arm(&mut engine, "source", b"x", 0, 100);
        let before = engine.records().to_vec();
        let error = engine
            .committed("source", observed, b"x", committed)
            .unwrap_err();
        assert!(error.contains(expected), "{error}");
        assert_eq!(engine.records(), before);
    }
    let mut engine = Triggers::default();
    arm(&mut engine, "source", b"x", 0, 100);
    let latest = u64::MAX - PROCESS_EFFECT_TIMEOUT_NS;
    assert_eq!(
        engine.committed("source", 3, b"x", latest).unwrap()[0].deadline,
        u64::MAX
    );
    assert_eq!(engine.records()[0].matched_at, Some(3));
    assert!(engine.due(latest - 1).is_empty());
    assert_eq!(engine.due(latest)[0].at, latest);
}

#[test]
fn cutoff_preserves_terminal_records_and_cancels_armed_and_matched_effects() {
    let mut engine = Triggers::default();
    for source in ["cancelled", "fired", "rejected", "armed", "matched"] {
        arm(&mut engine, source, b"x", 0, 100);
    }
    engine.cancel(1, 1).unwrap();
    engine.committed("fired", 2, b"x", 2).unwrap();
    engine.committed("rejected", 2, b"x", 2).unwrap();
    engine.committed("matched", 2, b"x", 2).unwrap();
    engine.resolve(2, 2, Ok(1)).unwrap();
    engine.resolve(3, 2, Err("down".into())).unwrap();
    engine.cut_off(2);
    assert_eq!(
        engine
            .records()
            .iter()
            .map(|r| r.status)
            .collect::<Vec<_>>(),
        [
            Status::Cancelled,
            Status::Fired,
            Status::Rejected,
            Status::CutOff,
            Status::CutOff
        ]
    );
    assert_eq!(engine.records()[3].cut_off_at, Some(2));
    assert_eq!(engine.records()[4].effect_at, Some(2));
    assert!(engine.due(2).is_empty());
    assert_eq!(engine.boundary(), None);
    assert!(engine.resolve(5, 2, Ok(2)).is_err());
}
