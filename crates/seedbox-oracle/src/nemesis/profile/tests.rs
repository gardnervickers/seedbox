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
  - {kind: disk_latency, request: flush, targets: [db1], duration_ns: {min: 1000000000, max: 5000000000},
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
            json!({"kind": "disk_error", "request": "flush", "ppm": 10, "effect": "corrupt",
                   "duration_ns": range}),
            action(control::validate_disk_error(
                DiskOp::Flush,
                10,
                DiskEffect::Corrupt,
                DiskScope::All,
            )),
        ),
        (
            json!({"kind": "disk_error", "request": "write", "ppm": 0, "duration_ns": range}),
            action(control::validate_disk_error(
                DiskOp::Write,
                0,
                DiskEffect::Eio,
                DiskScope::All,
            )),
        ),
        (
            json!({"kind": "disk_latency", "request": "any", "duration_ns": range,
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
    // Narrowed to the targets that have a far side, a `from` may name a target.
    with(json!({"kind": "partition", "duration_ns": range, "from": ["db1"], "targets": ["db2", "db3"]}))
        .unwrap();
    assert_eq!(
        with(json!({"kind": "partition", "duration_ns": range, "targets": ["db9"]}))
            .unwrap_err()
            .0,
        "fault profile partition: targets must be profile targets, not \"db9\""
    );
    assert_eq!(
        with(json!({"kind": "partition", "duration_ns": range, "targets": ["db2", "db2"]}))
            .unwrap_err()
            .0,
        "fault profile partition: targets must be distinct"
    );
    // A shape's `to` narrows the same way.
    let shape = json!({"kind": "shape", "duration_ns": range, "to": ["db1"],
                       "latency_ns": {"min": 1_000_000, "max": 2_000_000}});
    assert_eq!(
        with(shape.clone()).unwrap_err().0,
        "fault profile shape: to needs a machine other than db1"
    );
    let mut narrowed = shape.clone();
    narrowed["targets"] = json!(["db2", "db3"]);
    with(narrowed.clone()).unwrap();
    narrowed["targets"] = json!(["db2", "client"]);
    assert_eq!(
        with(narrowed).unwrap_err().0,
        "fault profile shape: targets must be profile targets, not \"client\""
    );
    assert_eq!(
        with(
            json!({"kind": "disk_error", "request": "write", "ppm": 10, "duration_ns": range,
                    "targets": ["db1", "client"]})
        )
        .unwrap_err()
        .0,
        "fault profile disk_error: targets must be profile targets, not \"client\""
    );
    let disk = json!({"kind": "disk_error", "request": "write", "ppm": 1_000_000,
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
    // As in the actions, the request kind and the error rate are required.
    for field in ["request", "ppm"] {
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

/// One entry of each kind.
const KINDS: &str = "
- {kind: crash, downtime_ns: {min: 1000000, max: 2000000}}
- {kind: stop, downtime_ns: {min: 1000000, max: 2000000}}
- {kind: pause, duration_ns: {min: 1000000, max: 2000000}}
- {kind: pause_process, duration_ns: {min: 1000000, max: 2000000}}
- {kind: partition, duration_ns: {min: 1000000, max: 2000000}}
- {kind: shape, duration_ns: {min: 1000000, max: 2000000}, latency_ns: {min: 1000000, max: 2000000}}
- {kind: clock_skew, duration_ns: {min: 1000000, max: 2000000}, max_offset_ns: 1000}
- {kind: clock_drift, duration_ns: {min: 1000000, max: 2000000}, max_rate_ppb: 1000}
- {kind: disk_error, duration_ns: {min: 1000000, max: 2000000}, request: write, ppm: 1}
- {kind: disk_latency, duration_ns: {min: 1000000, max: 2000000}, request: any,
   latency_ns: {min: 1000000, max: 2000000}}
- {kind: throttle, duration_ns: {min: 1000000, max: 2000000}, factor_permille: 2000}
- {kind: preempt, duration_ns: {min: 1000000, max: 2000000}, ppm: 1}
";

fn fault(kind: &str) -> Fault {
    let kinds: Vec<Fault> = serde_yaml_ng::from_str(KINDS).unwrap();
    kinds
        .into_iter()
        .find(|fault| fault.name() == kind)
        .unwrap()
}

#[test]
fn every_kind_has_its_domains() {
    use Domain::*;
    let table: [(&str, &[Domain]); 12] = [
        ("crash", &[Lifecycle]),
        ("stop", &[Lifecycle]),
        ("pause", &[Lifecycle]),
        ("pause_process", &[Lifecycle]),
        ("partition", &[Network]),
        ("shape", &[Network]),
        ("clock_skew", &[Clock, Lifecycle]),
        ("clock_drift", &[Clock]),
        ("disk_error", &[DiskError]),
        ("disk_latency", &[DiskLatency]),
        ("throttle", &[Scheduler]),
        ("preempt", &[Scheduler]),
    ];
    for (kind, domains) in table {
        assert_eq!(fault(kind).domains(), domains, "{kind}");
    }
}

/// A world that records what it applies, and refuses the apply numbered
/// `refuse` (from 0).
#[derive(Default)]
struct Log {
    now_ns: u64,
    applied: Vec<Action>,
    refuse: Option<usize>,
}

impl World for Log {
    fn now_ns(&mut self) -> Result<u64, Error> {
        Ok(self.now_ns)
    }
    fn sleep(&mut self, duration: Duration) -> Result<(), Error> {
        self.now_ns += duration.as_nanos() as u64;
        Ok(())
    }
    fn apply(&mut self, action: &Action) -> Result<(), Error> {
        if self.refuse == Some(self.applied.len()) {
            return Err(Error(format!("{}: invalid_state: refused", action.name())));
        }
        self.applied.push(action.clone());
        Ok(())
    }
}

/// A profile on n1 alone, and the world it applies to.
struct Single {
    profile: Profile,
    world: Log,
}

impl Single {
    fn new() -> Self {
        let profile = serde_yaml_ng::from_str(
            "{start_ns: 1, end_ns: 2, interval_ns: 1, targets: [n1], faults: []}",
        );
        let world = Log::default();
        Self {
            profile: profile.unwrap(),
            world,
        }
    }

    /// Draw `kind` on n1, as a start that picked that entry does; false
    /// when n1 is not free for it.
    fn try_start(&mut self, kind: &str) -> Result<bool, Error> {
        self.profile.faults = vec![fault(kind)];
        self.profile.start(&mut self.world, &mut crate::Rng::new(1))
    }

    fn start(&mut self, kind: &str) -> bool {
        self.try_start(kind).unwrap()
    }

    /// End the fault holding `domain` on n1, as `run` does once it is due.
    fn end(&mut self, domain: Domain) {
        let ending = &self.profile.state.ending;
        let index = ending.iter().position(|e| e.domains.contains(&domain));
        self.profile.end(&mut self.world, index.unwrap()).unwrap();
    }
}

#[test]
fn a_target_holds_one_fault_of_each_domain() {
    let mut n1 = Single::new();
    assert!(n1.start("disk_latency") && n1.start("crash"));
    for held in ["disk_latency", "crash", "pause", "stop", "pause_process"] {
        assert!(!n1.start(held), "{held}");
    }
    assert!(n1.start("throttle") && !n1.start("preempt"));
    assert_eq!(n1.world.applied.len(), 3);
}

#[test]
fn ending_a_fault_frees_only_its_own_domains_in_either_order() {
    let mut n1 = Single::new();
    assert!(n1.start("crash") && n1.start("disk_latency"));
    // A latency ends during a crash's downtime: another latency may start
    // before the reboot, but no second crash.
    n1.end(Domain::DiskLatency);
    assert!(!n1.start("crash") && n1.start("disk_latency"));
    // A reboot comes during a latency: another crash may start, but no
    // second latency.
    n1.end(Domain::Lifecycle);
    assert!(!n1.start("disk_latency") && n1.start("crash"));
}

#[test]
fn a_clock_skew_holds_and_frees_both_its_clock_and_lifecycle_domains() {
    let mut n1 = Single::new();
    assert!(n1.start("clock_skew"));
    for held in ["clock_drift", "crash", "pause"] {
        assert!(!n1.start(held), "{held}");
    }
    assert!(n1.start("disk_latency"));
    // Both free once it ends, and no skew starts during a pause.
    n1.end(Domain::Clock);
    assert!(n1.start("pause") && !n1.start("clock_skew") && n1.start("clock_drift"));
}

#[test]
fn overlapping_domains_share_one_fault_budget() {
    // Every fault ends long before the window does, so only the budget
    // stops the starts.
    let yaml = "{start_ns: 1000000000, end_ns: 60000000000, interval_ns: 1000000000,
                 max_faults: 3, targets: [n1], faults: [
                   {kind: crash, downtime_ns: {min: 1500000000, max: 3000000000}},
                   {kind: disk_latency, request: any, duration_ns: {min: 1500000000, max: 3000000000},
                    latency_ns: {min: 1000000, max: 2000000}}]}";
    let mut overlapped = false;
    for seed in 0..16 {
        let mut profile: Profile = serde_yaml_ng::from_str(yaml).unwrap();
        let mut world = Log::default();
        profile.run(&mut world, &mut crate::Rng::new(seed)).unwrap();
        // Three starts and their three ends.
        assert_eq!(world.applied.len(), 6, "seed {seed}");
        overlapped |= !matches!(
            world.applied[1],
            Action::Reboot { .. } | Action::Heal { .. }
        );
    }
    assert!(overlapped, "some seed starts two faults on n1 at once");
}

#[test]
fn heal_ends_every_fault_on_a_target_after_a_refused_apply() {
    let mut n1 = Single::new();
    assert!(n1.start("crash") && n1.start("disk_latency"));
    n1.world.refuse = Some(2);
    let error = n1.try_start("clock_drift").unwrap_err();
    assert!(error.0.ends_with("invalid_state: refused"), "{error}");
    n1.world.refuse = None;
    n1.profile.heal(&mut n1.world).unwrap();
    let ended: BTreeSet<_> = n1.world.applied[2..].iter().map(Action::name).collect();
    assert_eq!(ended, BTreeSet::from(["heal", "reboot"]));
    // Both domains are free again.
    assert!(n1.start("crash") && n1.start("disk_latency"));
}

/// Narrowing picks which target the fault lands on; with no `from` or `to`,
/// the far side is still every other target of the profile.
#[test]
fn a_narrowed_entry_without_a_side_faces_all_other_profile_targets() {
    let profile = documented();
    let range = json!({"min": 1_000_000, "max": 2_000_000});
    for fault in [
        json!({"kind": "partition", "duration_ns": range, "targets": ["db1"]}),
        json!({"kind": "shape", "duration_ns": range, "latency_ns": range, "targets": ["db1"]}),
    ] {
        let fault: Fault = serde_json::from_value(fault).unwrap();
        assert_eq!(fault.targets(&profile.targets), ["db1"]);
        let drawn = fault.draw(
            "db1",
            &profile.targets,
            &mut crate::Rng::new(7),
            &mut InForce::default(),
        );
        let far = match drawn.action {
            Action::Partition { left, right, .. } => {
                assert_eq!(left, ["db1"]);
                right
            }
            Action::Shape { from, to, .. } => {
                assert_eq!(from, ["db1"]);
                to
            }
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(far, ["db2", "db3"]);
    }
}
