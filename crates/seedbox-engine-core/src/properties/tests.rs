use super::*;
use seedbox_engine_protocol::property::{AssessmentStatus, MAX_LOST_EVIDENCE, MAX_PROPERTIES};

fn frame(session: u32, seq: u64, record: Value) -> Frame {
    Frame {
        session,
        seq,
        payload: serde_json::to_vec(&record).unwrap(),
    }
}

/// Reduce guest records of session 1 on machine `m`, numbered in order.
fn ledger(records: &[Value]) -> Ledger {
    let mut ledger = Ledger::default();
    for (i, record) in records.iter().enumerate() {
        let seq = i as u64 + 1;
        ledger.record("m", 1, seq * 10, &frame(1, seq, record.clone()));
    }
    ledger
}

fn assert_record(id: &str, kind: &str, condition: bool) -> Value {
    json!({"assert": {"id": id, "kind": kind, "condition": condition}})
}

fn require(id: &str) -> Value {
    json!({"require": {"id": id}})
}

fn violation<'a>(assessment: &'a Assessment, id: &str) -> &'a Violation {
    assessment
        .violations
        .iter()
        .find(|v| v.id == id)
        .unwrap_or_else(|| panic!("no violation of {id}"))
}

#[test]
fn passing_runs_report_no_assessment() {
    assert_eq!(Ledger::default().assessment(), None);
    let ledger = ledger(&[
        require("p"),
        assert_record("p", "always", true),
        assert_record("q", "never", false),
        assert_record("unrequired and true", "always", true),
    ]);
    assert_eq!(ledger.assessment(), None);
    // Nothing is tallied: only which properties were seen.
    assert_eq!(ledger.properties.len(), 3);
    assert!(ledger.violations.is_empty());
}

#[test]
fn violations_keep_their_first_example() {
    let ledger = ledger(&[
        assert_record("safety", "always", true),
        json!({"assert": {"id": "safety", "kind": "always", "condition": false, "details": {"key": 7}}}),
        assert_record("safety", "always", false),
        assert_record("two leaders", "never", false),
        json!({"assert": {"id": "two leaders", "kind": "never", "condition": true, "details": {"leaders": 2}}}),
    ]);
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    assert_eq!(assessment.violations.len(), 2);
    let safety = violation(&assessment, "safety");
    assert_eq!(safety.kind, Kind::Always);
    // The first violation decides it, at its commit time and sequence.
    assert_eq!(
        (safety.example.origin.seq, safety.example.origin.time_ns),
        (2, 20)
    );
    assert_eq!(safety.example.details, Some(json!({"key": 7})));
    let leaders = violation(&assessment, "two leaders");
    assert_eq!(leaders.kind, Kind::Never);
    assert_eq!(leaders.example.details, Some(json!({"leaders": 2})));
    // Sorted by ID.
    assert_eq!(assessment.violations[0].id, "safety");
}

#[test]
fn required_properties_never_evaluated_are_unmet() {
    let mut ledger = ledger(&[
        require("failover exercised"),
        require("checked"),
        // Evaluated before another machine requires it: still met.
        assert_record("leader elected", "always", true),
        assert_record("checked", "never", false),
    ]);
    ledger.record("n", 0, 50, &frame(1, 1, require("leader elected")));
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.unmet_requirements, ["failover exercised"]);
    assert!(assessment.violations.is_empty());
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    // A violating evaluation also meets the requirement.
    let ledger = self::ledger(&[require("p"), assert_record("p", "always", false)]);
    let assessment = ledger.assessment().unwrap();
    assert!(assessment.unmet_requirements.is_empty());
    assert_eq!(assessment.violations.len(), 1);
}

#[test]
fn lost_evidence_makes_a_run_incomplete_but_never_hides_a_violation() {
    let mut ledger = ledger(&[assert_record("p", "always", true)]);
    // A gap in the session's sequence: something never arrived.
    ledger.record("m", 1, 50, &frame(1, 5, assert_record("p", "always", true)));
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Incomplete);
    assert_eq!(
        assessment.lost_evidence[0].reason,
        "property sequence 5 where 2 was next"
    );
    assert_eq!(assessment.lost_evidence[0].origin.as_ref().unwrap().seq, 5);
    // The session continues from the record that arrived.
    ledger.record(
        "m",
        1,
        60,
        &frame(1, 6, assert_record("p", "always", false)),
    );
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    assert_eq!(assessment.lost_count, 1);
}

