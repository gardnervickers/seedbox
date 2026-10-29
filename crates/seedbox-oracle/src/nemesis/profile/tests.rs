use super::*;
use crate::control::{DiskEffect, DiskOp, DiskScope};
use serde_json::{json, Value};

/// The example in docs/scenarios.md.
const DOCUMENTED: &str = r#"
start_ns: 2000000000
end_ns: 60000000000
interval_ns: 5000000000
targets: [db1, db2, db3]
faults:
  - {kind: partition, weight: 3, duration_ns: {min: 1000000000, max: 10000000000}}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: clock_skew, duration_ns: {min: 1000000000, max: 5000000000},
     max_offset_ns: 30000000000}
  - {kind: disk_latency, op: flush, targets: [db1], duration_ns: {min: 1000000000, max: 5000000000},
     latency_ns: {min: 1000000, max: 50000000}}
"#;

fn documented() -> Profile {
    serde_yaml_ng::from_str(DOCUMENTED).unwrap()
}

#[test]
fn documented_profile_parses_with_defaults_and_round_trips() {
    let profile = documented();
    profile.validate().unwrap();
    assert_eq!(profile.max_faults, DEFAULT_MAX_FAULTS);
    let weights: Vec<_> = profile.faults.iter().map(Fault::weight).collect();
    assert_eq!(weights, [3, 1, 1, 1]);
    let names: Vec<_> = profile.faults.iter().map(Fault::name).collect();
    assert_eq!(names, ["partition", "crash", "clock_skew", "disk_latency"]);
    // Entries use their actions' snake_case names. Defaults are omitted, so
    // a spelled-out default and an omitted one are the same profile.
    let wire = serde_json::to_value(&profile).unwrap();
    assert!(wire.get("max_faults").is_none());
    assert!(wire["faults"][1].get("weight").is_none());
    assert!(wire["faults"][0].get("from").is_none());
    assert_eq!(wire["faults"][2]["max_offset_ns"], 30_000_000_000u64);
    assert!(wire["faults"][3].get("jitter_ns").is_none());
    assert_eq!(wire["faults"][3]["latency_ns"]["max"], 50_000_000);
    let again: Profile = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(&again).unwrap(), wire);
}

#[test]
fn unknown_fields_and_kinds_are_refused() {
    let mut wire = serde_json::to_value(documented()).unwrap();
    wire["faults"][0]["surprise"] = true.into();
    assert!(serde_json::from_value::<Profile>(wire.clone()).is_err());
    wire["faults"][0] = json!({"kind": "meteor", "duration_ns": {"min": 1, "max": 2}});
    assert!(serde_json::from_value::<Profile>(wire.clone()).is_err());
    wire["faults"] = json!([]);
    wire["extra"] = 1.into();
    assert!(serde_json::from_value::<Profile>(wire).is_err());
}

fn with(fault: Value) -> Result<(), Error> {
    let mut profile = documented();
    profile.faults = vec![serde_json::from_value(fault).unwrap()];
    profile.validate()
}

