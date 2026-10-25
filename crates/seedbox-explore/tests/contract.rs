//! The planning contract: plans depend on results, never on the order in
//! which members finish.
use seedbox_api::{CampaignPlan, MemberPlan, TestVerdict};
use seedbox_explore::{Feature, Feedback, Generation, Generations, Planner};

/// A seed range as a planner: every member is a root with the next seed.
struct Sweep {
    first_seed: u64,
}
impl Planner for Sweep {
    fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String> {
        Ok((0..generation.size)
            .map(|i| MemberPlan::Root {
                seed: self.first_seed + u64::from(generation.first_ordinal + i),
                profile_entries: None,
            })
            .collect())
    }
}

/// After the first generation, branch from the member with the most new
/// features (earliest on ties) at its last anchor, with reseeds from a
/// stream of the planner's own.
struct Guided {
    stream: u64,
}
impl Planner for Guided {
    fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String> {
        let Some(best) = generation
            .history
            .iter()
            .rev()
            .max_by_key(|m| m.feedback.features.len())
        else {
            return Sweep { first_seed: 200 }.plan(generation);
        };
        Ok((0..generation.size)
            .map(|_| {
                self.stream = self
                    .stream
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1);
                MemberPlan::Branch {
                    parent: best.ordinal,
                    anchor: best.feedback.anchors.last().unwrap().0.clone(),
                    reseed: self.stream,
                    choices: vec![],
                    mutation: None,
                }
            })
            .collect())
    }
}

fn plan(budget: u32, batch: u32) -> CampaignPlan {
    CampaignPlan {
        planner_seed: Some(7),
        budget,
        batch,
        fresh_permille: 0,
        corpus_limit: 4,
        anchors_ns: vec![2_000_000_000],
    }
}

/// A member's result is a function of its plan alone, as a simulation's is.
fn result(ordinal: u32, plan: &MemberPlan) -> Feedback {
    let salt = match plan {
        MemberPlan::Root { seed, .. } => *seed,
        MemberPlan::Branch { reseed, .. } => *reseed,
    };
    Feedback {
        ordinal,
        verdict: if salt % 5 == 0 {
            TestVerdict::Failed
        } else {
            TestVerdict::Passed
        },
        features: (0..salt % 4)
            .map(|i| Feature {
                key: format!("event:e{i}:0"),
                at_ns: Some(i),
            })
            .collect(),
        anchors: vec![("a0".into(), 2_000_000_000)],
        choices: vec![],
        moments: vec![],
    }
}

/// Run a campaign, reporting each generation's results in the order `order`
/// gives, and return every plan made.
fn run(
    planner: impl Planner,
    order: impl Fn(&mut Vec<(u32, MemberPlan)>),
) -> Vec<(u32, MemberPlan)> {
    let mut campaign = Generations::new(planner, &plan(10, 4)).unwrap();
    let mut all = Vec::new();
    while !campaign.done() {
        let mut generation = campaign.next_generation().unwrap();
        assert!(!generation.is_empty());
        // Nothing more is planned while members are outstanding.
        assert!(campaign.next_generation().unwrap().is_empty());
        all.extend(generation.iter().cloned());
        order(&mut generation);
        for (ordinal, plan) in generation {
            campaign.report(result(ordinal, &plan)).unwrap();
        }
    }
    assert!(campaign.next_generation().unwrap().is_empty());
    let ordinals: Vec<_> = campaign.history().iter().map(|m| m.ordinal).collect();
    assert_eq!(ordinals, (0..10).collect::<Vec<_>>());
    all
}

#[test]
fn plans_do_not_depend_on_completion_order() {
    let planners: [fn() -> Box<dyn Planner>; 2] = [
        || Box::new(Guided { stream: 7 }),
        || Box::new(Sweep { first_seed: 200 }),
    ];
    for planner in planners {
        let in_order = run(planner(), |_| {});
        let reversed = run(planner(), |g| g.reverse());
        let rotated = run(planner(), |g| g.rotate_left(1));
        assert_eq!(in_order, reversed);
        assert_eq!(in_order, rotated);
    }
    // Ten members in generations of four, four and two.
    let guided = run(Guided { stream: 7 }, |_| {});
    assert_eq!(guided.len(), 10);
    assert!(matches!(guided[4].1, MemberPlan::Branch { .. }));
}

