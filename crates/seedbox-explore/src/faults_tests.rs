use super::*;
use seedbox_control::Action;
use seedbox_spec::fault_profile::{Duration, DurationRange, FaultKind, FaultProfile};

fn range(min_ms: u64, max_ms: u64) -> DurationRange {
    DurationRange {
        min: Duration(min_ms * 1_000_000),
        max: Duration(max_ms * 1_000_000),
    }
}

fn profile(faults: Vec<FaultKind>) -> FaultProfile {
    FaultProfile {
        start: Duration(1_000_000_000),
        end: Duration(60_000_000_000),
        interval: Duration(2_000_000_000),
        max_faults: 64,
        targets: vec![],
        faults,
    }
}

fn machines() -> Vec<String> {
    ["n1", "n2", "n3", "oracle"].map(String::from).to_vec()
}

fn all_kinds() -> Vec<FaultKind> {
    vec![
        FaultKind::Partition {
            weight: 2,
            duration: range(500, 5000),
            from: vec![],
        },
        FaultKind::Shape {
            weight: 1,
            duration: range(500, 2000),
            latency: range(10, 100),
            loss_ppm: 1000,
        },
        FaultKind::Crash {
            weight: 1,
            downtime: range(200, 3000),
        },
        FaultKind::Pause {
            weight: 1,
            duration: range(200, 3000),
        },
        FaultKind::PauseProcess {
            weight: 1,
            duration: range(200, 3000),
        },
        FaultKind::Stop {
            weight: 1,
            downtime: range(200, 3000),
        },
        FaultKind::ClockSkew {
            weight: 1,
            duration: range(500, 2000),
            max_offset: Duration(1_000_000_000),
        },
    ]
}

fn machine(action: &Action) -> Option<&str> {
    match action {
        Action::Crash { machine }
        | Action::Reboot { machine }
        | Action::Pause { machine }
        | Action::Resume { machine }
        | Action::PauseProcess { machine }
        | Action::ResumeProcess { machine }
        | Action::Stop { machine }
        | Action::Restart { machine }
        | Action::ClockSkew { machine, .. } => Some(machine),
        _ => None,
    }
}

#[test]
fn schedules_are_deterministic_ordered_and_seed_dependent() {
    let p = profile(all_kinds());
    let a = generate(&p, 7, &machines(), "oracle", u64::MAX).unwrap();
    assert_eq!(a, generate(&p, 7, &machines(), "oracle", u64::MAX).unwrap());
    assert_ne!(a, generate(&p, 8, &machines(), "oracle", u64::MAX).unwrap());
    assert!(a.len() >= 20 && a.len().is_multiple_of(2), "{}", a.len());
    assert!(a.windows(2).all(|w| w[0].at_ns <= w[1].at_ns));
    for input in &a {
        assert!(input.key.starts_with(KEY_PREFIX));
        assert!(input.at_ns >= p.start.0);
        assert_ne!(machine(&input.action), Some("oracle"));
    }
    let starts = a.iter().filter(|i| !i.key.ends_with("-end")).count();
    assert_eq!(starts * 2, a.len());
    // Every start precedes its teardown.
    for start in a.iter().filter(|i| !i.key.ends_with("-end")) {
        let end = a
            .iter()
            .find(|i| i.key == format!("{}-end", start.key))
            .unwrap();
        assert!(end.at_ns > start.at_ns);
    }
}

#[test]
fn lifecycle_faults_never_overlap_on_one_machine() {
    let mut p = profile(all_kinds());
    p.interval = Duration(300_000_000);
    p.max_faults = 512;
    let inputs = generate(&p, 3, &machines(), "oracle", u64::MAX).unwrap();
    // Replay the schedule: a lifecycle start must find its machine idle.
    let mut down = std::collections::BTreeSet::new();
    let mut skewed = std::collections::BTreeMap::new();
    for input in &inputs {
        match &input.action {
            Action::Crash { machine }
            | Action::Pause { machine }
            | Action::PauseProcess { machine }
            | Action::Stop { machine } => {
                assert!(
                    down.insert(machine.clone()),
                    "{} overlaps on {machine}",
                    input.key
                );
            }
            Action::Reboot { machine }
            | Action::Resume { machine }
            | Action::ResumeProcess { machine }
            | Action::Restart { machine } => assert!(down.remove(machine)),
            Action::ClockSkew {
                handle,
                machine,
                offset_ns,
                ..
            } => {
                assert!(offset_ns.unwrap().unsigned_abs() <= 1_000_000_000);
                assert!(
                    !skewed.values().any(|m| m == machine),
                    "{} overlaps on {machine}",
                    input.key
                );
                skewed.insert(handle.clone(), machine.clone());
            }
            Action::Heal { handle } => {
                skewed.remove(handle);
            }
            _ => {}
        }
    }
    assert!(down.is_empty() && skewed.is_empty());
}

