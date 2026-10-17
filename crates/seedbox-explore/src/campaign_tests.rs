use super::*;

fn config() -> Config {
    Config {
        seed: 7,
        first_seed: 100,
        budget: 24,
        batch: 4,
        fresh_permille: 250,
        corpus_limit: 3,
        anchors: vec![1_000, 2_000],
        mutations: Mutations {
            kinds: vec![MutationKind::Crash, MutationKind::Isolate],
            durations: Vec::new(),
            targets: vec!["db".into()],
            machines: vec!["db".into(), "oracle".into()],
        },
    }
}

/// A synthetic workload: what a member shows depends only on its plan.
fn feedback(plan: &Plan) -> Feedback {
    let (salt, anchors) = match &plan.kind {
        PlanKind::Root { seed, .. } => (*seed, vec![("a0".into(), 1_000), ("a1".into(), 2_000)]),
        PlanKind::Branch(b) => (
            b.reseed % 5,
            if b.anchor == "a0" {
                vec![("a1".into(), 2_000)]
            } else {
                Vec::new()
            },
        ),
    };
    Feedback {
        ordinal: plan.ordinal,
        outcome: if salt % 7 == 3 {
            Outcome::Failed
        } else {
            Outcome::Passed
        },
        features: vec![
            Feature {
                key: format!("event:e{}:0", salt % 6),
                at_ns: None,
            },
            Feature {
                key: format!("property:p{}:holds", salt % 4),
                at_ns: Some(1_500),
            },
        ],
        anchors,
        choices: vec![ObservedChoice {
            machine: "oracle".into(),
            incarnation: 0,
            session: 1,
            site: "order".into(),
            occurrence: 1,
            alternatives: 3,
            selected: (salt % 3) as usize,
            requested_at_ns: 1_800,
        }],
        moments: vec![(
            seedbox_control::EventSelector::Event {
                name: "commit".into(),
                machine: Some("db".into()),
            },
            1_900,
        )],
    }
}

/// Run a whole campaign, reporting each batch in the given order.
fn run(reverse: bool) -> (Vec<Plan>, Vec<Node>) {
    let mut planner = Planner::new(config()).unwrap();
    let mut all = Vec::new();
    while !planner.done() {
        let mut batch = planner.next_batch();
        assert!(!batch.is_empty());
        assert!(planner.next_batch().is_empty(), "one generation at a time");
        all.extend(batch.clone());
        if reverse {
            batch.reverse();
        }
        for plan in &batch {
            planner.report(feedback(plan)).unwrap();
        }
    }
    (all, planner.nodes().to_vec())
}

#[test]
fn plans_depend_on_results_not_completion_order() {
    let (forward, nodes) = run(false);
    assert_eq!(run(true), (forward.clone(), nodes.clone()));
    assert_eq!(forward.len(), 24);
    assert_eq!(
        forward.iter().map(|p| p.ordinal).collect::<Vec<_>>(),
        (0..24).collect::<Vec<_>>()
    );
    // Generation 0 is a seed sweep of consecutive roots.
    for (i, plan) in forward.iter().take(4).enumerate() {
        assert_eq!(
            plan.kind,
            PlanKind::Root {
                seed: 100 + i as u64,
                kinds: None
            }
        );
    }
    let branches: Vec<&Branch> = forward
        .iter()
        .filter_map(|p| match &p.kind {
            PlanKind::Branch(b) => Some(&**b),
            _ => None,
        })
        .collect();
    assert!(branches.len() > 8, "guidance branches most later members");
    assert!(forward.len() - branches.len() > 4, "a share stays fresh");
    for b in &branches {
        assert!(nodes[b.parent as usize].outcome.is_some());
        assert!(b.at_ns == 1_000 || b.at_ns == 2_000);
        for choice in &b.choices {
            assert!(1_800 > b.at_ns, "choices only change after the anchor");
            assert!(choice.select < 3);
        }
    }
    assert!(branches.iter().any(|b| !b.choices.is_empty()));
    // Mutations select what the parent did after the anchor.
    let mutated: Vec<&Mutation> = branches
        .iter()
        .filter_map(|b| b.mutation.as_ref())
        .collect();
    assert!(!mutated.is_empty());
    for m in &mutated {
        assert_eq!(m.target, "db");
        assert_eq!(m.others, ["oracle"]);
        assert!(DELAYS_NS.contains(&m.delay_ns) && DURATIONS_NS.contains(&m.duration_ns));
    }
    assert!(nodes.iter().filter(|n| n.in_corpus).count() <= 3);
}