#[test]
fn results_are_taken_once_from_planned_members() {
    let mut campaign = Generations::new(Sweep { first_seed: 1 }, &plan(2, 2)).unwrap();
    let generation = campaign.next_generation().unwrap();
    let first = result(generation[0].0, &generation[0].1);
    campaign.report(first.clone()).unwrap();
    assert!(campaign.report(first).is_err());
    assert!(campaign.report(result(9, &generation[1].1)).is_err());
    assert!(!campaign.done());
}

#[test]
fn plans_that_break_the_contract_are_refused() {
    struct Bad(fn(Generation<'_>) -> Vec<MemberPlan>);
    impl Planner for Bad {
        fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String> {
            Ok((self.0)(generation))
        }
    }
    fn branch(parent: u32, anchor: &str) -> MemberPlan {
        MemberPlan::Branch {
            parent,
            anchor: anchor.into(),
            reseed: 1,
            choices: vec![],
            mutation: None,
        }
    }
    for (planner, error) in [
        (Bad(|_| vec![]), "made 0 plans for a generation of 2"),
        (
            Bad(|_| vec![branch(0, "a0"); 2]),
            "parent 0 has not completed",
        ),
    ] {
        let mut campaign = Generations::new(planner, &plan(4, 2)).unwrap();
        assert!(campaign.next_generation().unwrap_err().contains(error));
    }
    let mut campaign = Generations::new(
        Bad(|g| {
            if g.index == 0 {
                (0..g.size)
                    .map(|i| MemberPlan::Root {
                        seed: u64::from(i),
                        profile_entries: None,
                    })
                    .collect()
            } else {
                vec![branch(1, "missing"); g.size as usize]
            }
        }),
        &plan(4, 2),
    )
    .unwrap();
    for (ordinal, plan) in campaign.next_generation().unwrap() {
        campaign.report(result(ordinal, &plan)).unwrap();
    }
    assert!(campaign
        .next_generation()
        .unwrap_err()
        .contains("member 1 has no capture at anchor missing"));
    assert!(Generations::new(Sweep { first_seed: 0 }, &plan(0, 1)).is_err());
}

#[test]
fn mutation_lineages_fit_the_shared_trigger_budget() {
    use seedbox_engine_protocol::input_program::Mutation;
    /// Every later member branches from the previous one with a mutation.
    struct Chain;
    impl Planner for Chain {
        fn plan(&mut self, generation: Generation<'_>) -> Result<Vec<MemberPlan>, String> {
            Ok(vec![match generation.history.last() {
                None => MemberPlan::Root {
                    seed: 1,
                    profile_entries: None,
                },
                Some(parent) => MemberPlan::Branch {
                    parent: parent.ordinal,
                    anchor: "a0".into(),
                    reseed: u64::from(parent.ordinal),
                    choices: vec![],
                    mutation: Some(Box::new(Mutation {
                        trigger: serde_json::from_value(serde_json::json!({
                            "when": {"kind": "event", "name": "committed"},
                            "then": {"action": "crash", "machine": "db"},
                        }))
                        .unwrap(),
                        duration_ns: 1_000,
                    })),
                },
            }])
        }
    }
    // The roots arm 60 triggers, leaving room for two mutations.
    let mut campaign = Generations::new(Chain, &plan(5, 1))
        .unwrap()
        .with_program_triggers(60);
    for _ in 0..3 {
        let generation = campaign.next_generation().unwrap();
        let (ordinal, plan) = &generation[0];
        campaign.report(result(*ordinal, plan)).unwrap();
    }
    let error = campaign.next_generation().unwrap_err();
    assert!(
        error.contains("a lineage of 3 mutations needs 6 trigger IDs; 4 are left"),
        "{error}"
    );
}
