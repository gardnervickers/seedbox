use super::*;
use serde_json::json;

fn plan() -> CampaignPlan {
    CampaignPlan {
        planner_seed: None,
        budget: 32,
        batch: 8,
        fresh_permille: 250,
        corpus_limit: 8,
        anchors_ns: vec![2_000_000_000, 8_000_000_000],
    }
}

#[test]
fn request_profile_plan_and_baseline_round_trip() {
    // The spec is an ordinary simulation spec; see resource.rs for its tests.
    let wire = json!({
        "faultProfile": {
            "end_ns": 40_000_000_000u64,
            "interval_ns": 3_000_000_000u64,
            "faults": [{"kind": "pause_process", "weight": 2,
                        "duration_ns": {"min": 500_000_000, "max": 6_000_000_000u64}}],
        },
        "plan": {"budget": 24, "batch": 4, "freshPermille": 250, "corpusLimit": 2,
                 "anchorsNs": [8_000_000_000u64]},
        "groupBy": ["replica"],
        "baseline": "a7m2p9qx",
    });
    for (field, value) in wire.as_object().unwrap().clone() {
        match field.as_str() {
            "plan" => {
                let plan: CampaignPlan = serde_json::from_value(value.clone()).unwrap();
                plan.validate().unwrap();
                assert_eq!(plan.planner_seed, None);
                assert_eq!(serde_json::to_value(&plan).unwrap(), value);
            }
            "faultProfile" => {
                let profile: FaultProfile = serde_json::from_value(value.clone()).unwrap();
                profile.validate().unwrap();
                assert_eq!(serde_json::to_value(&profile).unwrap(), value);
            }
            "baseline" => {
                let id: SimulationId = serde_json::from_value(value).unwrap();
                assert_eq!(id.to_string(), "a7m2p9qx");
            }
            _ => {}
        }
    }
}

#[test]
fn plans_are_bounded_before_any_member_runs() {
    plan().validate().unwrap();
    for (change, error) in [
        (
            (|p: &mut CampaignPlan| p.budget = 0) as fn(&mut CampaignPlan),
            "budget must be 1 to 4096",
        ),
        (|p| p.budget = MAX_CAMPAIGN_MEMBERS + 1, "budget must be"),
        (|p| p.batch = 0, "batch must be 1 to 64"),
        (|p| p.batch = MAX_BATCH + 1, "batch must be"),
        (|p| p.fresh_permille = 1001, "fresh share"),
        (|p| p.corpus_limit = 0, "corpus holds"),
        (|p| p.anchors_ns = vec![5, 5], "strictly increasing"),
        (
            |p| p.anchors_ns = (1..=MAX_ANCHORS as u64 + 1).collect(),
            "at most 16",
        ),
    ] {
        let mut invalid = plan();
        change(&mut invalid);
        let message = invalid.validate().unwrap_err();
        assert!(message.contains(error), "{message}");
    }
}

#[test]
fn member_plans_are_complete_recipes() {
    for wire in [
        json!({"kind": "root", "seed": 201}),
        json!({"kind": "root", "seed": 202, "profileEntries": [0, 2]}),
        json!({"kind": "branch", "parent": 3, "anchor": "a1", "reseed": 99}),
        json!({
            "kind": "branch", "parent": 3, "anchor": "a1", "reseed": 99,
            "choices": [{"machine": "client", "incarnation": 0, "session": 1, "site": "retry",
                         "occurrence": 2, "select": 1}],
            "mutation": {
                "trigger": {
                    "when": {"kind": "event", "name": "acknowledged before sync", "machine": "db"},
                    "delay_ns": 10_000_000,
                    "then": {"action": "crash", "machine": "db"},
                },
                "duration_ns": 300_000_000,
            },
        }),
    ] {
        let plan: MemberPlan = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(&plan).unwrap(), wire);
        if let MemberPlan::Branch {
            mutation: Some(mutation),
            choices,
            ..
        } = &plan
        {
            mutation.trigger.validate().unwrap();
            choices[0].validate().unwrap();
        }
    }
    assert!(serde_json::from_value::<MemberPlan>(
        json!({"kind": "branch", "parent": 3, "anchor": "a1"})
    )
    .is_err());
}

#[test]
fn status_completes_when_every_started_member_has_a_verdict() {
    let member = |ordinal, verdict| CampaignMember {
        ordinal,
        generation: 0,
        plan: MemberPlan::Root {
            seed: 200 + u64::from(ordinal),
            profile_entries: None,
        },
        simulation: None,
        verdict,
        novelty: 0,
        in_corpus: false,
        error: None,
    };
    let mut status = CampaignStatus {
        source: "k7m2p9qx".parse().unwrap(),
        plan: CampaignPlan {
            budget: 2,
            ..plan()
        },
        generations: 1,
        features: 0,
        corpus: 0,
        members: vec![member(0, Some(TestVerdict::Passed))],
        assessment: CampaignAssessment {
            status: crate::PropertyAssessmentStatus::Passed,
            members_completed: 1,
            members_incomplete: 0,
            members_failed: 0,
            properties: vec![],
        },
        findings: vec![],
        not_observed: vec![],
        stop_requested: false,
        error: None,
    };
    assert!(!status.complete());
    status.members.push(member(1, Some(TestVerdict::Pending)));
    assert!(!status.complete());
    status.members[1].verdict = Some(TestVerdict::Failed);
    assert!(status.complete());
    status.findings.push(Finding {
        group: "oracle".into(),
        property: None,
        label: Some(FindingLabel::New),
        representative: "a7m2p9qx".parse().unwrap(),
        ordinal: 1,
        ancestry: vec![],
        members: vec![1],
        first_failure: None,
        evidence_incomplete: false,
    });
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["findings"][0]["label"], "new");
    assert!(wire.get("notObserved").is_none());
    assert_eq!(
        serde_json::from_value::<CampaignStatus>(wire).unwrap(),
        status
    );
}
