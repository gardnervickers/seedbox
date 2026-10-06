//! Acceptance assertions over normalized Rust run records. No historical witness
//! hashes or seeds required to keep failing: compare fresh executions only.
use crate::{
    execution::{ExecutionDiagnostics, WitnessMode},
    orchestration::{Outcome, ReplayStatus},
    runner::{Job, JobResult},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub scenario: String,
    pub seed: u64,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Race {
    pub scenario: String,
    pub seeds: Vec<u64>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixed {
    pub scenario: String,
    pub seeds: [u64; 2],
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Placement {
    pub scenario: String,
    pub seed: u64,
    pub variants: Vec<BTreeMap<String, serde_json::Value>>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cases {
    pub scenarios: BTreeMap<String, PathBuf>,
    pub replay: Case,
    pub stop_placement: Placement,
    pub race: Race,
    pub checker_control: Case,
    pub fault_effects: Case,
    pub fixed_control: Fixed,
}
impl Cases {
    pub fn load(path: &Path) -> Result<Self> {
        let cases: Self = serde_json::from_slice(&std::fs::read(path)?)?;
        if cases.race.seeds.len() < 2
            || cases.race.seeds.iter().collect::<BTreeSet<_>>().len() != cases.race.seeds.len()
            || cases.fixed_control.seeds[0] > cases.fixed_control.seeds[1]
            || cases.fixed_control.seeds[1] - cases.fixed_control.seeds[0] > 10000
        {
            return Err(Error::Execution("invalid acceptance seed sample".into()));
        }
        Ok(cases)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Verdict {
    Pass,
    Fail,
    Unresolved,
    Skipped,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Observation {
    pub job: Job,
    pub outcome: Option<Outcome>,
    pub detail: String,
    pub workload_exit: Option<u8>,
    pub witness: Option<ContentDigest>,
    pub state_digests: Option<BTreeMap<String, ContentDigest>>,
    pub diagnostics: Option<ExecutionDiagnostics>,
    pub console: String,
    pub evidence: Option<PathBuf>,
    pub attempts: Vec<PathBuf>,
    pub overshoot_retry: bool,
    pub replay: Option<ReplayStatus>,
}
impl Observation {
    pub fn completed(&self) -> bool {
        matches!(self.outcome, Some(Outcome::Pass | Outcome::Fail))
            && self.witness.is_some()
            && self
                .diagnostics
                .as_ref()
                .is_some_and(|d| !d.machine_witnesses.is_empty())
            && (self.job.witness != WitnessMode::FullState
                || self.state_digests.as_ref().is_some_and(|s| {
                    self.diagnostics
                        .as_ref()
                        .is_some_and(|d| s.keys().eq(d.machine_witnesses.keys()))
                }))
    }
    pub fn from_result(result: JobResult, store: &ContentStore) -> Self {
        let mut obs = Self {
            job: result.job,
            outcome: None,
            detail: result.error.unwrap_or_default(),
            workload_exit: None,
            witness: None,
            state_digests: None,
            diagnostics: None,
            console: String::new(),
            evidence: None,
            attempts: result.attempts,
            overshoot_retry: result.overshoot_retry,
            replay: None,
        };
        if let Some(run) = result.run {
            obs.evidence = Some(run.directory);
            obs.outcome = Some(run.record.outcome);
            if let Some(e) = run.record.error {
                obs.detail = e;
            }
            let check = &run.record.identity.configuration.check.service;
            if let Some(hash) = run.record.logs.get(&format!("machines/{check}.console")) {
                match store.read(hash) {
                    Ok(bytes) => obs.console = String::from_utf8_lossy(&bytes).into_owned(),
                    Err(e) => {
                        obs.outcome = Some(Outcome::Error);
                        obs.detail = e.to_string();
                    }
                }
            }
            if let Some(report) = run.record.report {
                obs.workload_exit = report.workload_exit.map(|e| e.code);
                obs.witness = report.witness;
                obs.state_digests = report.full_state;
                if obs.detail.is_empty() {
                    obs.detail = report.detail.unwrap_or_default();
                }
                obs.diagnostics = report.diagnostics;
            }
            obs.replay = run.record.replay.map(|r| r.status);
        }
        obs
    }
}
pub trait Launcher {
    fn launch(&mut self, jobs: &[Job]) -> Vec<Observation>;
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub verdict: Verdict,
    pub detail: String,
    pub runs: Vec<Observation>,
}
fn check(
    name: &str,
    runs: Vec<Observation>,
    assertion: impl FnOnce(&[Observation]) -> std::result::Result<String, String>,
) -> Check {
    let (verdict, detail) = if let Some(bad) = runs.iter().find(|r| !r.completed()) {
        (
            Verdict::Unresolved,
            format!("{} seed {}: {}", bad.job.scenario, bad.job.seed, bad.detail),
        )
    } else {
        match assertion(&runs) {
            Ok(s) => (Verdict::Pass, s),
            Err(s) => (Verdict::Fail, s),
        }
    };
    Check {
        name: name.into(),
        verdict,
        detail,
        runs,
    }
}
pub fn differences(a: &Observation, b: &Observation) -> Vec<String> {
    let mut diffs = vec![];
    if a.outcome != b.outcome || a.workload_exit != b.workload_exit {
        diffs.push("outcome".into());
    }
    if a.witness != b.witness {
        diffs.push("world witness".into());
    }
    if a.state_digests != b.state_digests {
        diffs.push("machine state digests".into());
    }
    match (&a.diagnostics, &b.diagnostics) {
        (Some(a), Some(b)) => {
            if a.machine_witnesses != b.machine_witnesses {
                diffs.push("machine witnesses".into());
            }
            if a.legacy_faults != b.legacy_faults {
                diffs.push("fault acknowledgements".into());
            }
        }
        _ => diffs.push("missing machine evidence".into()),
    }
    diffs
}
fn equal(a: &Observation, b: &Observation) -> std::result::Result<(), String> {
    let diffs = differences(a, b);
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!("fresh executions differ: {}", diffs.join(", ")))
    }
}
fn replay_job(original: &Observation) -> Option<Job> {
    let mut job = original.job.clone();
    job.replay = Some(original.evidence.clone()?);
    Some(job)
}
fn race_counts(run: &Observation) -> Option<(i64, i64)> {
    let mut found = BTreeMap::new();
    for line in run.console.lines().filter(|l| l.starts_with("RACE ")) {
        let fields: BTreeMap<_, _> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|s| s.split_once('='))
            .collect();
        let mode = fields.get("mode")?;
        let lost = fields.get("lost")?.parse::<i64>().ok()?;
        if found.insert(*mode, lost).is_some() {
            return None;
        }
    }
    Some((*found.get("racy")?, *found.get("atomic")?))
}
fn recovered(run: &Observation) -> bool {
    run.diagnostics
        .as_ref()
        .is_some_and(|d| d.legacy_faults.iter().any(|f| f.stopped && f.restarted))
}

/// Checks run over a launcher, so policy tests require neither KVM nor the CLI.
/// Fresh results may PASS or FAIL in replay tests; only planted controls require
/// failure. Fixed-version controls assert the application invariant, not a hash.
pub fn suite(
    cases: &Cases,
    full: bool,
    baseline: Option<PathBuf>,
    launcher: &mut dyn Launcher,
) -> Result<Vec<Check>> {
    let mut jobs = vec![Job::new(&cases.replay.scenario, cases.replay.seed)];
    let mut state = jobs[0].clone();
    state.witness = WitnessMode::FullState;
    jobs.push(state);
    let placement = jobs.len();
    jobs.push(Job::new(
        &cases.stop_placement.scenario,
        cases.stop_placement.seed,
    ));
    for variant in &cases.stop_placement.variants {
        let mut job = jobs[placement].clone();
        for (k, v) in variant {
            match k.as_str() {
                "undo" => {
                    job.undo = v
                        .as_u64()
                        .ok_or_else(|| Error::Execution("invalid undo case".into()))?
                }
                "margin" => {
                    job.margin = v
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                }
                _ => return Err(Error::Execution("unknown performance variant".into())),
            }
        }
        jobs.push(job);
    }
    let race_start = jobs.len();
    jobs.extend(
        cases
            .race
            .seeds
            .iter()
            .map(|s| Job::new(&cases.race.scenario, *s)),
    );
    let checker = jobs.len();
    jobs.push(Job::new(
        &cases.checker_control.scenario,
        cases.checker_control.seed,
    ));
    let faults = jobs.len();
    jobs.push(Job::new(
        &cases.fault_effects.scenario,
        cases.fault_effects.seed,
    ));
    let fixed = jobs.len();
    if full {
        jobs.extend(
            (cases.fixed_control.seeds[0]..=cases.fixed_control.seeds[1])
                .map(|s| Job::new(&cases.fixed_control.scenario, s)),
        );
    }
    let first = launcher.launch(&jobs);
    if first.len() != jobs.len() {
        return Err(Error::Execution("launcher lost acceptance results".into()));
    }
    let mut checks = vec![];
    for (index, original) in first.iter().take(2).enumerate() {
        let mut runs = vec![original.clone()];
        if let Some(job) = replay_job(original) {
            runs.extend(launcher.launch(&[job]));
        }
        checks.push(check(
            if index == 0 {
                "replay exits"
            } else {
                "replay full state"
            },
            runs,
            |r| {
                if r.len() != 2 {
                    return Err("missing replay attempt".into());
                }
                equal(&r[0], &r[1])?;
                if r[1].replay != Some(ReplayStatus::Match) {
                    return Err("orchestrator did not report replay MATCH".into());
                }
                Ok("fresh replay matches outcome and every declared witness".into())
            },
        ));
    }
    let mut placement_check = check(
        "stop-placement invariance",
        first[placement..race_start].to_vec(),
        |r| {
            for other in &r[1..] {
                equal(&r[0], other)?;
            }
            Ok("performance settings preserve outcomes and witnesses".into())
        },
    );
    if placement_check.verdict == Verdict::Pass
        && placement_check.runs[1..]
            .iter()
            .any(|r| r.overshoot_retry && r.job.margin != "adaptive")
    {
        placement_check.verdict = Verdict::Unresolved;
        placement_check.detail =
            "fixed margin only completed after retry; requested setting was not exercised".into();
    }
    checks.push(placement_check);
    let mut race_runs = first[race_start..checker].to_vec();
    let selected = race_runs
        .iter()
        .find(|r| race_counts(r).is_some_and(|(lost, _)| lost > 0))
        .cloned();
    if let Some(job) = selected.as_ref().and_then(replay_job) {
        race_runs.extend(launcher.launch(&[job]));
    }
    checks.push(check(
        "race exploration and atomic control",
        race_runs,
        |r| {
            let mut outcomes = BTreeSet::new();
            for run in &r[..cases.race.seeds.len()] {
                let (lost, atomic) =
                    race_counts(run).ok_or("missing/duplicate RACE observations")?;
                if atomic != 0 {
                    return Err("atomic control lost updates".into());
                }
                outcomes.insert(lost);
            }
            if outcomes.len() < 2 {
                return Err("seed sample did not explore distinct race outcomes".into());
            }
            let original = selected
                .as_ref()
                .ok_or("no observed lost-update race to replay")?;
            let again = r.get(cases.race.seeds.len()).ok_or("missing race replay")?;
            equal(original, again)?;
            if race_counts(original) != race_counts(again)
                || again.replay != Some(ReplayStatus::Match)
            {
                return Err("observed race did not replay".into());
            }
            Ok(format!(
                "{} distinct outcomes; atomic control clean; observed race replays",
                outcomes.len()
            ))
        },
    ));
    checks.push(check(
        "planted checker failure",
        vec![first[checker].clone()],
        |r| {
            if r[0].outcome == Some(Outcome::Fail)
                && r[0].workload_exit == Some(1)
                && r[0]
                    .diagnostics
                    .as_ref()
                    .and_then(|d| d.checker_result.as_ref())
                    .is_some()
            {
                Ok("planted assertion failure detected".into())
            } else {
                Err("checker did not detect planted failure".into())
            }
        },
    ));
    checks.push(check(
        "fault effects and recovery",
        vec![first[faults].clone()],
        |r| {
            if recovered(&r[0]) && r[0].outcome == Some(Outcome::Pass) {
                Ok("guest acknowledged stop/restart and workload recovered".into())
            } else {
                Err("missing acknowledged restart or application recovery".into())
            }
        },
    ));
    if full {
        checks.push(check(
            "fixed-version application control",
            first[fixed..].to_vec(),
            |r| {
                if r.iter()
                    .all(|r| r.outcome == Some(Outcome::Pass) && recovered(r))
                {
                    Ok(format!(
                        "{} seeds passed after acknowledged faults",
                        r.len()
                    ))
                } else {
                    Err("fixed-version failure or missing acknowledged fault".into())
                }
            },
        ));
    }
    if let Some(engine) = baseline {
        let mut runs = vec![first[0].clone()];
        if let Some(mut job) = replay_job(&first[0]) {
            job.engine = Some(engine);
            runs.extend(launcher.launch(&[job]));
        }
        checks.push(check(
            "declared performance-only engine change",
            runs,
            |r| {
                if r.len() != 2 {
                    return Err("missing cross-engine replay".into());
                }
                equal(&r[0], &r[1])?;
                if r[1].replay != Some(ReplayStatus::Match) {
                    return Err("cross-engine replay did not MATCH".into());
                }
                Ok("retained inputs match across engines".into())
            },
        ));
    }
    Ok(checks)
}
#[derive(Serialize, Deserialize)]
pub struct SuiteReport {
    pub checks: Vec<Check>,
    pub verdict: Verdict,
}
impl SuiteReport {
    pub fn new(checks: Vec<Check>) -> Self {
        let verdict = if checks.iter().any(|c| c.verdict == Verdict::Fail) {
            Verdict::Fail
        } else if checks.iter().any(|c| c.verdict == Verdict::Unresolved) {
            Verdict::Unresolved
        } else {
            Verdict::Pass
        };
        Self { checks, verdict }
    }
    pub fn exit_code(&self) -> u8 {
        match self.verdict {
            Verdict::Pass | Verdict::Skipped => 0,
            Verdict::Fail => 1,
            Verdict::Unresolved => 3,
        }
    }
}