#[test]
fn members_outside_the_corpus_are_released_once() {
    let mut planner = Planner::new(config()).unwrap();
    let mut released = Vec::new();
    while !planner.done() {
        for plan in planner.next_batch() {
            planner.report(feedback(&plan)).unwrap();
        }
        released.extend(planner.take_released());
    }
    let in_corpus: Vec<u32> = planner
        .nodes()
        .iter()
        .filter(|n| n.in_corpus)
        .map(|n| n.ordinal)
        .collect();
    let mut all: Vec<u32> = released.iter().chain(&in_corpus).copied().collect();
    all.sort();
    assert_eq!(all, (0..24).collect::<Vec<_>>());
}

#[test]
fn feedback_is_checked_and_configs_are_bounded() {
    let mut planner = Planner::new(config()).unwrap();
    let batch = planner.next_batch();
    assert!(planner.report(feedback(&batch[0])).is_ok());
    assert!(planner.report(feedback(&batch[0])).is_err());
    let mut stray = feedback(&batch[1]);
    stray.ordinal = 99;
    assert!(planner.report(stray).is_err());
    let mut bad = config();
    bad.anchors = vec![5, 5];
    assert!(Planner::new(bad).is_err());
    let mut bad = config();
    bad.batch = 0;
    assert!(Planner::new(bad).is_err());
    let mut all_fresh = config();
    all_fresh.fresh_permille = 1000;
    let mut planner = Planner::new(all_fresh).unwrap();
    while !planner.done() {
        for plan in planner.next_batch() {
            assert!(matches!(plan.kind, PlanKind::Root { .. }));
            planner.report(feedback(&plan)).unwrap();
        }
    }
}

#[test]
fn features_bucket_event_counts_and_name_outcomes() {
    let mut ledger = Ledger::default();
    ledger.events.insert("commit".into(), 5);
    let keys: Vec<String> = features(Some(&ledger), &[])
        .into_iter()
        .map(|f| f.key)
        .collect();
    assert_eq!(keys, ["event:commit:0", "event:commit:1", "event:commit:2"]);
}

#[test]
fn branch_programs_keep_the_prefix_and_resample_the_suffix() {
    use crate::faults::Generated;
    use seedbox_engine_protocol::input_program::{validate_program, TimedInput};
    let input = |key: &str, at_ns: u64, action: serde_json::Value| -> TimedInput {
        serde_json::from_value(serde_json::json!({"key": key, "at_ns": at_ns, "action": action}))
            .unwrap()
    };
    let crash = serde_json::json!({"action":"crash","machine":"db"});
    let reboot = serde_json::json!({"action":"reboot","machine":"db"});
    let parent = root_program(
        vec![
            input("gen-0", 500, crash.clone()),
            input("gen-0-end", 1_500, reboot.clone()),
            input("gen-1", 1_800, crash.clone()),
            input("gen-1-end", 1_900, reboot.clone()),
            input("mine", 2_000, crash.clone()),
        ],
        &[1_000, 2_000],
    )
    .unwrap();
    assert_eq!(
        parent.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
        [
            "gen-0",
            "a0",
            "gen-0-end",
            "gen-1",
            "gen-1-end",
            "a1",
            "mine"
        ]
    );
    let branch = Branch {
        parent: 0,
        anchor: "a0".into(),
        at_ns: 1_000,
        reseed: 9,
        identity: "c".into(),
        choices: vec![ChoiceOverride {
            machine: "oracle".into(),
            site: "order".into(),
            occurrence: 1,
            incarnation: None,
            session: None,
            select: 1,
        }],
        mutation: None,
    };
    let suffix = vec![
        Generated {
            key: "gen-0".into(),
            at_ns: 700,
            action: serde_json::from_value(crash.clone()).unwrap(),
        },
        Generated {
            key: "gen-0-end".into(),
            at_ns: 800,
            action: serde_json::from_value(reboot.clone()).unwrap(),
        },
        Generated {
            key: "gen-1".into(),
            at_ns: 1_200,
            action: serde_json::from_value(crash).unwrap(),
        },
        Generated {
            key: "gen-1-end".into(),
            at_ns: 1_300,
            action: serde_json::from_value(reboot).unwrap(),
        },
    ];
    let child = branch_program(&parent, &branch, 5, suffix).unwrap();
    assert_eq!(
        child.iter().map(|i| i.key.as_str()).collect::<Vec<_>>(),
        [
            "gen-0",
            "a0",
            "reseed-5",
            "choice-5-0",
            "gen-5x1",
            "gen-5x1-end",
            "gen-0-end",
            "a1",
            "mine"
        ]
    );
    validate_program(&child, 10_000).unwrap();
    // A mutation arms a fault and its teardown at the anchor.
    let mut mutated = branch.clone();
    mutated.mutation = Some(Mutation {
        when: seedbox_control::EventSelector::Lifecycle {
            machine: "db".into(),
            transition: seedbox_control::Transition::Rebooted,
        },
        nth: 1,
        delay_ns: 3_000_000,
        kind: MutationKind::Isolate,
        target: "db".into(),
        duration_ns: 100_000_000,
        others: vec!["oracle".into()],
        latency_ns: 0,
    });
    let child = branch_program(&parent, &mutated, 6, Vec::new()).unwrap();
    let keys: Vec<&str> = child.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        &keys[..6],
        [
            "gen-0",
            "a0",
            "reseed-6",
            "mutate-6",
            "mutate-6-end",
            "choice-6-0"
        ]
    );
    validate_program(&child, 10_000).unwrap();
}

