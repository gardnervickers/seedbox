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
