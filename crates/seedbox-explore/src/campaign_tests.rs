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
    }
}

/// A synthetic workload: what a member shows depends only on its plan.
fn feedback(plan: &Plan) -> Feedback {
    let (salt, anchors) = match &plan.kind {
        PlanKind::Root { seed } => (*seed, vec![("a0".into(), 1_000), ("a1".into(), 2_000)]),
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
                seed: 100 + i as u64
            }
        );
    }
    let branches: Vec<&Branch> = forward
        .iter()
        .filter_map(|p| match &p.kind {
            PlanKind::Branch(b) => Some(b),
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
}
