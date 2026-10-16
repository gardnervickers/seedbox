use super::*;
use serde_json::json;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn frame(session: u32, seq: u64, record: Value) -> Frame {
    Frame {
        session,
        seq,
        payload: serde_json::to_vec(&record).unwrap(),
    }
}

/// Apply SDK records from one session in order.
fn ledger(records: &[Value]) -> Ledger {
    let mut ledger = Ledger::default();
    for (i, record) in records.iter().enumerate() {
        ledger.apply(
            &frame(1, i as u64 + 1, record.clone()),
            "n1",
            0,
            10 + i as u64,
        );
    }
    ledger
}

fn assert_record(id: &str, kind: &str, condition: bool) -> Value {
    json!({"assert": {"id": id, "kind": kind, "condition": condition}})
}

fn status(ledger: &Ledger, id: &str) -> PropertyStatus {
    ledger.properties[id].status()
}

#[test]
fn decodes_frames_and_ignores_other_management_lines() {
    let line = format!("SBX_PROP1 3 17 {}", hex(b"{}"));
    assert_eq!(
        decode(line.as_bytes()),
        Ok(Some(Frame {
            session: 3,
            seq: 17,
            payload: b"{}".to_vec()
        }))
    );
    assert_eq!(decode(b"SBX_LOG1 1 00"), Ok(None));
    for bad in [
        "SBX_PROP1 1 1",
        "SBX_PROP1 1 1 ",
        "SBX_PROP1 1 1 0",
        "SBX_PROP1 1 1 0G",
        "SBX_PROP1 1 1 7B7D",
        "SBX_PROP1 01 1 7b7d",
        "SBX_PROP1 1 x 7b7d",
        "SBX_PROP1 4294967296 1 7b7d",
    ] {
        assert!(decode(bad.as_bytes()).is_err(), "{bad}");
    }
    let maximum = format!("SBX_PROP1 1 1 {}", "ab".repeat(MAX_RECORD_BYTES));
    assert!(decode(maximum.as_bytes()).unwrap().is_some());
    assert!(decode(format!("{maximum}ab").as_bytes()).is_err());
}

#[test]
fn kinds_follow_their_semantics() {
    let l = ledger(&[
        assert_record("a-true", "always", true),
        assert_record("a-false", "always", true),
        assert_record("a-false", "always", false),
        assert_record("aou", "always_or_unreachable", true),
        assert_record("s-miss", "sometimes", false),
        assert_record("s-hit", "sometimes", false),
        assert_record("s-hit", "sometimes", true),
        assert_record("reach", "reachable", false),
        assert_record("unreach", "unreachable", false),
        json!({"declare": {"id": "a-never", "kind": "always"}}),
        json!({"declare": {"id": "aou-never", "kind": "always_or_unreachable"}}),
        json!({"declare": {"id": "reach-never", "kind": "reachable"}}),
        json!({"declare": {"id": "unreach-never", "kind": "unreachable"}}),
    ]);
    use PropertyStatus::*;
    for (id, expected) in [
        ("a-true", Passed),
        ("a-false", Violated),
        ("aou", Passed),
        ("s-miss", Unmet),
        ("s-hit", Passed),
        ("reach", Passed),
        ("unreach", Violated),
        ("a-never", Unmet),
        ("aou-never", Passed),
        ("reach-never", Unmet),
        ("unreach-never", Passed),
    ] {
        assert_eq!(status(&l, id), expected, "{id}");
    }
    let assessment = l.assessment();
    assert_eq!(assessment.status, AssessmentStatus::Violated);
    assert_eq!((assessment.violated, assessment.unmet), (2, 3));
    assert!(assessment.evidence_complete);
    // The deciding example is the first failure, for unreachable the first hit.
    let a_false = assessment
        .properties
        .iter()
        .find(|p| p.id == "a-false")
        .unwrap();
    assert_eq!(a_false.example.as_ref().unwrap().origin.seq, 3);
    let unreach = assessment
        .properties
        .iter()
        .find(|p| p.id == "unreach")
        .unwrap();
    assert_eq!(unreach.example.as_ref().unwrap().origin.seq, 9);
}

