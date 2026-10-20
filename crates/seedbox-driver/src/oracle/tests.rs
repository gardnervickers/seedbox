use super::*;

fn request(arguments: Value) -> Request {
    let mut wire = json!({"version":1, "id":9});
    wire.as_object_mut()
        .unwrap()
        .extend(arguments.as_object().unwrap().clone());
    parse(&serde_json::to_vec(&wire).unwrap()).unwrap()
}
fn operation(arguments: Value) -> Operation {
    request(arguments).operation.unwrap()
}
fn invalid(arguments: Value) -> OperationError {
    let parsed = request(arguments);
    assert_eq!(parsed.id.get(), 9);
    parsed.operation.unwrap_err()
}
fn metadata() -> ReplyMetadata {
    ReplyMetadata {
        requested_at_ns: 10,
        admitted_at_ns: 30,
        effect_at_ns: Some(40),
        replied_at_ns: 60,
    }
}

#[test]
fn full_scoped_wire_vocabulary_preserves_targets_and_binary_marker() {
    for op in ["now", "inspect"] {
        operation(json!({"op":op})).validate_at(30).unwrap();
    }
    assert_eq!(
        operation(json!({"op":"sleep","duration_ns":0})),
        Operation::Sleep { duration_ns: 0 }
    );
    for op in [
        "stop",
        "restart",
        "pause_process",
        "resume_process",
        "pause",
        "resume",
        "crash",
        "reboot",
    ] {
        for target in [
            json!({"service":"database"}),
            json!({"machine":"replica-1"}),
        ] {
            operation(json!({"op":op,"target":target}))
                .validate_at(30)
                .unwrap();
        }
    }
    assert!(matches!(
        operation(json!({"op":"clock_skew","target":{"machine":"m"},"offset_ns":-1})),
        Operation::ClockSkew {
            offset_ns: Some(-1),
            max_offset_ns: None,
            ..
        }
    ));
    operation(json!({"op":"clock_drift","target":{"service":"s"},"max_rate_ppb":100_000_000}));
    operation(json!({"op":"throttle","target":{"machine":"m"},"factor_permille":1000}));
    operation(json!({"op":"heal","handle":"p2"}));
    assert!(
        matches!(operation(json!({"op":"arm_trigger","source":{"service":"s"},"marker":[0,255,10],"match_timeout_ns":7,"fault":{"action":"stop","target":{"machine":"m"}}})), Operation::ArmTrigger { marker, fault:TriggerFault::Stop { .. }, .. } if marker == [0,255,10])
    );
    // Zero is syntactically valid; unknown/cancelled trigger state is dispatcher-owned.
    assert_eq!(
        operation(json!({"op":"cancel_trigger","trigger_id":0})),
        Operation::CancelTrigger { trigger_id: 0 }
    );
}

#[test]
fn spawn_wire_contains_only_a_declared_name_not_guest_supplied_definitions() {
    assert_eq!(
        operation(json!({"op":"spawn","machine":"replica-1"})),
        Operation::Spawn {
            machine: "replica-1".into()
        }
    );
    for value in [
        json!({"op":"spawn"}),
        json!({"op":"spawn","machine":{"name":"replica-1"}}),
        json!({"op":"spawn","machine":"replica-1","resources":{}}),
        json!({"op":"spawn","machine":"replica-1","application":{}}),
        json!({"op":"spawn","target":{"machine":"replica-1"}}),
    ] {
        assert_eq!(invalid(value).code, ErrorCode::InvalidRequest);
    }
}

