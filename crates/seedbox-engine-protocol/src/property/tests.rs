use super::*;
use serde_json::json;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse(record: Value) -> Result<Record, String> {
    Record::parse(&serde_json::to_vec(&record).unwrap())
}

#[test]
fn decodes_frames_and_ignores_other_management_lines() {
    let line = format!("SBX_PROP1 3 17 {}", hex(b"{}"));
    let frame = Frame {
        session: 3,
        seq: 17,
        payload: b"{}".to_vec(),
    };
    assert_eq!(decode(line.as_bytes()), Ok(Some(frame.clone())));
    assert_eq!(frame.encode(), line.as_bytes());
    assert_eq!(decode(b"SBX_LOG1 1 00"), Ok(None));
    assert_eq!(decode(b"SBX_OOM1 31 300948 1 707974686f6e"), Ok(None));
    for bad in [
        "SBX_PROP1 1 1",
        "SBX_PROP1 1 1 ",
        "SBX_PROP1 1 1 0",
        "SBX_PROP1 1 1 0G",
        "SBX_PROP1 1 1 7B7D",
        "SBX_PROP1 01 1 7b7d",
        "SBX_PROP1 +1 1 7b7d",
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
fn records_have_one_json_shape() {
    for (wire, record) in [
        (
            json!({"require": {"id": "failover exercised"}}),
            Record::Require {
                id: "failover exercised".into(),
            },
        ),
        (
            json!({"assert": {"id": "p", "kind": "never", "condition": false, "details": {"k": 1}}}),
            Record::Assert {
                id: "p".into(),
                kind: Kind::Never,
                condition: false,
                details: Some(json!({"k": 1})),
                details_truncated: false,
            },
        ),
        (
            json!({"assert": {"id": "p", "kind": "always", "condition": false, "details_truncated": true}}),
            Record::Assert {
                id: "p".into(),
                kind: Kind::Always,
                condition: false,
                details: None,
                details_truncated: true,
            },
        ),
        (json!({"flush": {}}), Record::Flush {}),
        (
            json!({"lost": {"reason": "socket buffer full", "count": 2}}),
            Record::Lost {
                reason: "socket buffer full".into(),
                count: 2,
            },
        ),
        (
            json!({"closed": {"session": 3}}),
            Record::Closed { session: 3 },
        ),
    ] {
        assert_eq!(parse(wire.clone()), Ok(record.clone()), "{wire}");
        assert_eq!(serde_json::to_value(&record).unwrap(), wire);
        let supervisor = wire.get("lost").is_some() || wire.get("closed").is_some();
        assert_eq!(record.allowed_from(1), wire.get("closed").is_none());
        assert_eq!(record.allowed_from(SUPERVISOR_SESSION), supervisor);
    }
}

#[test]
fn invalid_records_are_refused() {
    for bad in [
        json!("not a record"),
        json!({"assert": {"id": "x", "kind": "always"}}),
        json!({"assert": {"id": "x", "kind": "sometimes", "condition": true}}),
        json!({"assert": {"id": "x", "kind": "always", "condition": true, "message": "m"}}),
        json!({"require": {"id": ""}}),
        json!({"require": {"id": "a\nb"}}),
        json!({"require": {"id": "x".repeat(MAX_ID_BYTES + 1)}}),
        json!({"require": {"id": "x", "kind": "always"}}),
        // Dropped record kinds are not part of the protocol.
        json!({"declare": {"id": "x", "kind": "always"}}),
        json!({"event": {"name": "x"}}),
        json!({"setup_complete": {}}),
        json!({"summary": {}}),
        json!({"flush": {}, "require": {"id": "x"}}),
    ] {
        assert!(parse(bad.clone()).is_err(), "{bad}");
    }
    assert!(Record::parse(b"not json").is_err());
}

#[test]
fn replies_round_trip_on_their_session_line() {
    let flush = Reply::Flush { seq: 3 };
    let line = encode_reply(7, &flush);
    assert_eq!(line, br#"SBX_SDK1 7 {"flush":{"seq":3}}"#);
    assert_eq!(decode_reply(&line), Ok(Some((7, flush))));
    assert_eq!(decode_reply(b"SBX_PROP1 1 1 7b7d"), Ok(None));
    for bad in [
        "SBX_SDK1 1",
        "SBX_SDK1 1 {}",
        r#"SBX_SDK1 01 {"flush":{"seq":1}}"#,
        r#"SBX_SDK1 1 {"summary":{"seq":1}}"#,
    ] {
        assert!(decode_reply(bad.as_bytes()).is_err(), "{bad}");
    }
}

#[test]
fn kinds_say_which_conditions_violate() {
    assert!(Kind::Always.violated_by(false));
    assert!(!Kind::Always.violated_by(true));
    assert!(Kind::Never.violated_by(true));
    assert!(!Kind::Never.violated_by(false));
}

#[test]
fn assessments_keep_only_failures_and_serialize_canonically() {
    let origin = Origin {
        machine: "n1".into(),
        incarnation: 1,
        session: 2,
        seq: 3,
        time_ns: 4,
    };
    assert_eq!(Assessment::default().status(), AssessmentStatus::Passed);
    assert!(Assessment::default().is_empty());
    assert_eq!(
        serde_json::to_value(Assessment::default()).unwrap(),
        json!({})
    );
    let lost = Assessment {
        lost_count: 3,
        lost_evidence: vec![LostEvidence {
            reason: "property sequence 5 where 3 was next".into(),
            origin: Some(origin.clone()),
        }],
        ..Default::default()
    };
    assert_eq!(lost.status(), AssessmentStatus::Incomplete);
    let unmet = Assessment {
        unmet_requirements: vec!["failover exercised".into()],
        ..lost.clone()
    };
    assert_eq!(unmet.status(), AssessmentStatus::Failed);
    let assessment = Assessment {
        violations: vec![Violation {
            id: DEFAULT_NO_OOM.into(),
            kind: Kind::Never,
            example: Example {
                origin,
                details: None,
                details_truncated: true,
            },
        }],
        ..lost
    };
    // A definite violation stays a failure when evidence was also lost.
    assert_eq!(assessment.status(), AssessmentStatus::Failed);
    let origin = json!({"machine": "n1", "incarnation": 1, "session": 2, "seq": 3, "time_ns": 4});
    let wire = serde_json::to_value(&assessment).unwrap();
    assert_eq!(
        wire,
        json!({
            "violations": [{"id": "seedbox.no_out_of_memory_kills", "kind": "never",
                "example": {"origin": origin, "details_truncated": true}}],
            "lost_count": 3,
            "lost_evidence": [{"reason": "property sequence 5 where 3 was next", "origin": origin}],
        })
    );
    assert_eq!(
        serde_json::from_value::<Assessment>(wire).unwrap(),
        assessment
    );
    assert!(DEFAULT_NO_OOM.starts_with(RESERVED_PREFIX));
    assert!(DEFAULT_NO_CRASH.starts_with(RESERVED_PREFIX));
}