#[test]
fn unmet_goals_alone_pass_a_run() {
    let l = ledger(&[
        assert_record("s", "sometimes", false),
        json!({"declare": {"id": "a", "kind": "always"}}),
    ]);
    assert_eq!(l.assessment().status, AssessmentStatus::Passed);
    // The unmet goal still shows where it was evaluated false.
    let s = l
        .assessment()
        .properties
        .into_iter()
        .find(|p| p.id == "s")
        .unwrap();
    assert_eq!(s.example.unwrap().origin.seq, 1);
}

#[test]
fn evidence_errors_make_a_passing_run_incomplete_but_never_hide_a_violation() {
    let mut l = ledger(&[assert_record("a", "always", true)]);
    l.apply(
        &frame(1, 5, assert_record("a", "always", true)),
        "n1",
        0,
        20,
    );
    let assessment = l.assessment();
    assert_eq!(assessment.status, AssessmentStatus::Incomplete);
    assert!(!assessment.evidence_complete);
    assert_eq!(assessment.errors[0].error, "property sequence 5 after 1");
    // Sequencing resumes after the gap.
    l.apply(
        &frame(1, 6, assert_record("a", "always", false)),
        "n1",
        0,
        21,
    );
    assert_eq!(l.error_count, 1);
    assert_eq!(l.assessment().status, AssessmentStatus::Violated);
}

#[test]
fn malformed_and_invalid_records_are_evidence_errors() {
    let mut l = Ledger::default();
    let bad = [
        b"not json".to_vec(),
        br#"{"assert":{"id":"x","kind":"always"}}"#.to_vec(),
        br#"{"assert":{"id":"x","kind":"often","condition":true}}"#.to_vec(),
        br#"{"declare":{"id":"","kind":"always"}}"#.to_vec(),
        br#"{"declare":{"id":"x","kind":"always","extra":1}}"#.to_vec(),
        br#"{"declare":{"id":"seedbox.x","kind":"always"}}"#.to_vec(),
        br#"{"exit":{"code":0}}"#.to_vec(),
    ];
    for (i, payload) in bad.iter().enumerate() {
        let f = Frame {
            session: 1,
            seq: i as u64 + 1,
            payload: payload.clone(),
        };
        l.apply(&f, "n1", 0, 1);
    }
    l.malformed_frame("n1", 0, 2, "invalid property payload encoding");
    assert_eq!(l.error_count, bad.len() as u64 + 1);
    assert!(l.properties.is_empty());
    assert_eq!(l.assessment().status, AssessmentStatus::Incomplete);
}

#[test]
fn conflicting_kinds_are_rejected_and_the_first_declaration_kept() {
    let l = ledger(&[
        json!({"declare": {"id": "p", "kind": "always", "message": "first"}}),
        assert_record("p", "sometimes", false),
        assert_record("p", "always", true),
    ]);
    let p = &l.properties["p"];
    assert_eq!((p.kind, p.passes, p.failures), (Kind::Always, 1, 0));
    assert_eq!(p.message.as_deref(), Some("first"));
    assert!(p.declared);
    assert_eq!(l.error_count, 1);
}

#[test]
fn supervisor_exit_records_feed_the_default_property() {
    let mut l = Ledger::default();
    let exit = |code: Value| frame(SUPERVISOR_SESSION, 1, json!({"exit": code}));
    l.apply(&exit(json!({"code": 0})), "n1", 0, 5);
    assert_eq!(status(&l, DEFAULT_FATAL_SIGNAL), PropertyStatus::Passed);
    l.apply(&exit(json!({"signal": 11})), "n2", 0, 6);
    assert_eq!(status(&l, DEFAULT_FATAL_SIGNAL), PropertyStatus::Violated);
    let example = l.properties[DEFAULT_FATAL_SIGNAL]
        .first_failure
        .clone()
        .unwrap();
    assert_eq!(example.origin.machine, "n2");
    assert_eq!(example.details, Some(json!({"signal": 11})));
    assert_eq!(l.exits.len(), 2);
    // Exit is supervisor-only, and needs exactly one cause.
    l.apply(&exit(json!({"code": 1, "signal": 9})), "n3", 0, 7);
    assert_eq!(l.error_count, 1);
    // A new incarnation restarts sequencing.
    l.apply(&exit(json!({"code": 0})), "n1", 1, 8);
    assert_eq!(l.error_count, 1);
}