#[test]
fn identity_failures_are_fatal_but_argument_errors_retain_correlation() {
    for wire in [
        br#"{"version":1,"id":0,"op":"now"}"#.as_slice(),
        br#"{"version":2,"id":1,"op":"now"}"#,
        br#"{"version":1,"id":4294967296,"op":"now"}"#,
        br#"{"version":1,"id":1,"id":2,"op":"now"}"#,
        br#"{"version":1,"id":1,"version":1,"op":"now"}"#,
        br#"{"version":1,"op":"now"}"#,
        br#"{"version":1,"id":1,"op":"now""#,
    ] {
        assert_eq!(
            parse(wire).unwrap_err(),
            ProtocolError::Identity,
            "{wire:?}"
        );
    }
    for arguments in [
        json!({"op":"unknown"}),
        json!({"op":"now","extra":1}),
        json!({}),
        json!({"op":"stop","target":"m"}),
        json!({"op":"stop","target":{"machine":"m","service":"s"}}),
        json!({"op":"sleep","duration_ns":-1}),
        json!({"op":"sleep","duration_ns":1.5}),
        json!({"op":"arm_trigger","source":{"machine":"m"},"marker":[256],"match_timeout_ns":1,"fault":{"action":"stop","target":{"machine":"m"}}}),
    ] {
        let error = invalid(arguments);
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.starts_with("invalid request arguments:"));
    }
    assert_eq!(parse(b"{\n}").unwrap_err(), ProtocolError::Frame);
    assert_eq!(
        parse(&vec![b' '; MAX_FRAME]).unwrap_err(),
        ProtocolError::Frame
    );
    let wire = br#"{"version":1,"id":4294967295,"op":"now"}"#;
    assert_eq!(parse(wire).unwrap().id.get(), u32::MAX);
}

