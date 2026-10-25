use super::*;
use serde_json::json;

fn parse(value: serde_json::Value) -> Result<Action, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

#[test]
fn disk_and_scheduler_actions_round_trip_in_canonical_form() {
    for (wire, name) in [
        (
            json!({"action":"disk_error","handle":"eio","machine":"db","op":"write","ppm":1_000_000}),
            "disk_error",
        ),
        (
            json!({"action":"disk_error","handle":"rot","machine":"db","op":"read","ppm":1000,"effect":"corrupt","scope":"data"}),
            "disk_error",
        ),
        (
            json!({"action":"disk_latency","handle":"slow","machine":"db","op":"any","latency_ns":20_000_000}),
            "disk_latency",
        ),
        (
            json!({"action":"disk_latency","handle":"slow","machine":"db","op":"flush","latency_ns":1000,"jitter_ns":5000}),
            "disk_latency",
        ),
        (
            json!({"action":"preempt","handle":"race","machine":"app","ppm":10_000}),
            "preempt",
        ),
    ] {
        let action = parse(wire.clone()).unwrap();
        assert_eq!(action.name(), name);
        assert_eq!(action.machine(), wire["machine"].as_str());
        assert!(action.is_defined_only());
        assert!(!action.is_process() && !action.is_lifecycle());
        action.validate().unwrap();
        assert_eq!(serde_json::to_value(&action).unwrap(), wire);
    }
    // Defaults are omitted on the way out, so an input that spells them
    // matches one that leaves them out. Neither `op` nor `ppm` has one.
    let spelled = parse(json!({"action":"disk_error","handle":"h","machine":"m","op":"any","ppm":5,"effect":"eio","scope":"all"})).unwrap();
    assert_eq!(
        serde_json::to_value(&spelled).unwrap(),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"any","ppm":5})
    );
    let latency = parse(json!({"action":"disk_latency","handle":"h","machine":"m","op":"any","latency_ns":1000,"jitter_ns":0})).unwrap();
    assert_eq!(
        serde_json::to_value(&latency).unwrap(),
        json!({"action":"disk_latency","handle":"h","machine":"m","op":"any","latency_ns":1000})
    );
    assert!(!Action::Crash {
        machine: "m".into()
    }
    .is_defined_only());
}

#[test]
fn disk_and_scheduler_actions_refuse_unknown_fields_and_missing_arguments() {
    for bad in [
        json!({"action":"disk_error","handle":"h","machine":"m","ppm":1}),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"read"}),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"trim","ppm":1}),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"read","ppm":1,"effect":"error"}),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"read","ppm":1,"scope":"disk"}),
        json!({"action":"disk_error","handle":"h","machine":"m","op":"read","ppm":1,"extra":1}),
        json!({"action":"disk_latency","handle":"h","machine":"m","latency_ns":1000}),
        json!({"action":"disk_latency","handle":"h","machine":"m","op":"any"}),
        json!({"action":"preempt","handle":"h","machine":"m"}),
        json!({"action":"preempt","machine":"m","ppm":1}),
    ] {
        assert!(parse(bad.clone()).is_err(), "accepted {bad}");
    }
}