#[test]
fn lost_records_from_the_supervisor_are_evidence_errors() {
    let mut l = Ledger::default();
    let lost = json!({"lost": {"reason": "too many sessions", "count": 2}});
    l.apply(&frame(SUPERVISOR_SESSION, 1, lost), "n1", 0, 5);
    assert_eq!(
        l.errors[0].error,
        "2 property records lost: too many sessions"
    );
    let lost = json!({"lost": {"reason": "record too large", "count": 1}});
    l.apply(&frame(SUPERVISOR_SESSION, 2, lost), "n1", 0, 6);
    assert_eq!(l.errors[1].error, "property record lost: record too large");
}

#[test]
fn bounds_are_enforced_deterministically() {
    let big = "x".repeat(MAX_DETAILS_BYTES);
    let l = ledger(&[json!({"assert": {
        "id": "d", "kind": "always", "condition": false, "details": {"big": big}
    }})]);
    let example = l.properties["d"].first_failure.as_ref().unwrap();
    assert!(example.details.is_none() && example.details_truncated);

    let records: Vec<Value> = (0..=MAX_PROPERTIES)
        .map(|i| json!({"declare": {"id": format!("p{i}"), "kind": "always"}}))
        .collect();
    let l = ledger(&records);
    assert_eq!(l.properties.len(), MAX_PROPERTIES);
    assert_eq!(l.errors[0].error, "too many properties");

    let records: Vec<Value> = (0..MAX_ERRORS + 4).map(|_| json!({"bogus": {}})).collect();
    let l = ledger(&records);
    assert_eq!(l.error_count, MAX_ERRORS as u64 + 4);
    assert_eq!(l.errors.len(), MAX_ERRORS);
}

#[test]
fn events_and_setup_are_counted() {
    let mut l = ledger(&[
        json!({"event": {"name": "leader", "details": {"term": 2}}}),
        json!({"event": {"name": "leader"}}),
        json!({"setup_complete": {}}),
    ]);
    l.apply(&frame(1, 4, json!({"setup_complete": {}})), "n1", 0, 99);
    assert_eq!(l.events["leader"], 2);
    assert_eq!(l.setup["n1"].time_ns, 12);
}

#[test]
fn ledger_round_trips_through_checkpoint_serialization() {
    let mut l = ledger(&[assert_record("a", "always", true)]);
    let restored: Ledger = serde_json::from_value(serde_json::to_value(&l).unwrap()).unwrap();
    assert_eq!(restored, l);
    // Restored sequencing continues where it left off.
    let mut restored = restored;
    let next = frame(1, 2, assert_record("a", "always", true));
    l.apply(&next, "n1", 0, 50);
    restored.apply(&next, "n1", 0, 50);
    assert_eq!(restored, l);
    assert_eq!(l.error_count, 0);
}

#[test]
fn ledger_survives_flattened_report_deserialization() {
    // Engine reports flatten their evidence; serde's flatten buffer cannot
    // turn string map keys back into integers.
    #[derive(Serialize, Deserialize)]
    struct Report {
        version: u32,
        #[serde(flatten)]
        evidence: Evidence,
    }
    #[derive(Serialize, Deserialize)]
    struct Evidence {
        properties: Ledger,
    }
    let mut l = ledger(&[assert_record("a", "always", true)]);
    l.apply(
        &frame(SUPERVISOR_SESSION, 1, json!({"exit": {"code": 0}})),
        "n2",
        3,
        9,
    );
    let text = serde_json::to_string(&Report {
        version: 1,
        evidence: Evidence {
            properties: l.clone(),
        },
    })
    .unwrap();
    let report: Report = serde_json::from_str(&text).unwrap();
    assert_eq!(report.evidence.properties, l);
}