#[test]
fn semantic_bounds_and_time_overflow_are_checked_without_runtime_admission() {
    let target = json!({"machine":"m"});
    for n in [-31_536_000_000_000_000i64, 31_536_000_000_000_000] {
        operation(json!({"op":"clock_skew","target":target,"offset_ns":n}));
    }
    for arguments in [
        json!({"op":"clock_skew","target":target,"offset_ns":0}),
        json!({"op":"clock_skew","target":target,"offset_ns":i64::MIN}),
        json!({"op":"clock_skew","target":target,"offset_ns":1,"max_offset_ns":1}),
        json!({"op":"clock_skew","target":target,"max_offset_ns":MAX_CLOCK_OFFSET_NS+1}),
        json!({"op":"clock_drift","target":target,"rate_ppb":100_000_001}),
        json!({"op":"clock_drift","target":target}),
        json!({"op":"throttle","target":target,"factor_permille":999}),
        json!({"op":"throttle","target":target,"factor_permille":1_000_001}),
        json!({"op":"heal","handle":""}),
    ] {
        assert_eq!(invalid(arguments).code, ErrorCode::InvalidRequest);
    }
    let sleep = operation(json!({"op":"sleep","duration_ns":u64::MAX}));
    sleep.validate_at(0).unwrap();
    assert_eq!(sleep.validate_at(1).unwrap_err().message, "sleep overflow");
    let stop = operation(json!({"op":"stop","target":target}));
    stop.validate_at(u64::MAX - PROCESS_EFFECT_TIMEOUT_NS)
        .unwrap();
    assert_eq!(
        stop.validate_at(u64::MAX - PROCESS_EFFECT_TIMEOUT_NS + 1)
            .unwrap_err()
            .message,
        "fault deadline overflow"
    );
    let mut trigger = json!({"op":"arm_trigger","source":target,"marker":[1],"match_timeout_ns":1,"fault":{"action":"stop","target":target}});
    operation(trigger.clone())
        .validate_at(u64::MAX - PROCESS_EFFECT_TIMEOUT_NS - 1)
        .unwrap();
    assert_eq!(
        operation(trigger.clone())
            .validate_at(u64::MAX - PROCESS_EFFECT_TIMEOUT_NS)
            .unwrap_err()
            .message,
        "trigger deadline overflow"
    );
    for marker in [json!([]), json!(vec![0; 257])] {
        trigger["marker"] = marker;
        assert_eq!(
            invalid(trigger.clone()).message,
            "trigger requires 1..=256 marker bytes and positive match timeout"
        );
    }
    trigger["marker"] = json!(vec![255; 256]);
    operation(trigger.clone());
    trigger["match_timeout_ns"] = json!(0);
    assert_eq!(invalid(trigger).code, ErrorCode::InvalidRequest);
    let too_large = br#"{"version":1,"id":1,"op":"sleep","duration_ns":18446744073709551616}"#;
    assert_eq!(
        parse(too_large).unwrap().operation.unwrap_err().code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn network_operations_parse_and_validate_before_policy_admission() {
    assert_eq!(
        operation(json!({"op":"connections","from":{"machine":"a"},"to":{"service":"b"}})),
        Operation::Connections {
            from: Target::Machine("a".into()),
            to: Target::Service("b".into()),
            after: 0
        }
    );
    assert_eq!(
        operation(
            json!({"op":"drop_connection","connection":{"from":"a","to":"b","src_addr":"10.0.0.1","dst_addr":"10.0.0.2","src_port":1,"dst_port":65535,"incarnation":u64::MAX}})
        ),
        Operation::DropConnection {
            connection: Connection {
                from: "a".into(),
                to: "b".into(),
                src_addr: [10, 0, 0, 1].into(),
                dst_addr: [10, 0, 0, 2].into(),
                src_port: 1,
                dst_port: 65535,
                incarnation: u64::MAX
            }
        }
    );
    let malformed =
        invalid(json!({"op":"shape","from":[],"to":[],"latency_ns":0,"jitter_ns":0,"loss_ppm":0}));
    assert_eq!(malformed.code, ErrorCode::InvalidRequest);
    assert_eq!(malformed.message, "jitter_ns must be less than latency_ns");
    // Partitions parse; membership is checked at admission.
    assert_eq!(
        operation(json!({"op":"partition","left":[{"machine":"a"}],"right":[{"service":"b"}]})),
        Operation::Partition {
            left: vec![Target::Machine("a".into())],
            right: vec![Target::Service("b".into())],
        }
    );
    operation(json!({"op":"arm_trigger","source":{"machine":"a"},"marker":[1],"match_timeout_ns":1,"fault":{"action":"partition","left":[],"right":[]}}))
        .validate_at(0)
        .unwrap();
    // Malformed known deferred operations remain argument errors.
    assert_eq!(
        invalid(json!({"op":"shape"})).code,
        ErrorCode::InvalidRequest
    );
    // Disk has no operation in the legacy guest schema; do not invent one.
    assert_eq!(
        invalid(json!({"op":"disk"})).code,
        ErrorCode::InvalidRequest
    );
}

#[test]
fn replies_preserve_complete_metadata_and_error_shape() {
    let id = NonZeroU32::new(7).unwrap();
    let bytes = encode(id, metadata(), Ok(&json!({"now_ns":40}))).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({
            "version":1,"id":7,"ok":true,"result":{"now_ns":40},
            "requested_at_ns":10,"admitted_at_ns":30,"effect_at_ns":40,"replied_at_ns":60,
        })
    );
    let error = OperationError {
        code: ErrorCode::TargetUnavailable,
        message: "target VM has exited".into(),
        handle: Some("p3".into()),
    };
    let mut times = metadata();
    times.effect_at_ns = None;
    let bytes = encode::<Value>(id, times, Err(error)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).unwrap(),
        json!({
            "version":1,"id":7,"ok":false,"error":{"code":"target_unavailable","message":"target VM has exited","handle":"p3"},
            "requested_at_ns":10,"admitted_at_ns":30,"effect_at_ns":null,"replied_at_ns":60,
        })
    );
    for code in [
        ErrorCode::InvalidRequest,
        ErrorCode::Unsupported,
        ErrorCode::InvalidState,
        ErrorCode::ResultTooLarge,
    ] {
        let bytes = encode::<Value>(
            id,
            metadata(),
            Err(OperationError {
                code,
                message: "detail".into(),
                handle: Some("must-not-leak".into()),
            }),
        )
        .unwrap();
        let reply: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(reply.get("result").is_none());
        assert!(reply["error"].get("handle").is_none());
        assert!(reply.get("session").is_none());
        assert!(reply.get("relay").is_none());
    }
}

#[test]
fn oversized_success_becomes_bounded_correlated_error_and_large_error_fails() {
    let id = NonZeroU32::new(1).unwrap();
    let bytes = encode(
        id,
        metadata(),
        Ok(&json!({"machines":"x".repeat(MAX_FRAME)})),
    )
    .unwrap();
    let reply: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["error"]["code"], "result_too_large");
    assert!(reply.get("result").is_none());
    assert!(bytes.len() < MAX_FRAME);
    assert_eq!(
        encode::<Value>(
            id,
            metadata(),
            Err(OperationError::invalid("x".repeat(MAX_FRAME)))
        )
        .unwrap_err(),
        EncodeError::ErrorFrameTooLarge
    );
}