#[test]
fn disk_and_scheduler_arguments_stay_within_their_bounds() {
    use DiskEffect::*;
    use DiskOp::*;
    use DiskScope::*;
    validate_disk_error(Any, 1, Eio, All).unwrap();
    validate_disk_error(Write, MAX_PPM, Corrupt, Data).unwrap();
    validate_disk_error(Flush, 10, Eio, All).unwrap();
    for (op, ppm, effect, scope) in [
        (Read, 0, Eio, All),
        (Read, MAX_PPM + 1, Eio, All),
        (Flush, 10, Corrupt, All),
        (Flush, 10, Eio, Data),
    ] {
        assert!(validate_disk_error(op, ppm, effect, scope).is_err());
    }
    validate_disk_latency(*DISK_LATENCY_NS.start(), 0).unwrap();
    validate_disk_latency(*DISK_LATENCY_NS.end(), *DISK_LATENCY_NS.end()).unwrap();
    for (latency, jitter) in [
        (DISK_LATENCY_NS.start() - 1, 0),
        (DISK_LATENCY_NS.end() + 1, 0),
        (1000, DISK_LATENCY_NS.end() + 1),
    ] {
        assert!(validate_disk_latency(latency, jitter).is_err());
    }
    validate_preempt(1).unwrap();
    validate_preempt(MAX_PPM).unwrap();
    assert!(validate_preempt(0).is_err() && validate_preempt(MAX_PPM + 1).is_err());
    let invalid = parse(json!({"action":"preempt","handle":"h","machine":"m","ppm":0})).unwrap();
    assert!(invalid.validate().unwrap_err().contains("preempt ppm"));
    // The same check covers the bounds main's actions already had.
    for (bad, needle) in [
        (
            json!({"action":"clock_skew","handle":"h","machine":"m","offset_ns":0}),
            "clock_skew",
        ),
        (
            json!({"action":"clock_drift","handle":"h","machine":"m"}),
            "clock_drift",
        ),
        (
            json!({"action":"throttle","handle":"h","machine":"m","factor_permille":999}),
            "throttle",
        ),
    ] {
        let error = parse(bad).unwrap().validate().unwrap_err();
        assert!(error.contains(needle), "{error}");
    }
    parse(json!({"action":"throttle","handle":"h","machine":"m","factor_permille":2000}))
        .unwrap()
        .validate()
        .unwrap();
}

#[test]
fn fault_counts_are_strict_and_refusals_name_the_fault() {
    let counts = FaultCounts {
        matched: 3,
        affected: 1,
    };
    let wire = json!({"matched":3,"affected":1});
    assert_eq!(serde_json::to_value(counts).unwrap(), wire);
    assert!(
        serde_json::from_value::<FaultCounts>(json!({"matched":3,"affected":1,"x":0})).is_err()
    );
    assert_eq!(
        crate::not_implemented("disk_error"),
        "disk_error is not implemented yet"
    );
}

fn trigger(value: serde_json::Value) -> Result<TriggerSpec, String> {
    serde_json::from_value(value).map_err(|error| error.to_string())
}

#[test]
fn trigger_specs_round_trip_with_defaults_omitted() {
    let wire = json!({
        "when": {"kind":"lifecycle","machine":"db","transition":"crashed"},
        "then": {"action":"reboot","machine":"db"},
    });
    let spec = trigger(wire.clone()).unwrap();
    assert_eq!((spec.nth, spec.delay_ns, spec.within_ns), (1, 0, None));
    spec.validate().unwrap();
    assert_eq!(serde_json::to_value(&spec).unwrap(), wire);
    for wire in [
        json!({"when":{"kind":"output","machine":"db","marker":"commit"},"nth":3,"delay_ns":5,"within_ns":1000,"then":{"action":"stop","machine":"db"}}),
        json!({"when":{"kind":"connection_open","to":"db","port":5432},"then":{"action":"partition","handle":"cut","left":["db"],"right":["app"]}}),
        json!({"when":{"kind":"frame"},"nth":100,"then":{"action":"throttle","handle":"slow","machine":"db","factor_permille":2000}}),
        json!({"when":{"kind":"block_request","machine":"db","op":"flush"},"nth":40,"then":{"action":"crash","machine":"db"}}),
    ] {
        let spec = trigger(wire.clone()).unwrap();
        spec.validate().unwrap();
        assert_eq!(serde_json::to_value(&spec).unwrap(), wire);
    }
    let selector = EventSelector::ConnectionOpen {
        from: Some("app".into()),
        to: Some("db".into()),
        port: None,
    };
    assert_eq!(selector.name(), "connection_open");
    assert_eq!(selector.machines(), ["app", "db"]);
}

