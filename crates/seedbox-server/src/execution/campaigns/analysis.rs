//! Counterfactual analysis jobs: the arms from
//! `seedbox_explore::counterfactual`, each submitted as ordinary runs of the
//! source's full program from a fresh start, then counted.
use super::*;
use seedbox_api::{
    Analysis, AnalysisArm, AnalysisArmKind, AnalysisRequest, MAX_ANALYSIS_RUNS,
    MAX_ANALYSIS_SAMPLES, MAX_ANALYSIS_TIMES,
};
use seedbox_engine_protocol::input_program::InputAction;
use seedbox_explore::counterfactual::{self as cf, Arm};

pub(super) struct AnalysisJob {
    source: SimulationId,
    request: AnalysisRequest,
    target: String,
    identity: FailureIdentity,
    spec: seedbox_api::SimulationSpec,
    program: Vec<TimedInput>,
    seed: u64,
    end_ns: u64,
    arms: Vec<(Arm, AnalysisArmKind)>,
    /// Each arm's runs once submitted, by variant (0 is a fault arm's exact
    /// run), with its outcome once decided.
    runs: Vec<Vec<(u32, SimulationId, Option<Outcome>)>>,
    submitted: bool,
    pub(super) error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Same,
    Other,
    Passed,
    Incomplete,
}

/// "pause on n2 at 12.5s", from a fault's action.
fn summary(input: &TimedInput) -> String {
    let InputAction::Fault(action) = &input.action else {
        return input.key.clone();
    };
    let value = serde_json::to_value(action).unwrap_or_default();
    let on = ["machine", "target"]
        .iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
        .map(|m| format!(" on {m}"))
        .unwrap_or_default();
    format!("{}{on} at {:.3}s", action.name(), input.at_ns as f64 / 1e9)
}

impl ExecutionService {
    /// Analyse how a failed simulation's failure depends on its later draws
    /// and, if asked, on each of its external faults.
    pub fn submit_analysis(
        &self,
        source: &SimulationId,
        request: AnalysisRequest,
    ) -> Result<Analysis, Error> {
        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?;
            if let Some(id) = jobs.requests.get(&request.request_id) {
                let (view, original) = jobs
                    .analysis_views
                    .get(id)
                    .ok_or_else(|| invalid("analysis request reused"))?;
                if original != &request || &view.source != source {
                    return Err(Error::Catalogue(catalogue::Error::RequestConflict));
                }
                return Ok(view.clone());
            }
            jobs.admit()?;
        }
        if request.samples == 0 || request.samples > MAX_ANALYSIS_SAMPLES {
            return Err(invalid(format!(
                "samples must be 1 to {MAX_ANALYSIS_SAMPLES}"
            )));
        }
        if request.times > MAX_ANALYSIS_TIMES {
            return Err(invalid(format!("times must be 0 to {MAX_ANALYSIS_TIMES}")));
        }
        let resource = self.portable_simulation(source)?;
        if resource.status.phase != Phase::Completed {
            return Err(invalid("analyse a completed simulation"));
        }
        let target = failure_target(&resource.status, request.property.as_ref())?;
        let mut spec = resource.spec.clone();
        match &mut spec.inputs {
            InputSource::Resolved { definition } => {
                definition.start = seedbox_spec::definition::Start::Fresh;
            }
            _ => return Err(invalid("analysis needs a resolved definition")),
        }
        let seed = spec
            .seed
            .ok_or_else(|| invalid(format!("{source} recorded no seed")))?;
        let program: Vec<TimedInput> = spec.program.iter().map(timed).collect::<Result<_, _>>()?;
        let end_ns = resource.status.simulated_time_ns;
        let mut arms = vec![(Arm::Baseline, AnalysisArmKind::Baseline)];
        for at_ns in cf::times(&program, end_ns, request.times) {
            arms.push((
                Arm::ReseedFrom { at_ns },
                AnalysisArmKind::ReseedFrom { at_ns },
            ));
        }
        if request.faults {
            for keys in cf::faults(&program, end_ns) {
                let first = program.iter().find(|i| i.key == keys[0]);
                let summary = first.map(summary).unwrap_or_default();
                arms.push((
                    Arm::Without { keys: keys.clone() },
                    AnalysisArmKind::Without { keys, summary },
                ));
            }
        }
        let faults = arms
            .iter()
            .filter(|(arm, _)| matches!(arm, Arm::Without { .. }))
            .count() as u32;
        let total = arms.len() as u32 * request.samples + faults;
        if total > MAX_ANALYSIS_RUNS {
            return Err(invalid(format!(
                "{total} runs ({} arms of {} samples); at most {MAX_ANALYSIS_RUNS}",
                arms.len(),
                request.samples
            )));
        }
        // Every arm's program must be well formed before anything runs;
        // submission checks the rest.
        for (arm, _) in &arms {
            let (inputs, _) = cf::run(&program, seed, arm, 1).map_err(invalid)?;
            seedbox_engine_protocol::input_program::validate_program(&inputs, u64::MAX)
                .map_err(invalid)?;
        }
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let id = format!("analysis-{}", catalogue::random_id()?);
        let job = AnalysisJob {
            source: source.clone(),
            request: request.clone(),
            identity: FailureIdentity::of(&target, &resource.status),
            target,
            spec,
            program,
            seed,
            end_ns,
            runs: vec![Vec::new(); arms.len()],
            arms,
            submitted: false,
            error: None,
        };
        let view = analysis_view(&id, &job);
        jobs.requests.insert(request.request_id.clone(), id.clone());
        jobs.analysis_views
            .insert(id.clone(), (view.clone(), request));
        jobs.analyses.insert(id, Arc::new(Mutex::new(job)));
        Ok(view)
    }

    pub fn analysis(&self, id: &str) -> Result<Analysis, Error> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        jobs.analysis_views
            .get(id)
            .map(|(view, _)| view.clone())
            .ok_or_else(|| invalid(format!("analysis not found: {id}")))
    }

    /// Submit every run at once, variant by variant so early results cover
    /// every arm; afterwards fold the outcomes of completed runs.
    pub(super) fn advance_analysis(
        &self,
        id: &str,
        job: &mut AnalysisJob,
        queued: &mut Vec<SimulationId>,
    ) -> Result<(), Error> {
        if !job.submitted {
            for variant in 0..=job.request.samples {
                for (index, (arm, _)) in job.arms.iter().enumerate() {
                    // Only a fault arm's exact run keeps the source's seeds.
                    if variant == 0 && !matches!(arm, Arm::Without { .. }) {
                        continue;
                    }
                    let (inputs, seed) =
                        cf::run(&job.program, job.seed, arm, variant).map_err(invalid)?;
                    let mut spec = job.spec.clone();
                    spec.seed = Some(seed);
                    spec.program = inputs.iter().map(program_input).collect::<Result<_, _>>()?;
                    let mut metadata = seedbox_spec::definition::Metadata {
                        name: format!("{}-analysis", job.source),
                        ..Default::default()
                    };
                    metadata.annotations.insert("analysis".into(), id.into());
                    metadata
                        .annotations
                        .insert("derived-from".into(), job.source.to_string());
                    let request = SubmitRequest {
                        options: job.request.options.clone(),
                        metadata,
                        request_id: format!("{id}-{index}-{variant}"),
                        spec,
                        fault_profile: None,
                    };
                    let (resource, _) = self.manager.submit(request)?;
                    if resource.status.phase == Phase::Queued {
                        queued.push(resource.metadata.id.clone());
                    }
                    job.runs[index].push((variant, resource.metadata.id, None));
                }
            }
            job.submitted = true;
            return Ok(());
        }
        for runs in &mut job.runs {
            for (_, simulation, outcome) in runs.iter_mut().filter(|r| r.2.is_none()) {
                let status = self.manager.describe(simulation)?.status;
                if status.phase != Phase::Completed {
                    continue;
                }
                *outcome = Some(if job.identity.matches(&status) {
                    Outcome::Same
                } else {
                    match status.verdict() {
                        TestVerdict::Passed => Outcome::Passed,
                        TestVerdict::Failed => Outcome::Other,
                        TestVerdict::Pending | TestVerdict::Incomplete => Outcome::Incomplete,
                    }
                });
            }
        }
        Ok(())
    }
}