#[test]
fn later_fresh_roots_sample_fault_kinds_and_generation_zero_stays_a_sweep() {
    let mut config = config();
    config.fresh_permille = 1000;
    config.budget = 40;
    let mut planner = Planner::new(config).unwrap();
    planner.set_profile_kinds(3);
    let mut sampled = 0;
    while !planner.done() {
        for plan in planner.next_batch() {
            let PlanKind::Root { kinds, .. } = &plan.kind else {
                panic!("all fresh");
            };
            if plan.generation == 0 {
                assert!(kinds.is_none());
            }
            if let Some(kinds) = kinds {
                assert!(!kinds.is_empty() && kinds.iter().all(|k| *k < 3));
                sampled += 1;
            }
            planner.report(feedback(&plan)).unwrap();
        }
    }
    assert!(sampled > 5 && sampled < 36, "{sampled}");
}

#[test]
fn slow_disk_and_preempt_mutations_respect_targets_and_heal() {
    let mut config = config();
    config.mutations = Mutations {
        kinds: vec![
            MutationKind::SlowDisk {
                latency_ns: (10_000_000, 20_000_000),
                targets: vec!["disk".into()],
            },
            MutationKind::Preempt { ppm: 5_000 },
        ],
        durations: vec![(2_000_000_000, 3_000_000_000), (500_000_000, 500_000_000)],
        targets: vec!["db".into(), "disk".into()],
        machines: vec!["db".into(), "disk".into(), "oracle".into()],
    };
    let mut planner = Planner::new(config).unwrap();
    let mut mutations = Vec::new();
    while !planner.done() {
        let batch = planner.next_batch();
        for plan in &batch {
            if let PlanKind::Branch(b) = &plan.kind {
                mutations.extend(b.mutation.clone().map(|m| (plan.ordinal, b.clone(), m)));
            }
            planner.report(feedback(plan)).unwrap();
        }
    }
    let (mut slow, mut preempt) = (0, 0);
    for (ordinal, branch, m) in &mutations {
        let parent = root_program(Vec::new(), &[1_000, 2_000]).unwrap();
        let child = branch_program(&parent, branch, *ordinal, Vec::new()).unwrap();
        let action = |key: String| {
            let input = child.iter().find(|i| i.key == key).unwrap();
            serde_json::to_value(&input.action).unwrap()
        };
        let start = action(format!("mutate-{ordinal}"));
        let end = action(format!("mutate-{ordinal}-end"));
        assert_eq!(end["then"]["action"], "heal");
        assert_eq!(end["then"]["handle"], format!("mutate-{ordinal}"));
        match &m.kind {
            MutationKind::SlowDisk { .. } => {
                slow += 1;
                assert!((2_000_000_000..=3_000_000_000).contains(&m.duration_ns));
                assert_eq!(m.target, "disk");
                assert!((10_000_000..=20_000_000).contains(&m.latency_ns));
                assert_eq!(start["then"]["action"], "disk_latency");
                assert_eq!(start["then"]["latency_ns"], m.latency_ns);
            }
            MutationKind::Preempt { ppm } => {
                preempt += 1;
                assert_eq!(m.duration_ns, 500_000_000);
                assert_eq!(start["then"]["action"], "preempt");
                assert_eq!(start["then"]["ppm"], *ppm);
            }
            other => panic!("{other:?}"),
        }
    }
    assert!(slow > 0 && preempt > 0, "{slow} {preempt}");
}