#[test]
fn malformed_reserved_and_misplaced_records_are_lost_evidence() {
    let mut ledger = Ledger::default();
    let records = [
        b"not json".to_vec(),
        br#"{"assert":{"id":"x","kind":"sometimes","condition":true}}"#.to_vec(),
        serde_json::to_vec(&assert_record(DEFAULT_NO_CRASH, "never", true)).unwrap(),
        serde_json::to_vec(&require("seedbox.mine")).unwrap(),
    ];
    for (i, payload) in records.into_iter().enumerate() {
        let frame = Frame {
            session: 1,
            seq: i as u64 + 1,
            payload,
        };
        assert_eq!(ledger.record("m", 1, 1, &frame), None);
    }
    // The supervisor's session reports losses only.
    ledger.record("m", 1, 2, &frame(0, 1, assert_record("p", "always", false)));
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Incomplete);
    assert_eq!(assessment.lost_count, 5);
    assert!(assessment.violations.is_empty());
    assert!(assessment.lost_evidence[0]
        .reason
        .starts_with("malformed record"));
    assert!(assessment.lost_evidence[2].reason.contains("is reserved"));
    assert_eq!(
        assessment.lost_evidence[4].reason,
        "record not allowed from the supervisor session"
    );
}

#[test]
fn lost_records_from_the_guest_or_supervisor_are_counted() {
    let mut ledger = ledger(&[json!({"lost": {"reason": "socket buffer full", "count": 3}})]);
    ledger.record(
        "m",
        1,
        20,
        &frame(
            0,
            1,
            json!({"lost": {"reason": "too many guest connections", "count": 1}}),
        ),
    );
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Incomplete);
    assert_eq!(assessment.lost_count, 4);
    let reasons: Vec<_> = assessment
        .lost_evidence
        .iter()
        .map(|e| e.reason.as_str())
        .collect();
    assert_eq!(
        reasons,
        [
            "3 property records lost: socket buffer full",
            "property record lost: too many guest connections"
        ]
    );
    assert_eq!(
        assessment.lost_evidence[1].origin.as_ref().unwrap().session,
        0
    );
}

#[test]
fn bounds_are_enforced_deterministically() {
    let records: Vec<_> = (0..=MAX_PROPERTIES)
        .map(|i| assert_record(&format!("p{i:04}"), "always", true))
        .collect();
    let ledger = ledger(&records);
    assert_eq!(ledger.properties.len(), MAX_PROPERTIES);
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.lost_count, 1);
    assert_eq!(assessment.lost_evidence[0].reason, "too many properties");
    // Only the first reasons are kept; later ones are counted.
    let mut flood = Ledger::default();
    for seq in 1..=40 {
        flood.record("m", 1, seq, &frame(1, seq, json!("bad")));
    }
    let assessment = flood.assessment().unwrap();
    assert_eq!(
        (assessment.lost_count, assessment.lost_evidence.len()),
        (40, MAX_LOST_EVIDENCE)
    );
}

#[test]
fn details_too_large_are_dropped_and_marked() {
    let big = "x".repeat(MAX_DETAILS_BYTES);
    let ledger = ledger(&[
        json!({"assert": {"id": "p", "kind": "always", "condition": false, "details": {"blob": big}}}),
        json!({"assert": {"id": "q", "kind": "never", "condition": true, "details_truncated": true}}),
    ]);
    let assessment = ledger.assessment().unwrap();
    for id in ["p", "q"] {
        let example = &violation(&assessment, id).example;
        assert_eq!((&example.details, example.details_truncated), (&None, true));
    }
}

#[test]
fn sessions_are_sequenced_per_machine_incarnation_and_session() {
    let mut ledger = Ledger::default();
    for (machine, incarnation, session) in [("a", 1, 1), ("a", 1, 2), ("b", 1, 1)] {
        ledger.record(
            machine,
            incarnation,
            1,
            &frame(session, 1, assert_record("p", "always", true)),
        );
    }
    // A reboot starts sessions afresh and retires the old boot's.
    ledger.record("a", 2, 2, &frame(1, 1, assert_record("p", "always", true)));
    assert_eq!(ledger.sequences.len(), 2);
    assert_eq!(ledger.sequences["a"].incarnation, 2);
    assert_eq!(ledger.sequences["a"].next.keys().collect::<Vec<_>>(), [&1]);
    assert_eq!(ledger.assessment(), None);
}

