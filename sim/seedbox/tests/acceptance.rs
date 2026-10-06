use seedbox::{
    acceptance::*,
    execution::*,
    orchestration::{Outcome, ReplayStatus},
    runner::Job,
    store::ContentDigest,
};
use std::collections::BTreeMap;
fn cases() -> Cases {
    serde_json::from_str(include_str!("../../acceptance/cases.json")).unwrap()
}
struct Fake<F>(F);
impl<F: FnMut(&mut Observation)> Launcher for Fake<F> {
    fn launch(&mut self, jobs: &[Job]) -> Vec<Observation> {
        jobs.iter()
            .map(|job| {
                let hash = ContentDigest::of(format!("{}:{}", job.scenario, job.seed).as_bytes());
                let planted = job.scenario == cases().checker_control.scenario;
                let mut obs = Observation {
                    job: job.clone(),
                    outcome: Some(if planted {
                        Outcome::Fail
                    } else {
                        Outcome::Pass
                    }),
                    detail: String::new(),
                    workload_exit: Some(u8::from(planted)),
                    witness: Some(hash.clone()),
                    state_digests: (job.witness == WitnessMode::FullState)
                        .then(|| BTreeMap::from([("n1".into(), hash.clone())])),
                    diagnostics: Some(ExecutionDiagnostics {
                        machine_witnesses: BTreeMap::from([("n1".into(), hash)]),
                        exits: BTreeMap::from([("n1".into(), 10)]),
                        simulated_time_ns: 100,
                        host_time_ns: 200,
                        legacy_faults: vec![LegacyFaultObservation {
                            member: "n1".into(),
                            stop_ns: 1,
                            start_ns: 2,
                            stopped: true,
                            restarted: true,
                        }],
                        checker_result: Some(
                            if planted {
                                "RESULT fail"
                            } else {
                                "RESULT pass"
                            }
                            .into(),
                        ),
                    }),
                    console: format!(
                        "RACE mode=racy lost={}\nRACE mode=atomic lost=0\n",
                        job.seed % 3
                    ),
                    evidence: Some(format!("/evidence/{}-{}", job.scenario, job.seed).into()),
                    attempts: vec![],
                    overshoot_retry: false,
                    replay: job.replay.as_ref().map(|_| ReplayStatus::Match),
                };
                (self.0)(&mut obs);
                obs
            })
            .collect()
    }
}
fn run(edit: impl FnMut(&mut Observation)) -> SuiteReport {
    SuiteReport::new(
        suite(
            &cases(),
            true,
            Some("/explicit/performance-only-engine".into()),
            &mut Fake(edit),
        )
        .unwrap(),
    )
}
fn verdict(report: &SuiteReport, name: &str) -> Verdict {
    report
        .checks
        .iter()
        .find(|c| c.name == name)
        .unwrap()
        .verdict
        .clone()
}
#[test]
fn suite_accepts_fresh_results_without_historical_hashes_or_failed_seed_pins() {
    assert_eq!(run(|_| {}).verdict, Verdict::Pass);
    // A changed engine may move every historical witness and make an old seed
    // pass or fail. Replay still checks only the current retained inputs.
    let report = run(|r| {
        if r.job.scenario == cases().replay.scenario {
            r.outcome = Some(Outcome::Fail);
            r.workload_exit = Some(1);
            r.witness = Some(ContentDigest::of(b"new execution semantics"));
        }
    });
    assert_eq!(verdict(&report, "replay exits"), Verdict::Pass);
    assert_eq!(verdict(&report, "replay full state"), Verdict::Pass);
}
#[test]
fn missing_evidence_is_unresolved_and_divergent_replay_fails() {
    for field in ["world", "machines", "state"] {
        let report = run(|r| {
            if r.job.replay.is_some() {
                match field {
                    "world" => r.witness = None,
                    "machines" => r.diagnostics = None,
                    _ => r.state_digests = None,
                }
            }
        });
        assert_eq!(
            verdict(&report, "replay full state"),
            Verdict::Unresolved,
            "{field}"
        );
    }
    for field in ["world", "machines", "state", "faults"] {
        let report = run(|r| {
            if r.job.replay.is_some() {
                let hash = ContentDigest::of(b"different");
                match field {
                    "world" => r.witness = Some(hash),
                    "machines" => {
                        r.diagnostics
                            .as_mut()
                            .unwrap()
                            .machine_witnesses
                            .insert("n1".into(), hash);
                    }
                    "state" => {
                        if let Some(s) = &mut r.state_digests {
                            s.insert("n1".into(), hash);
                        }
                    }
                    _ => r.diagnostics.as_mut().unwrap().legacy_faults[0].restarted = false,
                }
            }
        });
        assert_eq!(
            verdict(&report, "replay full state"),
            Verdict::Fail,
            "{field}"
        );
    }
}
#[test]
fn host_metrics_are_not_semantic_replay_assertions() {
    assert_eq!(
        run(|r| if r.job.replay.is_some() {
            let d = r.diagnostics.as_mut().unwrap();
            d.host_time_ns += 999;
            d.exits.insert("n1".into(), 999);
        })
        .verdict,
        Verdict::Pass
    );
}
#[test]
fn performance_change_must_preserve_current_results_and_exercise_requested_margin() {
    let report = run(|r| {
        if r.job.undo == 0 {
            r.witness = Some(ContentDigest::of(b"drift"));
        }
    });
    assert_eq!(verdict(&report, "stop-placement invariance"), Verdict::Fail);
    let report = run(|r| {
        if r.job.margin != "adaptive" {
            r.overshoot_retry = true;
        }
    });
    assert_eq!(
        verdict(&report, "stop-placement invariance"),
        Verdict::Unresolved
    );
}
#[test]
fn controls_detect_broken_checker_atomicity_and_unapplied_faults() {
    let report = run(|r| {
        if r.job.scenario == cases().checker_control.scenario {
            r.outcome = Some(Outcome::Pass);
            r.workload_exit = Some(0);
        }
    });
    assert_eq!(verdict(&report, "planted checker failure"), Verdict::Fail);
    let report = run(|r| r.console = r.console.replace("atomic lost=0", "atomic lost=1"));
    assert_eq!(
        verdict(&report, "race exploration and atomic control"),
        Verdict::Fail
    );
    let report = run(|r| r.diagnostics.as_mut().unwrap().legacy_faults[0].restarted = false);
    assert_eq!(
        verdict(&report, "fault effects and recovery"),
        Verdict::Fail
    );
    assert_eq!(
        verdict(&report, "fixed-version application control"),
        Verdict::Fail
    );
}