/// What a probe of `status` must show to fail the same way: `property`, or
/// the oracle failure, or the first violated property.
pub(super) fn failure_target(
    status: &seedbox_api::SimulationStatus,
    property: Option<&String>,
) -> Result<String, Error> {
    match property {
        Some(property) => Ok(property.clone()),
        None if status.oracle_verdict() == TestVerdict::Failed => Ok("oracle".into()),
        None => status
            .properties
            .as_ref()
            .and_then(|a| {
                a.properties
                    .iter()
                    .find(|p| p.status == PropertyStatus::Violated)
            })
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                invalid("the simulation neither failed its oracle nor violated a property")
            }),
    }
}

pub(super) fn analysis_view(id: &str, job: &AnalysisJob) -> Analysis {
    let arms = job
        .arms
        .iter()
        .zip(&job.runs)
        .map(|((_, kind), runs)| {
            let sampled = || runs.iter().filter(|r| r.0 > 0);
            let count = |o: Outcome| sampled().filter(|r| r.2 == Some(o)).count() as u32;
            let (failed, other, passed) = (
                count(Outcome::Same),
                count(Outcome::Other),
                count(Outcome::Passed),
            );
            let completed = failed + other + passed;
            let exact = runs.iter().find(|r| r.0 == 0);
            AnalysisArm {
                kind: kind.clone(),
                runs: sampled().map(|r| r.1.clone()).collect(),
                completed,
                failed,
                other,
                incomplete: count(Outcome::Incomplete),
                interval_per_mille: cf::interval(failed, completed),
                exact: exact.map(|r| r.1.clone()),
                exact_failed: exact.and_then(|r| match r.2 {
                    Some(Outcome::Same) => Some(true),
                    Some(Outcome::Incomplete) | None => None,
                    Some(_) => Some(false),
                }),
            }
        })
        .collect();
    let finished =
        job.error.is_some() || (job.submitted && job.runs.iter().flatten().all(|r| r.2.is_some()));
    Analysis {
        id: id.into(),
        source: job.source.clone(),
        target: job.target.clone(),
        phase: if finished {
            CampaignPhase::Completed
        } else {
            CampaignPhase::Running
        },
        end_ns: job.end_ns,
        arms,
        error: job.error.clone(),
    }
}