#[test]
fn closed_sessions_are_forgotten_and_open_ones_stay_bounded() {
    let mut ledger = Ledger::default();
    let assert = || assert_record("p", "always", true);
    ledger.record("m", 0, 1, &frame(1, 1, assert()));
    ledger.record("m", 0, 2, &frame(0, 1, json!({"closed": {"session": 1}})));
    assert!(!ledger.sequences["m"].next.contains_key(&1));
    // Only the supervisor may report a session closed.
    ledger.record("m", 0, 3, &frame(2, 1, json!({"closed": {"session": 2}})));
    assert_eq!(ledger.lost_count, 1);
    // Sessions never reported closed are forgotten oldest first, as lost.
    for session in 3..=MAX_SESSIONS as u32 + 3 {
        ledger.record("m", 0, 4, &frame(session, 1, assert()));
    }
    // Sessions 0 and 2 plus 65 more: two had to go.
    assert_eq!(ledger.sequences["m"].next.len(), MAX_SESSIONS + 1);
    assert_eq!(ledger.lost_count, 3);
    assert!(!ledger.sequences["m"].next.contains_key(&2));
    assert_eq!(ledger.validate(), Ok(()));
}

#[test]
fn flush_is_answered_with_its_own_sequence() {
    let mut ledger = ledger(&[
        assert_record("p", "always", true),
        assert_record("p", "always", false),
    ]);
    assert_eq!(
        ledger.record("m", 1, 30, &frame(1, 3, json!({"flush": {}}))),
        Some(Reply::Flush { seq: 3 })
    );
    // Requests from the supervisor's own session are refused, not answered.
    assert_eq!(
        ledger.record("m", 1, 1, &frame(0, 1, json!({"flush": {}}))),
        None
    );
}

fn report(message: &str) -> crash::Report {
    let hex: String = message.bytes().map(|b| format!("{b:02x}")).collect();
    crash::decode(format!("SBX_CRASH1 {hex}").as_bytes())
        .unwrap()
        .unwrap()
}

#[test]
fn crashes_and_oom_kills_violate_defaults_unless_the_machine_allows_them() {
    let mut ledger = Ledger::new(["s3".to_string()].into());
    let abort = report("libdynamorio.so: keel-watchdog: potentially unexpected fatal signal 6.");
    let kill = Kill {
        pid: 9,
        command: "hog".into(),
        anon_rss_kib: 1024,
        application: true,
    };
    ledger.crash("s3", 1, 10, &abort);
    ledger.out_of_memory("s3", 1, 11, &kill);
    assert_eq!(ledger.assessment(), None);
    // The same evidence on any other machine violates both.
    ledger.crash("oracle", 1, 12, &abort);
    ledger.out_of_memory("oracle", 1, 13, &kill);
    ledger.crash(
        "oracle",
        1,
        14,
        &report("x: potentially unexpected fatal signal 11."),
    );
    let assessment = ledger.assessment().unwrap();
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    let crash = violation(&assessment, DEFAULT_NO_CRASH);
    assert_eq!(crash.kind, Kind::Never);
    assert_eq!(crash.example.origin.machine, "oracle");
    assert_eq!(crash.example.origin.session, SUPERVISOR_SESSION);
    // The first crash is the example.
    let details = crash.example.details.as_ref().unwrap();
    assert_eq!(details["signal"], 6);
    assert_eq!(details["message"], abort.message);
    let oom = violation(&assessment, DEFAULT_NO_OOM);
    assert_eq!(
        oom.example.details.as_ref().unwrap()["kill"]["command"],
        "hog"
    );
}

#[test]
fn ledgers_round_trip_and_validate_their_bounds() {
    let mut ledger = Ledger::new(["s3".to_string()].into());
    ledger.record("m", 1, 1, &frame(1, 1, require("p")));
    ledger.record("m", 1, 2, &frame(1, 2, assert_record("q", "always", false)));
    ledger.crash(
        "n",
        1,
        2,
        &report("x: potentially unexpected fatal signal 11."),
    );
    let wire = serde_json::to_value(&ledger).unwrap();
    let restored: Ledger = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(restored, ledger);
    assert_eq!(restored.validate(), Ok(()));
    let mut crowded = wire.clone();
    let next: serde_json::Map<String, Value> = (0..=MAX_SESSIONS as u32 + 1)
        .map(|s| (s.to_string(), json!(1)))
        .collect();
    crowded["sequences"] = json!({"m": {"incarnation": 1, "next": next}});
    let crowded: Ledger = serde_json::from_value(crowded).unwrap();
    assert!(crowded.validate().is_err());
    // A violation of a property never evaluated cannot be restored.
    let mut orphan = wire.clone();
    orphan["violations"]["never seen"] = orphan["violations"]["q"].clone();
    let orphan: Ledger = serde_json::from_value(orphan).unwrap();
    assert!(orphan.validate().is_err());
    let mut unknown = wire;
    unknown["extra"] = json!(1);
    assert!(serde_json::from_value::<Ledger>(unknown).is_err());
}
