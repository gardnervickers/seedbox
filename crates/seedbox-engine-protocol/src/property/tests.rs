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
            json!({"declare": {"id": "p", "kind": "always_or_unreachable"}}),
            Record::Declare {
                id: "p".into(),
                kind: Kind::AlwaysOrUnreachable,
                message: None,
            },
        ),
        (
            json!({"assert": {"id": "p", "kind": "sometimes", "condition": false, "details": {"k": 1}}}),
            Record::Assert {
                id: "p".into(),
                kind: Kind::Sometimes,
                condition: false,
                message: None,
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
                message: None,
                details: None,
                details_truncated: true,
            },
        ),
        (
            json!({"event": {"name": "leader-elected"}}),
            Record::Event {
                name: "leader-elected".into(),
                details: None,
            },
        ),
        (
            json!({"setup_complete": {}}),
            Record::SetupComplete { details: None },
        ),
        (json!({"flush": {}}), Record::Flush {}),
        (json!({"summary": {}}), Record::Summary {}),
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
        json!({"assert": {"id": "x", "kind": "often", "condition": true}}),
        json!({"declare": {"id": "", "kind": "always"}}),
        json!({"declare": {"id": "a\nb", "kind": "always"}}),
        json!({"declare": {"id": "x".repeat(MAX_ID_BYTES + 1), "kind": "always"}}),
        json!({"declare": {"id": "x", "kind": "always", "message": "m".repeat(MAX_MESSAGE_BYTES + 1)}}),
        json!({"declare": {"id": "x", "kind": "always", "extra": 1}}),
        json!({"state": {"name": "role", "value": "leader"}}),
        json!({"choice": {"site": "pick", "alternatives": ["a"]}}),
        json!({"flush": {}, "summary": {}}),
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
    let summary = Reply::Summary {
        seq: 4,
        properties: [(
            "p".to_string(),
            Counts {
                passes: 1,
                failures: 0,
            },
        )]
        .into(),
        truncated: false,
    };
    assert_eq!(
        serde_json::to_value(&summary).unwrap(),
        json!({"summary": {"seq": 4, "properties": {"p": {"passes": 1, "failures": 0}}, "truncated": false}})
    );
    assert_eq!(
        decode_reply(&encode_reply(1, &summary)),
        Ok(Some((1, summary)))
    );
    assert_eq!(decode_reply(b"SBX_PROP1 1 1 7b7d"), Ok(None));
    for bad in [
        "SBX_SDK1 1",
        "SBX_SDK1 1 {}",
        r#"SBX_SDK1 01 {"flush":{"seq":1}}"#,
        r#"SBX_SDK1 1 {"choice":{"index":0}}"#,
    ] {
        assert!(decode_reply(bad.as_bytes()).is_err(), "{bad}");
    }
}

#[test]
fn summaries_fit_one_reply_line_whatever_their_ids() {
    let counts = Counts {
        passes: u64::MAX,
        failures: u64::MAX,
    };
    // IDs of the maximum length that double in JSON.
    let quoted = |i: usize| format!("{i:03}{}", "\"".repeat(MAX_ID_BYTES - 3));
    let reply = Reply::summary(u64::MAX, (0..40).map(|i| (quoted(i), counts)));
    let Reply::Summary {
        properties,
        truncated,
        ..
    } = &reply
    else {
        panic!("not a summary");
    };
    assert!(*truncated);
    assert!(!properties.is_empty() && properties.len() < MAX_SUMMARY_PROPERTIES);
    // The first IDs in order are the ones kept.
    assert!(properties.contains_key(&quoted(0)));
    let line = encode_reply(u32::MAX, &reply);
    assert!(line.len() <= MAX_REPLY_BYTES, "{}", line.len());
    assert_eq!(decode_reply(&line), Ok(Some((u32::MAX, reply))));
    // Plain IDs stop at the count limit; few enough are not truncated.
    let plain = Reply::summary(1, (0..40).map(|i| (format!("p{i:02}"), counts)));
    let Reply::Summary {
        properties,
        truncated,
        ..
    } = plain
    else {
        panic!("not a summary");
    };
    assert_eq!(
        (properties.len(), truncated),
        (MAX_SUMMARY_PROPERTIES, true)
    );
    assert_eq!(
        Reply::summary(1, [("p".to_string(), counts)]),
        Reply::Summary {
            seq: 1,
            properties: [("p".to_string(), counts)].into(),
            truncated: false
        }
    );
}

#[test]
fn kinds_follow_their_semantics() {
    use Kind::*;
    use PropertyStatus::*;
    for (kind, passes, failures, expected) in [
        (Always, 1, 0, Passed),
        (Always, 1, 1, Violated),
        (Always, 0, 0, Unmet),
        (AlwaysOrUnreachable, 0, 0, Passed),
        (AlwaysOrUnreachable, 3, 0, Passed),
        (AlwaysOrUnreachable, 3, 1, Violated),
        (Sometimes, 0, 0, Unmet),
        (Sometimes, 0, 4, Unmet),
        (Sometimes, 1, 4, Passed),
        (Reachable, 0, 0, Unmet),
        (Reachable, 1, 0, Passed),
        (Unreachable, 0, 0, Passed),
        (Unreachable, 1, 0, Violated),
    ] {
        assert_eq!(
            PropertyStatus::of(kind, passes, failures),
            expected,
            "{kind:?} {passes} {failures}"
        );
    }
}

#[test]
fn assessments_serialize_canonically() {
    let origin = Origin {
        machine: "n1".into(),
        incarnation: 1,
        session: 2,
        seq: 3,
        time_ns: 4,
    };
    let assessment = Assessment {
        status: AssessmentStatus::Violated,
        violated: 1,
        unmet: 0,
        evidence_complete: false,
        error_count: 1,
        errors: vec![EvidenceError {
            error: "property sequence 5 after 3".into(),
            origin: Some(origin.clone()),
        }],
        properties: vec![PropertyResult {
            id: DEFAULT_NO_OOM.into(),
            kind: Kind::AlwaysOrUnreachable,
            status: PropertyStatus::Violated,
            message: None,
            passes: 0,
            failures: 1,
            example: Some(Example {
                origin,
                details: None,
                details_truncated: true,
            }),
        }],
    };
    let wire = serde_json::to_value(&assessment).unwrap();
    assert_eq!(
        wire,
        json!({
            "status": "violated", "violated": 1, "unmet": 0, "evidence_complete": false,
            "error_count": 1,
            "errors": [{"error": "property sequence 5 after 3", "origin":
                {"machine": "n1", "incarnation": 1, "session": 2, "seq": 3, "time_ns": 4}}],
            "properties": [{"id": "seedbox.no_out_of_memory_kills", "kind": "always_or_unreachable",
                "status": "violated", "passes": 0, "failures": 1, "example": {"origin":
                {"machine": "n1", "incarnation": 1, "session": 2, "seq": 3, "time_ns": 4},
                "details_truncated": true}}]
        })
    );
    assert_eq!(
        serde_json::from_value::<Assessment>(wire).unwrap(),
        assessment
    );
    assert!(DEFAULT_NO_OOM.starts_with(RESERVED_PREFIX));
    assert!(DEFAULT_NO_CRASH.starts_with(RESERVED_PREFIX));
}