#[test]
fn partitions_isolate_one_target_from_everyone_and_respect_bounds() {
    let mut p = profile(vec![FaultKind::Partition {
        weight: 1,
        duration: range(100, 100),
        from: vec![],
    }]);
    p.targets = vec!["n2".into()];
    p.max_faults = 3;
    let inputs = generate(&p, 1, &machines(), "oracle", u64::MAX).unwrap();
    assert_eq!(inputs.len(), 6);
    let Action::Partition {
        left,
        right,
        handle,
    } = &inputs[0].action
    else {
        panic!("{:?}", inputs[0].action)
    };
    assert_eq!(
        (left.as_slice(), handle.as_str()),
        (["n2".to_string()].as_slice(), "gen-0")
    );
    assert_eq!(right, &["n1", "n3", "oracle"]);
    assert_eq!(inputs[1].at_ns - inputs[0].at_ns, 100_000_000);
    // Teardowns end at the simulated time limit.
    let limit = inputs[0].at_ns + 50_000_000;
    let clipped = generate(&p, 1, &machines(), "oracle", limit).unwrap();
    assert_eq!(clipped[1].at_ns, limit - 1);
    assert!(clipped.iter().all(|i| i.at_ns < limit));
}

#[test]
fn invalid_targets_are_rejected() {
    let mut p = profile(all_kinds());
    p.targets = vec!["oracle".into()];
    assert!(generate(&p, 1, &machines(), "oracle", u64::MAX).is_err());
    p.targets = vec!["n9".into()];
    assert!(generate(&p, 1, &machines(), "oracle", u64::MAX).is_err());
    p.targets = vec![];
    assert!(generate(&p, 1, &["oracle".to_string()], "oracle", u64::MAX).is_err());
}

#[test]
fn disk_errors_target_their_narrowed_machines_one_at_a_time() {
    use seedbox_spec::fault_profile::DiskOp as SpecDiskOp;
    let mut p = profile(vec![FaultKind::DiskError {
        weight: 1,
        duration: range(1000, 3000),
        op: SpecDiskOp::Flush,
        ppm: 500_000,
        effect: Default::default(),
        scope: Default::default(),
        targets: vec!["n2".into()],
    }]);
    p.interval = Duration(500_000_000);
    let inputs = generate(&p, 9, &machines(), "oracle", u64::MAX).unwrap();
    let mut active = false;
    for input in &inputs {
        match &input.action {
            Action::DiskError {
                machine, op, ppm, ..
            } => {
                assert_eq!(
                    (machine.as_str(), *op, *ppm),
                    ("n2", seedbox_control::DiskOp::Flush, 500_000)
                );
                assert!(!active, "{} overlaps", input.key);
                active = true;
            }
            Action::Heal { .. } => active = false,
            other => panic!("{other:?}"),
        }
    }
    assert!(inputs.len() >= 4);
    if let FaultKind::DiskError { targets, .. } = &mut p.faults[0] {
        *targets = vec!["oracle".into()];
    }
    assert!(generate(&p, 9, &machines(), "oracle", u64::MAX).is_err());
}

#[test]
fn disk_latency_and_preempt_generate_healed_windows() {
    let mut p = profile(vec![
        FaultKind::DiskLatency {
            weight: 1,
            duration: range(1000, 2000),
            latency: range(20, 40),
            op: seedbox_spec::fault_profile::DiskOp::Any,
            jitter: Duration(1_000_000),
            targets: vec!["n1".into()],
        },
        FaultKind::Preempt {
            weight: 1,
            duration: range(1000, 2000),
            ppm: 50_000,
        },
    ]);
    p.interval = Duration(500_000_000);
    let inputs = generate(&p, 3, &machines(), "oracle", u64::MAX).unwrap();
    let (mut latencies, mut preempts, mut heals) = (0, 0, 0);
    for input in &inputs {
        match &input.action {
            Action::DiskLatency {
                machine,
                latency_ns,
                jitter_ns,
                ..
            } => {
                assert_eq!(machine, "n1");
                assert!((20_000_000..=40_000_000).contains(latency_ns));
                assert_eq!(*jitter_ns, 1_000_000);
                latencies += 1;
            }
            Action::Preempt { machine, ppm, .. } => {
                assert_ne!(machine, "oracle");
                assert_eq!(*ppm, 50_000);
                preempts += 1;
            }
            Action::Heal { .. } => heals += 1,
            other => panic!("{other:?}"),
        }
    }
    assert!(latencies > 0 && preempts > 0);
    assert_eq!(heals, latencies + preempts);
}

#[test]
fn partitions_can_cut_a_target_off_from_named_machines_only() {
    let mut p = profile(vec![FaultKind::Partition {
        weight: 1,
        duration: range(100, 200),
        from: vec!["n1".into()],
    }]);
    p.targets = vec!["n2".into(), "n3".into()];
    let inputs = generate(&p, 4, &machines(), "oracle", u64::MAX).unwrap();
    assert!(!inputs.is_empty());
    for input in &inputs {
        if let Action::Partition { left, right, .. } = &input.action {
            assert!(left == &["n2"] || left == &["n3"], "{left:?}");
            assert_eq!(right, &["n1"]);
        }
    }
    if let FaultKind::Partition { from, .. } = &mut p.faults[0] {
        *from = vec!["n9".into()];
    }
    assert!(generate(&p, 4, &machines(), "oracle", u64::MAX).is_err());
}