#[test]
fn trigger_specs_refuse_reserved_selectors_and_bad_shapes() {
    for name in RESERVED_SELECTORS {
        let error = trigger(
            json!({"when":{"kind":name,"name":"committed"},"then":{"action":"crash","machine":"db"}}),
        )
        .unwrap_err();
        assert!(error.contains("unknown variant"), "{name}: {error}");
    }
    assert!(trigger(
        json!({"when":{"kind":"frame","extra":1},"then":{"action":"crash","machine":"db"}})
    )
    .is_err());
    let crash = json!({"action":"crash","machine":"db"});
    for (when, extra, needle) in [
        (
            json!({"kind":"frame"}),
            json!({"nth":0}),
            "nth counts from 1",
        ),
        (json!({"kind":"frame"}), json!({"within_ns":0}), "within_ns"),
        (
            json!({"kind":"output","machine":"db","marker":""}),
            json!({}),
            "marker",
        ),
        (
            json!({"kind":"output","machine":"db","marker":"x".repeat(MAX_TRIGGER_MARKER + 1)}),
            json!({}),
            "marker",
        ),
        (
            json!({"kind":"frame","from":""}),
            json!({}),
            "machine is empty",
        ),
        (
            json!({"kind":"connection_open","port":0}),
            json!({}),
            "port",
        ),
        (
            json!({"kind":"block_request","machine":"db","op":"write"}),
            json!({"delay_ns":1}),
            "block_request",
        ),
        (
            json!({"kind":"block_request","machine":"other","op":"write"}),
            json!({}),
            "block_request",
        ),
    ] {
        let mut wire = json!({"when":when,"then":crash});
        wire.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = trigger(wire.clone()).unwrap().validate().unwrap_err();
        assert!(error.contains(needle), "{wire}: {error}");
    }
    // The effect's own arguments are checked too.
    let bad_effect = trigger(json!({"when":{"kind":"frame"},"then":{"action":"preempt","handle":"p","machine":"db","ppm":0}})).unwrap();
    assert!(bad_effect.validate().unwrap_err().contains("preempt ppm"));
    let late = trigger(json!({"when":{"kind":"frame"},"within_ns":10,"then":crash})).unwrap();
    late.validate_at(1).unwrap();
    assert!(late.validate_at(u64::MAX - 5).is_err());
}

#[test]
fn trigger_receipts_use_the_driver_lifecycle_names() {
    for (status, name, last) in [
        (TriggerStatus::Armed, "armed", false),
        (TriggerStatus::Matched, "matched", false),
        (TriggerStatus::Fired, "fired", true),
        (TriggerStatus::Rejected, "rejected", true),
        (TriggerStatus::Cancelled, "cancelled", true),
        (TriggerStatus::Expired, "expired", true),
        (TriggerStatus::CutOff, "cut_off", true),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), name);
        assert_eq!(status.is_final(), last);
    }
    let wire = json!({
        "id":1,"when":{"kind":"output","machine":"oracle","marker":"!"},"status":"rejected",
        "armed_at_ns":5,"match_deadline_ns":105,"seen":1,"matched_at_ns":7,"effect_at_ns":8,
        "reason":"machine is down"
    });
    let receipt: TriggerReceipt = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(receipt.input_key, None);
    assert_eq!(serde_json::to_value(&receipt).unwrap(), wire);
}

#[test]
fn output_markers_are_text_or_explicit_hex_bytes() {
    let marker = |value: serde_json::Value| serde_json::from_value::<Marker>(value);
    assert_eq!(marker(json!("ready")).unwrap(), Marker(b"ready".to_vec()));
    assert_eq!(
        marker(json!({"hex":"00ff"})).unwrap(),
        Marker(vec![0, 0xff])
    );
    // Text is always written as a string, however it was spelled.
    let spelled = marker(json!({"hex":"6f6b"})).unwrap();
    assert_eq!(serde_json::to_value(spelled).unwrap(), json!("ok"));
    assert_eq!(
        serde_json::to_value(Marker(vec![0xff])).unwrap(),
        json!({"hex":"ff"})
    );
    for bad in [
        json!([1, 2]),
        json!({"hex":"abc"}),
        json!({"hex":"zz"}),
        json!({"hex":"00","x":1}),
    ] {
        assert!(marker(bad.clone()).is_err(), "accepted {bad}");
    }
    // The tag is `kind`, not `on`, which YAML 1.1 reads as a boolean.
    let selector: EventSelector =
        serde_json::from_value(json!({"kind":"output","machine":"db","marker":"ready"})).unwrap();
    assert_eq!(selector.name(), "output");
    assert!(serde_json::from_value::<EventSelector>(json!({"on":"frame"})).is_err());
}