#[test]
fn entries_are_refused_with_their_actions_own_errors() {
    let range = json!({"min": 1_000_000, "max": 2_000_000});
    let action = |error: Result<(), String>| error.unwrap_err();
    for (fault, error) in [
        (
            json!({"kind": "clock_skew", "max_offset_ns": 0, "duration_ns": range}),
            action(control::validate_clock_skew(None, Some(0))),
        ),
        (
            json!({"kind": "clock_drift", "max_rate_ppb": 200_000_000, "duration_ns": range}),
            action(control::validate_clock_drift(None, Some(200_000_000))),
        ),
        (
            json!({"kind": "throttle", "factor_permille": 10, "duration_ns": range}),
            action(control::validate_throttle(10)),
        ),
        (
            json!({"kind": "disk_error", "op": "flush", "ppm": 10, "effect": "corrupt",
                   "duration_ns": range}),
            action(control::validate_disk_error(
                DiskOp::Flush,
                10,
                DiskEffect::Corrupt,
                DiskScope::All,
            )),
        ),
        (
            json!({"kind": "disk_error", "op": "write", "ppm": 0, "duration_ns": range}),
            action(control::validate_disk_error(
                DiskOp::Write,
                0,
                DiskEffect::Eio,
                DiskScope::All,
            )),
        ),
        (
            json!({"kind": "disk_latency", "op": "any", "duration_ns": range,
                   "latency_ns": {"min": 1, "max": 2}}),
            action(control::validate_disk_latency(1, 0)),
        ),
        (
            json!({"kind": "preempt", "ppm": 2_000_000, "duration_ns": range}),
            action(control::validate_preempt(2_000_000)),
        ),
        (
            json!({"kind": "shape", "duration_ns": range,
                   "latency_ns": {"min": 1_000, "max": 2_000}, "jitter_ns": 1_000}),
            action(control::validate_link(1_000, 1_000, 0, None, None, 0)),
        ),
        (
            json!({"kind": "shape", "duration_ns": range,
                   "latency_ns": {"min": 1_000, "max": 4_000_000_000_000u64}}),
            action(control::validate_link(
                4_000_000_000_000,
                0,
                0,
                None,
                None,
                0,
            )),
        ),
        (
            json!({"kind": "shape", "duration_ns": range,
                   "latency_ns": {"min": 1_000, "max": 2_000}, "loss_ppm": 2_000_000}),
            action(control::validate_link(1_000, 0, 2_000_000, None, None, 0)),
        ),
    ] {
        let kind = fault["kind"].as_str().unwrap().to_owned();
        assert_eq!(
            with(fault).unwrap_err().0,
            format!("fault profile {kind}: {error}")
        );
    }
    assert_eq!(
        with(json!({"kind": "pause", "duration_ns": {"min": 5, "max": 1}}))
            .unwrap_err()
            .0,
        "fault profile pause: duration_ns needs 0 < min <= max"
    );
    assert_eq!(
        with(json!({"kind": "partition", "duration_ns": range, "from": ["db1"]}))
            .unwrap_err()
            .0,
        "fault profile partition: from needs a machine other than db1"
    );
    with(json!({"kind": "partition", "duration_ns": range, "from": ["db1", "client"]})).unwrap();
    assert_eq!(
        with(
            json!({"kind": "disk_error", "op": "write", "ppm": 10, "duration_ns": range,
                    "targets": ["db1", "client"]})
        )
        .unwrap_err()
        .0,
        "fault profile disk_error: targets must be profile targets, not \"client\""
    );
    let disk = json!({"kind": "disk_error", "op": "write", "ppm": 1_000_000,
                      "scope": "data", "duration_ns": range});
    with(disk.clone()).unwrap();
    let parsed: Fault = serde_json::from_value(disk.clone()).unwrap();
    assert!(matches!(
        parsed,
        Fault::DiskError {
            ppm: control::MAX_PPM,
            effect: DiskEffect::Eio,
            scope: DiskScope::Data,
            ..
        }
    ));
    // As in the actions, the disk op and the error rate are required.
    for field in ["op", "ppm"] {
        let mut partial = disk.clone();
        partial.as_object_mut().unwrap().remove(field);
        let error = serde_json::from_value::<Fault>(partial)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("missing field `{field}`")),
            "{error}"
        );
    }
}

#[test]
fn profile_bounds_are_explicit() {
    type Change = fn(&mut Profile);
    for (change, error) in [
        (
            (|p| p.end_ns = p.start_ns) as Change,
            "end_ns must be after",
        ),
        (|p| p.interval_ns = 0, "interval_ns must be positive"),
        (|p| p.max_faults = 0, "max_faults must be positive"),
        (|p| p.targets.clear(), "targets must name at least one"),
        (|p| p.targets.push(String::new()), "targets must name"),
        (|p| p.targets.push("db1".into()), "targets must be distinct"),
        (|p| p.faults.clear(), "positive weight"),
        (
            |p| {
                p.faults.truncate(1);
                if let Fault::Partition { weight, .. } = &mut p.faults[0] {
                    *weight = 0;
                }
            },
            "positive weight",
        ),
        (
            |p| p.targets.truncate(1),
            "partition: from needs a machine other than db1",
        ),
    ] {
        let mut profile = documented();
        change(&mut profile);
        let message = profile.validate().unwrap_err().0;
        assert!(message.contains(error), "{message}");
    }
}
