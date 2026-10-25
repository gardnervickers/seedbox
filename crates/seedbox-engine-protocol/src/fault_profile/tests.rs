use super::*;

/// The example in docs/exploration.md.
const DOCUMENTED: &str = r#"
start_ns: 2000000000
end_ns: 60000000000
interval_ns: 5000000000
targets: [db1, db2, db3]
faults:
  - {kind: partition, weight: 3, duration_ns: {min: 1000000000, max: 10000000000}}
  - {kind: crash, downtime_ns: {min: 500000000, max: 3000000000}}
  - {kind: disk_latency, op: flush, duration_ns: {min: 1000000000, max: 5000000000},
     latency_ns: {min: 1000000, max: 50000000}}
"#;

#[test]
fn documented_profile_parses_with_defaults_and_round_trips() {
    let profile: FaultProfile = serde_yaml_ng::from_str(DOCUMENTED).unwrap();
    profile.validate().unwrap();
    assert_eq!(profile.max_faults, DEFAULT_MAX_FAULTS);
    let weights: Vec<_> = profile.faults.iter().map(FaultKind::weight).collect();
    assert_eq!(weights, [3, 1, 1]);
    let names: Vec<_> = profile.faults.iter().map(FaultKind::name).collect();
    assert_eq!(names, ["partition", "crash", "disk_latency"]);
    // Entries use their actions' snake_case names. Defaults are omitted, so
    // a spelled-out default and an omitted one are the same request.
    let wire = serde_json::to_value(&profile).unwrap();
    assert!(wire.get("max_faults").is_none());
    assert!(wire["faults"][1].get("weight").is_none());
    assert!(wire["faults"][2].get("jitter_ns").is_none());
    assert_eq!(wire["faults"][2]["latency_ns"]["max"], 50_000_000);
    assert_eq!(
        serde_json::from_value::<FaultProfile>(wire).unwrap(),
        profile
    );
}

#[test]
fn unknown_fields_and_kinds_are_refused() {
    let mut wire =
        serde_json::to_value(serde_yaml_ng::from_str::<FaultProfile>(DOCUMENTED).unwrap()).unwrap();
    wire["faults"][0]["surprise"] = true.into();
    assert!(serde_json::from_value::<FaultProfile>(wire.clone()).is_err());
    wire["faults"][0] = serde_json::json!({"kind": "meteor", "duration_ns": {"min": 1, "max": 2}});
    assert!(serde_json::from_value::<FaultProfile>(wire.clone()).is_err());
    wire["faults"] = serde_json::json!([]);
    wire["extra"] = 1.into();
    assert!(serde_json::from_value::<FaultProfile>(wire).is_err());
}

#[test]
fn entries_are_checked_by_their_actions_validators() {
    let base: FaultProfile = serde_yaml_ng::from_str(DOCUMENTED).unwrap();
    let with = |fault: serde_json::Value| {
        let mut profile = base.clone();
        profile.faults = vec![serde_json::from_value(fault).unwrap()];
        profile.validate()
    };
    let range = serde_json::json!({"min": 1_000_000, "max": 2_000_000});
    for (fault, error) in [
        (
            serde_json::json!({"kind": "disk_error", "op": "flush", "ppm": 10, "effect": "corrupt", "duration_ns": range}),
            "neither the corrupt effect nor the data scope",
        ),
        (
            serde_json::json!({"kind": "disk_error", "op": "write", "ppm": 0, "duration_ns": range}),
            "disk_error ppm must be",
        ),
        (
            serde_json::json!({"kind": "disk_latency", "op": "any", "duration_ns": range,
                               "latency_ns": {"min": 1, "max": 2}}),
            "disk_latency latency_ns must be",
        ),
        (
            serde_json::json!({"kind": "preempt", "ppm": 2_000_000, "duration_ns": range}),
            "preempt ppm must be",
        ),
        (
            serde_json::json!({"kind": "clock_skew", "max_offset_ns": 0, "duration_ns": range}),
            "clock_skew needs",
        ),
        (
            serde_json::json!({"kind": "throttle", "factor_permille": 10, "duration_ns": range}),
            "factor_permille must be",
        ),
        (
            serde_json::json!({"kind": "shape", "duration_ns": range,
                               "latency_ns": {"min": 1_000, "max": 2_000}, "jitter_ns": 1_000}),
            "jitter_ns must be less than latency_ns",
        ),
        (
            serde_json::json!({"kind": "shape", "duration_ns": range,
                               "latency_ns": {"min": 1_000, "max": 4_000_000_000_000u64}}),
            "exceeds one hour",
        ),
        (
            serde_json::json!({"kind": "shape", "duration_ns": range,
                               "latency_ns": {"min": 1_000, "max": 2_000}, "loss_ppm": 2_000_000}),
            "loss_ppm exceeds",
        ),
        (
            serde_json::json!({"kind": "pause", "duration_ns": {"min": 5, "max": 1}}),
            "fault profile pause: duration_ns needs 0 < min <= max",
        ),
    ] {
        let message = with(fault).unwrap_err();
        assert!(message.contains(error), "{message}");
    }
    let disk = serde_json::json!({"kind": "disk_error", "op": "write", "ppm": 1_000_000,
                                  "scope": "data", "duration_ns": range});
    with(disk.clone()).unwrap();
    let parsed: FaultKind = serde_json::from_value(disk.clone()).unwrap();
    assert!(matches!(
        parsed,
        FaultKind::DiskError {
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
        let error = serde_json::from_value::<FaultKind>(partial)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(&format!("missing field `{field}`")),
            "{error}"
        );
    }
    let latency = serde_json::json!({"kind": "disk_latency", "duration_ns": range,
                                     "latency_ns": {"min": 1_000, "max": 2_000}});
    assert!(serde_json::from_value::<FaultKind>(latency).is_err());
}

#[test]
fn profile_bounds_are_explicit() {
    let base: FaultProfile = serde_yaml_ng::from_str(DOCUMENTED).unwrap();
    for (change, error) in [
        (
            (|p: &mut FaultProfile| p.end_ns = p.start_ns) as fn(&mut FaultProfile),
            "end_ns must be after",
        ),
        (|p| p.interval_ns = 0, "interval_ns must be positive"),
        (|p| p.max_faults = 0, "max_faults must be 1 to 1024"),
        (
            |p| p.max_faults = MAX_GENERATED_FAULTS + 1,
            "max_faults must be 1 to 1024",
        ),
        (
            |p| p.targets.push(String::new()),
            "targets must be nonempty",
        ),
        (|p| p.faults.clear(), "positive weight"),
        (
            |p| {
                for fault in &mut p.faults {
                    if let FaultKind::Partition { weight, .. } = fault {
                        *weight = 0;
                    }
                }
                p.faults.truncate(1);
            },
            "positive weight",
        ),
    ] {
        let mut profile = base.clone();
        change(&mut profile);
        let message = profile.validate().unwrap_err();
        assert!(message.contains(error), "{message}");
    }
}