#[test]
fn kernel_oom_and_crash_lines_violate_their_default_properties() {
    let mut l = Ledger::default();
    let kernel = |seq, message: &str| {
        frame(
            SUPERVISOR_SESSION,
            seq,
            json!({"kernel": {"message": message}}),
        )
    };
    l.apply(
        &kernel(
            1,
            "Out of memory: Killed process 412 (java) total-vm:1024kB",
        ),
        "n1",
        0,
        5,
    );
    l.apply(
        &kernel(1, "worker[77]: segfault at 0 ip 0000 sp 0000 error 4"),
        "n2",
        0,
        6,
    );
    assert_eq!(status(&l, DEFAULT_NO_OOM), PropertyStatus::Violated);
    assert_eq!(status(&l, DEFAULT_NO_CRASH), PropertyStatus::Violated);
    let oom = l.properties[DEFAULT_NO_OOM].first_failure.clone().unwrap();
    assert_eq!(oom.origin.machine, "n1");
    assert!(oom.details.unwrap()["message"]
        .as_str()
        .unwrap()
        .contains("java"));
    // Applications cannot forge kernel evidence.
    l.apply(
        &frame(1, 1, json!({"kernel": {"message": "Killed process"}})),
        "n3",
        0,
        7,
    );
    assert_eq!(l.error_count, 1);
    assert_eq!(l.properties[DEFAULT_NO_OOM].failures, 1);
}

#[test]
fn coverage_maps_union_per_machine_and_only_from_the_supervisor() {
    let map = |set: &[usize]| {
        let mut bytes = vec![0u8; COVERAGE_BYTES];
        for &bit in set {
            bytes[bit / 8] |= 1 << (bit % 8);
        }
        hex(&bytes)
    };
    let mut ledger = Ledger::default();
    let coverage = |m: String| json!({"coverage": {"map": m}});
    ledger.apply(&frame(0, 1, coverage(map(&[3]))), "n1", 0, 10);
    ledger.apply(&frame(0, 2, coverage(map(&[9]))), "n1", 0, 20);
    assert_eq!(ledger.coverage["n1"], map(&[3, 9]));
    assert_eq!(ledger.error_count, 0);
    // An application cannot claim coverage, and a malformed map is an error.
    ledger.apply(&frame(1, 1, coverage(map(&[1]))), "n1", 0, 30);
    ledger.apply(&frame(0, 3, coverage("zz".into())), "n1", 0, 40);
    assert_eq!(ledger.error_count, 2);
    assert_eq!(ledger.coverage["n1"], map(&[3, 9]));
}

#[test]
fn states_keep_first_positions_of_values_and_transitions() {
    let state = |name: &str, value: &str| json!({"state": {"name": name, "value": value}});
    let ledger = ledger(&[
        state("role", "follower"),
        state("role", "follower"),
        state("role", "leader"),
        state("role", "follower"),
        json!({"event": {"name": "commit"}}),
        json!({"event": {"name": "commit"}}),
    ]);
    let times = |map: &BTreeMap<String, Origin>| -> Vec<(String, u64)> {
        map.iter().map(|(k, o)| (k.clone(), o.time_ns)).collect()
    };
    assert_eq!(
        times(&ledger.states),
        [
            ("n1/role=follower".into(), 10),
            ("n1/role=leader".into(), 12)
        ]
    );
    assert_eq!(
        times(&ledger.transitions),
        [
            ("n1/role:follower>leader".into(), 12),
            ("n1/role:leader>follower".into(), 13)
        ]
    );
    assert_eq!(ledger.state_values["n1/role"], "follower");
    assert_eq!(ledger.event_first["commit"].time_ns, 14);
    assert_eq!(ledger.error_count, 0);
    let bad = super::Record::parse(br#"{"state":{"name":"","value":"x"}}"#);
    assert!(bad.is_err());
}
