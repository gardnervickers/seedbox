//! Server-owned campaigns and shrinks. Both are passive plans from
//! `seedbox-explore` turned into ordinary member submissions; the driver
//! only submits, waits and folds. Plans never depend on completion order,
//! capacity or wall time.
use super::*;
use seedbox_api::{
    AppliedFault, Campaign, CampaignAssessment, CampaignCompletion, CampaignMember, CampaignPhase,
    CampaignProperty, CampaignRequest, CampaignSummary, Encounter, EncounterPage, ExportState,
    Finding, FindingLabel, FindingQuery, MemberPlan, Moment, ProgramInput, Shrink, ShrinkOutcome,
    ShrinkProbe, ShrinkRequest, SubmitRequest, TestVerdict,
};
use seedbox_engine_protocol::input_program::TimedInput;
use seedbox_engine_protocol::property::{AssessmentStatus, Kind, Ledger, PropertyStatus};
use seedbox_explore::campaign::{self as plan, Planner};
use seedbox_explore::shrink::{self, Shrinker};
use std::collections::BTreeSet;

const MAX_JOBS: usize = 64;

/// Jobs each have their own lock, held only by the driver; readers use the
/// snapshot published after every step.
#[derive(Default)]
pub(crate) struct Jobs {
    campaigns: BTreeMap<String, Arc<Mutex<CampaignJob>>>,
    shrinks: BTreeMap<String, Arc<Mutex<ShrinkJob>>>,
    views: BTreeMap<String, Published>,
    shrink_views: BTreeMap<String, (Shrink, ShrinkRequest)>,
    /// Request IDs to the job they created.
    requests: BTreeMap<String, String>,
    stops: BTreeSet<String>,
}

/// A campaign as readers see it between driver steps.
struct Published {
    campaign: Campaign,
    encounters: Vec<Encounter>,
    incomplete: bool,
    request: CampaignRequest,
}

/// What aggregation reads from a campaign.
struct Context<'a> {
    members: &'a BTreeMap<u32, Member>,
    group_by: &'a [String],
    oracle: &'a str,
    exports: &'a BTreeMap<String, ExportState>,
    baseline: Option<&'a BTreeSet<String>>,
}

impl CampaignJob {
    fn context(&self) -> Context<'_> {
        Context {
            members: &self.members,
            group_by: &self.request.group_by,
            oracle: &self.oracle,
            exports: &self.exports,
            baseline: self.baseline.as_ref(),
        }
    }
}

fn publish(id: &str, job: &CampaignJob) -> Published {
    Published {
        campaign: view(id, job),
        encounters: encounters(&job.context()),
        incomplete: job.members.values().any(|m| {
            m.error.is_some()
                || m.verdict.is_none()
                || m.evidence.as_ref().is_some_and(|e| !e.complete)
        }),
        request: job.request.clone(),
    }
}

/// What a completed member showed, kept for aggregation and queries.
#[derive(Clone, Default)]
struct Evidence {
    ledger: Option<Ledger>,
    /// The parent's ledger at the branch anchor: what this member inherited.
    inherited: Option<Ledger>,
    faults: Vec<AppliedFault>,
    complete: bool,
}

struct Member {
    plan: plan::Plan,
    simulation: Option<SimulationId>,
    verdict: Option<TestVerdict>,
    error: Option<String>,
    evidence: Option<Evidence>,
    /// Program inputs, as submitted, for branching from it.
    program: Vec<TimedInput>,
    inherited: Option<Ledger>,
}

struct CampaignJob {
    request: CampaignRequest,
    created_at_unix_ms: u64,
    planner: Planner,
    members: BTreeMap<u32, Member>,
    root: Option<seedbox_api::SimulationSpec>,
    machines: Vec<String>,
    oracle: String,
    limit_ns: u64,
    stop_requested: bool,
    completion: Option<CampaignCompletion>,
    findings_dir: PathBuf,
    exports: BTreeMap<String, ExportState>,
    baseline: Option<BTreeSet<String>>,
}

/// A failure's identity: an oracle target keeps the oracle's exit code and
/// every property the source violated, so a probe cannot drift into an
/// unrelated failure (a setup error exiting the same way, say).
#[derive(Clone, Debug, PartialEq, Eq)]
struct FailureIdentity {
    exit_code: Option<u8>,
    violated: BTreeSet<String>,
}

impl FailureIdentity {
    fn of(target: &str, status: &seedbox_api::SimulationStatus) -> Self {
        if target != "oracle" {
            return Self {
                exit_code: None,
                violated: BTreeSet::from([target.to_owned()]),
            };
        }
        Self {
            exit_code: status.oracle_exit_code,
            violated: violated(status),
        }
    }
    fn matches(&self, status: &seedbox_api::SimulationStatus) -> bool {
        self.exit_code
            .is_none_or(|code| status.oracle_exit_code == Some(code))
            && self.violated.is_subset(&violated(status))
    }
}

fn violated(status: &seedbox_api::SimulationStatus) -> BTreeSet<String> {
    status.properties.as_ref().map_or_else(BTreeSet::new, |a| {
        a.properties
            .iter()
            .filter(|p| p.status == PropertyStatus::Violated)
            .map(|p| p.id.clone())
            .collect()
    })
}

struct ShrinkJob {
    source: SimulationId,
    request: ShrinkRequest,
    target: String,
    /// What a probe must show to fail the same way.
    identity: FailureIdentity,
    spec: seedbox_api::SimulationSpec,
    inputs: Vec<ProgramInput>,
    removable: u32,
    /// Inputs a probe keeps unchanged: a checkpoint's consumed prefix.
    pending: usize,
    shrinker: Shrinker,
    /// Edits the kept inputs once removal is done.
    simplifier: Option<shrink::Simplifier>,
    /// The checkpoint form of a branched source, should its full program
    /// not reproduce from a fresh start.
    fallback: Option<(seedbox_api::SimulationSpec, usize)>,
    from_checkpoint: bool,
    probes: Vec<(ShrinkProbe, Vec<String>)>,
    running: Option<SimulationId>,
    error: Option<String>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Catalogue(catalogue::Error::Invalid(message.into()))
}

fn timed(input: &ProgramInput) -> Result<TimedInput, Error> {
    Ok(TimedInput {
        key: input.key.clone(),
        at_ns: input.at_ns,
        action: serde_json::from_value(input.action.clone())
            .map_err(|e| invalid(format!("input {}: {e}", input.key)))?,
        timeout_ns: input.timeout_ns,
    })
}

fn program_input(input: &TimedInput) -> Result<ProgramInput, Error> {
    Ok(ProgramInput {
        key: input.key.clone(),
        at_ns: input.at_ns,
        action: serde_json::to_value(&input.action).map_err(std::io::Error::other)?,
        timeout_ns: input.timeout_ns,
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// One member's own counts: its ledger less what it inherited at the anchor.
fn own(evidence: &Evidence, id: &str) -> (u64, u64) {
    let total = evidence
        .ledger
        .as_ref()
        .and_then(|l| l.properties.get(id))
        .map_or((0, 0), |p| (p.passes, p.failures));
    let inherited = evidence
        .inherited
        .as_ref()
        .and_then(|l| l.properties.get(id))
        .map_or((0, 0), |p| (p.passes, p.failures));
    (
        total.0.saturating_sub(inherited.0),
        total.1.saturating_sub(inherited.1),
    )
}

impl ExecutionService {
    /// Where campaigns export findings when a request names no directory.
    pub fn with_findings_root(mut self, root: PathBuf) -> Self {
        self.findings_root = Some(root);
        self
    }

    pub fn submit_campaign(&self, request: CampaignRequest) -> Result<Campaign, Error> {
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        if let Some(id) = jobs.requests.get(&request.request_id) {
            let published = jobs
                .views
                .get(id)
                .ok_or_else(|| invalid("campaign request reused"))?;
            if published.request != request {
                return Err(Error::Catalogue(catalogue::Error::RequestConflict));
            }
            return Ok(published.campaign.clone());
        }
        if jobs.campaigns.len() + jobs.shrinks.len() >= MAX_JOBS {
            return Err(invalid(format!(
                "at most {MAX_JOBS} campaigns and shrinks per server"
            )));
        }
        if matches!(request.spec.inputs, InputSource::Checkpoint { .. }) {
            return Err(invalid(
                "a campaign starts from a fresh simulation, not a checkpoint",
            ));
        }
        let config = plan::Config {
            seed: request.plan.seed,
            first_seed: request.spec.seed.unwrap_or(request.plan.seed),
            budget: request.plan.budget,
            batch: request.plan.batch,
            fresh_permille: request.plan.fresh_permille,
            corpus_limit: request.plan.corpus_limit,
            anchors: request.plan.anchors_ns.clone(),
            mutations: plan::Mutations::default(),
        };
        let planner = Planner::new(config).map_err(invalid)?;
        if let Some(group) = request
            .group_by
            .iter()
            .find(|g| g.is_empty() || g.len() > 64)
        {
            return Err(invalid(format!("invalid finding group field {group:?}")));
        }
        // Unique across server sessions, so findings directories never mix.
        let id = format!("campaign-{}", catalogue::random_id()?);
        let findings_dir = match &request.findings_dir {
            Some(dir) => PathBuf::from(dir),
            None => self
                .findings_root
                .as_ref()
                .ok_or_else(|| invalid("no findings directory; name one"))?
                .join(&id),
        };
        if !findings_dir.is_absolute() {
            return Err(invalid(
                "the findings directory must be an absolute server path",
            ));
        }
        let baseline = match &request.baseline {
            Some(dir) => Some(read_index(Path::new(dir))?),
            None => None,
        };
        fs::create_dir_all(&findings_dir)?;
        let job = CampaignJob {
            request,
            created_at_unix_ms: now_ms(),
            planner,
            members: BTreeMap::new(),
            root: None,
            machines: Vec::new(),
            oracle: String::new(),
            limit_ns: 0,
            stop_requested: false,
            completion: None,
            findings_dir,
            exports: BTreeMap::new(),
            baseline,
        };
        let published = publish(&id, &job);
        let campaign = published.campaign.clone();
        jobs.requests
            .insert(job.request.request_id.clone(), id.clone());
        jobs.views.insert(id.clone(), published);
        jobs.campaigns.insert(id, Arc::new(Mutex::new(job)));
        Ok(campaign)
    }

    fn published<T>(&self, id: &str, read: impl FnOnce(&Published) -> T) -> Result<T, Error> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        jobs.views
            .get(id)
            .map(read)
            .ok_or_else(|| invalid(format!("campaign not found: {id}")))
    }

    pub fn campaign(&self, id: &str) -> Result<Campaign, Error> {
        self.published(id, |p| p.campaign.clone())
    }

    pub fn campaigns(&self) -> Result<Vec<CampaignSummary>, Error> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        Ok(jobs
            .views
            .iter()
            .map(|(id, p)| {
                let view = &p.campaign;
                CampaignSummary {
                    id: id.clone(),
                    name: view.name.clone(),
                    phase: view.phase,
                    completion: view.completion.clone(),
                    budget: view.plan.budget,
                    completed: view.assessment.members_completed,
                    findings: view.findings.len() as u32,
                }
            })
            .collect())
    }

    /// Stop planning and stop every running member.
    pub fn stop_campaign(&self, id: &str) -> Result<Campaign, Error> {
        let members: Vec<SimulationId> = {
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?;
            let view = jobs
                .views
                .get(id)
                .ok_or_else(|| invalid(format!("campaign not found: {id}")))?;
            let members = view
                .campaign
                .members
                .iter()
                .filter(|m| m.verdict.is_none() && m.error.is_none())
                .filter_map(|m| m.simulation.clone())
                .collect();
            jobs.stops.insert(id.into());
            members
        };
        for member in members {
            self.manager.stop(&member)?;
        }
        self.campaign(id)
    }

    pub fn campaign_encounters(
        &self,
        id: &str,
        query: &FindingQuery,
    ) -> Result<EncounterPage, Error> {
        let limit = query
            .limit
            .unwrap_or(seedbox_api::MAX_PAGE)
            .clamp(1, seedbox_api::MAX_PAGE);
        self.published(id, |p| {
            let all: Vec<&Encounter> = p.encounters.iter().filter(|e| matches(e, query)).collect();
            let start = (query.offset as usize).min(all.len());
            let end = (start + limit as usize).min(all.len());
            EncounterPage {
                encounters: all[start..end].iter().map(|e| (*e).clone()).collect(),
                total: all.len() as u32,
                next_offset: (end < all.len()).then_some(end as u32),
                incomplete: p.incomplete,
            }
        })
    }

    /// Shrink a failed simulation's external inputs.
    pub fn submit_shrink(
        &self,
        source: &SimulationId,
        request: ShrinkRequest,
    ) -> Result<Shrink, Error> {
        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?;
            if let Some(id) = jobs.requests.get(&request.request_id) {
                let (view, original) = jobs
                    .shrink_views
                    .get(id)
                    .ok_or_else(|| invalid("shrink request reused"))?;
                if original != &request || &view.source != source {
                    return Err(Error::Catalogue(catalogue::Error::RequestConflict));
                }
                return Ok(view.clone());
            }
            if jobs.campaigns.len() + jobs.shrinks.len() >= MAX_JOBS {
                return Err(invalid(format!(
                    "at most {MAX_JOBS} campaigns and shrinks per server"
                )));
            }
        }
        if request.max_probes == 0 || request.max_probes > shrink::MAX_PROBES {
            return Err(invalid(format!(
                "max probes must be 1 to {}",
                shrink::MAX_PROBES
            )));
        }
        let resource = self.portable_simulation(source)?;
        if resource.status.phase != Phase::Completed {
            return Err(invalid("shrink a completed simulation"));
        }
        let target = match &request.property {
            Some(property) => property.clone(),
            None if resource.status.oracle_verdict() == TestVerdict::Failed => "oracle".into(),
            None => resource
                .status
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
                })?,
        };
        let InputSource::Resolved { definition } = &resource.spec.inputs else {
            return Err(invalid("shrink needs a resolved definition"));
        };
        // A branched source shrinks its full program from a fresh start, so
        // its prefix can shrink too; its checkpoint, whose consumed prefix is
        // fixed, is the fallback.
        let (spec, fallback) = match &definition.start {
            seedbox_spec::definition::Start::Checkpoint { manifest, .. } => {
                let pending = manifest
                    .pointer("/state/world/input_program/next")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0) as usize;
                let mut cold = resource.spec.clone();
                if let InputSource::Resolved { definition } = &mut cold.inputs {
                    definition.start = seedbox_spec::definition::Start::Fresh;
                }
                (cold, Some((resource.spec.clone(), pending)))
            }
            seedbox_spec::definition::Start::Fresh => (resource.spec.clone(), None),
        };
        let inputs = resource.spec.program.clone();
        let timed: Vec<TimedInput> = inputs.iter().map(timed).collect::<Result<_, _>>()?;
        let candidates = shrink::candidates(&timed, 0);
        let removable = candidates.atoms.iter().map(Vec::len).sum::<usize>() as u32;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        let id = format!("shrink-{}", catalogue::random_id()?);
        let job = ShrinkJob {
            source: source.clone(),
            shrinker: Shrinker::new(candidates, request.max_probes),
            request: request.clone(),
            identity: FailureIdentity::of(&target, &resource.status),
            target,
            spec,
            inputs,
            removable,
            pending: 0,
            simplifier: None,
            fallback,
            from_checkpoint: false,
            probes: Vec::new(),
            running: None,
            error: None,
        };
        let view = shrink_view(&id, &job);
        jobs.requests.insert(request.request_id.clone(), id.clone());
        jobs.shrink_views
            .insert(id.clone(), (view.clone(), request));
        jobs.shrinks.insert(id, Arc::new(Mutex::new(job)));
        Ok(view)
    }

    pub fn shrink(&self, id: &str) -> Result<Shrink, Error> {
        let jobs = self
            .jobs
            .lock()
            .map_err(|_| catalogue::Error::StorageFailed)?;
        jobs.shrink_views
            .get(id)
            .map(|(view, _)| view.clone())
            .ok_or_else(|| invalid(format!("shrink not found: {id}")))
    }

    /// Fold finished members and submit the next ones, holding each job's
    /// own lock so reads never wait on slow evidence work. Returns newly
    /// queued simulations for the execution owner.
    pub fn advance_jobs(&self) -> Result<Vec<SimulationId>, Error> {
        let (campaigns, shrinks, stops) = {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?;
            let running = |id: &String| {
                jobs.views
                    .get(id)
                    .is_some_and(|p| p.campaign.phase == CampaignPhase::Running)
            };
            (
                jobs.campaigns
                    .iter()
                    .filter(|(id, _)| running(id))
                    .map(|(id, job)| (id.clone(), job.clone()))
                    .collect::<Vec<_>>(),
                jobs.shrinks
                    .iter()
                    .filter(|(id, _)| {
                        jobs.shrink_views
                            .get(*id)
                            .is_some_and(|(v, _)| v.phase == CampaignPhase::Running)
                    })
                    .map(|(id, job)| (id.clone(), job.clone()))
                    .collect::<Vec<_>>(),
                jobs.stops.clone(),
            )
        };
        let mut queued = Vec::new();
        for (id, job) in campaigns {
            let mut job = job.lock().map_err(|_| catalogue::Error::StorageFailed)?;
            job.stop_requested |= stops.contains(&id);
            if let Err(error) = self.advance_campaign(&id, &mut job, &mut queued) {
                if matches!(error, Error::Catalogue(catalogue::Error::Unavailable)) {
                    return Err(error);
                }
                job.completion = Some(CampaignCompletion::Failed {
                    message: error.to_string(),
                });
            }
            let published = publish(&id, &job);
            self.jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?
                .views
                .insert(id, published);
        }
        for (id, job) in shrinks {
            let mut job = job.lock().map_err(|_| catalogue::Error::StorageFailed)?;
            if let Err(error) = self.advance_shrink(&id, &mut job, &mut queued) {
                if matches!(error, Error::Catalogue(catalogue::Error::Unavailable)) {
                    return Err(error);
                }
                job.error = Some(error.to_string());
            }
            let view = shrink_view(&id, &job);
            let mut jobs = self
                .jobs
                .lock()
                .map_err(|_| catalogue::Error::StorageFailed)?;
            if let Some(entry) = jobs.shrink_views.get_mut(&id) {
                entry.0 = view;
            }
        }
        Ok(queued)
    }

    fn advance_campaign(
        &self,
        id: &str,
        job: &mut CampaignJob,
        queued: &mut Vec<SimulationId>,
    ) -> Result<(), Error> {
        // Fold every member that finished, in any order; the planner orders it.
        let running: Vec<u32> = job
            .members
            .iter()
            .filter(|(_, m)| m.simulation.is_some() && m.verdict.is_none() && m.error.is_none())
            .map(|(o, _)| *o)
            .collect();
        for ordinal in running {
            let simulation = job.members[&ordinal].simulation.clone().expect("submitted");
            let resource = self.manager.describe(&simulation)?;
            if job.root.is_none() && ordinal == 0 {
                if let InputSource::Resolved { .. } = resource.spec.inputs {
                    job.root = Some(resource.spec.clone());
                    job.machines = resource
                        .spec
                        .configuration
                        .services
                        .keys()
                        .cloned()
                        .collect();
                    job.oracle = resource.spec.configuration.check.service.clone();
                    job.limit_ns = resource.spec.configuration.limits.simulated_time_ns;
                    job.planner.set_mutations(mutations(
                        job.request.fault_profile.as_ref(),
                        &job.machines,
                        &job.oracle,
                    ));
                    if let Some(profile) = &job.request.fault_profile {
                        job.planner.set_profile_kinds(profile.faults.len() as u32);
                    }
                }
            }
            if resource.status.phase != Phase::Completed {
                continue;
            }
            self.finish_member(job, ordinal, &resource)?;
        }
        self.export_findings(id, job, queued)?;
        // Finish only once every finding has its export.
        let reproducing = job
            .exports
            .values()
            .any(|e| matches!(e, ExportState::Reproducing { .. }));
        if job.stop_requested {
            if !reproducing
                && job
                    .members
                    .values()
                    .all(|m| m.verdict.is_some() || m.error.is_some() || m.simulation.is_none())
            {
                job.completion = Some(CampaignCompletion::Cancelled);
                self.release_all(job)?;
            }
            return Ok(());
        }
        if job.planner.done() {
            self.release_all(job)?;
            if !reproducing {
                job.completion = Some(CampaignCompletion::Exhausted);
            }
            return Ok(());
        }
        for plan in job.planner.next_batch() {
            job.members.insert(
                plan.ordinal,
                Member {
                    plan,
                    simulation: None,
                    verdict: None,
                    error: None,
                    evidence: None,
                    program: Vec::new(),
                    inherited: None,
                },
            );
        }
        // Dispatch the current generation; roots after the first wait for its
        // preparation, so the campaign prepares once.
        let unsubmitted: Vec<u32> = job
            .members
            .iter()
            .filter(|(_, m)| m.simulation.is_none() && m.error.is_none())
            .map(|(o, _)| *o)
            .collect();
        for ordinal in unsubmitted {
            if ordinal > 0 && job.root.is_none() {
                break;
            }
            match self.submit_member(id, job, ordinal) {
                Ok(Some(simulation)) => queued.push(simulation),
                Ok(None) => {}
                Err(error @ Error::Catalogue(catalogue::Error::Unavailable)) => return Err(error),
                Err(error) => {
                    // A member that cannot be built still reports, so the
                    // generation folds.
                    let member = job.members.get_mut(&ordinal).expect("planned");
                    member.error = Some(error.to_string());
                    job.planner
                        .report(plan::Feedback {
                            ordinal,
                            outcome: plan::Outcome::Error,
                            features: Vec::new(),
                            anchors: Vec::new(),
                            choices: Vec::new(),
                            moments: Vec::new(),
                        })
                        .map_err(invalid)?;
                }
            }
        }
        Ok(())
    }

    fn submit_member(
        &self,
        id: &str,
        job: &mut CampaignJob,
        ordinal: u32,
    ) -> Result<Option<SimulationId>, Error> {
        let plan = job.members[&ordinal].plan.clone();
        let anchors = &job.request.plan.anchors_ns;
        let mut options = job.request.options.clone();
        let (spec, fault_profile, inherited) = match &plan.kind {
            plan::PlanKind::Root { seed, kinds } => {
                let mut spec = job.root.clone().unwrap_or_else(|| job.request.spec.clone());
                if ordinal > 0 {
                    // Later roots reuse the prepared closure with a fresh timeline.
                    if job.request.fault_profile.is_some() {
                        spec.program
                            .retain(|i| !i.key.starts_with(seedbox_explore::faults::KEY_PREFIX));
                    }
                    spec.program
                        .retain(|i| !(0..anchors.len()).any(|a| i.key == plan::anchor_key(a)));
                }
                spec.seed = Some(*seed);
                let program: Vec<TimedInput> =
                    spec.program.iter().map(timed).collect::<Result<_, _>>()?;
                let program = plan::root_program(program, anchors).map_err(invalid)?;
                spec.program = program
                    .iter()
                    .map(program_input)
                    .collect::<Result<_, _>>()?;
                options.capture_anchors = (0..anchors.len()).map(plan::anchor_key).collect();
                // A swarm root keeps only the profile entries it sampled.
                let profile = job.request.fault_profile.clone().map(|mut profile| {
                    if let Some(kinds) = kinds {
                        profile.faults = (profile.faults.into_iter().enumerate())
                            .filter(|(i, _)| kinds.contains(&(*i as u32)))
                            .map(|(_, f)| f)
                            .collect();
                    }
                    profile
                });
                (spec, profile, None)
            }
            plan::PlanKind::Branch(branch) => {
                let parent = job
                    .members
                    .get(&branch.parent)
                    .and_then(|m| m.simulation.clone())
                    .ok_or_else(|| invalid("branch parent was never submitted"))?;
                let checkpoint =
                    self.checkpoint_definition(&parent, &format!("anchor-{}", branch.anchor))?;
                let mut spec = checkpoint.spec;
                let InputSource::Resolved { definition } = &spec.inputs else {
                    return Err(invalid("checkpoint has no resolved definition"));
                };
                let inherited = match &definition.start {
                    seedbox_spec::definition::Start::Checkpoint { manifest, .. } => manifest
                        .pointer("/state/world/properties")
                        .cloned()
                        .and_then(|v| serde_json::from_value::<Ledger>(v).ok()),
                    seedbox_spec::definition::Start::Fresh => None,
                };
                let parent_program: Vec<TimedInput> =
                    spec.program.iter().map(timed).collect::<Result<_, _>>()?;
                let suffix = match &job.request.fault_profile {
                    Some(profile) => seedbox_explore::faults::generate(
                        profile,
                        branch.reseed,
                        &job.machines,
                        &job.oracle,
                        job.limit_ns,
                    )
                    .map_err(invalid)?,
                    None => Vec::new(),
                };
                let program = plan::branch_program(&parent_program, branch, ordinal, suffix)
                    .map_err(invalid)?;
                spec.program = program
                    .iter()
                    .map(program_input)
                    .collect::<Result<_, _>>()?;
                options.capture_anchors = (0..anchors.len())
                    .filter(|&a| anchors[a] > branch.at_ns)
                    .map(plan::anchor_key)
                    .collect();
                (spec, None, inherited)
            }
        };
        let mut metadata = seedbox_spec::definition::Metadata {
            name: job.request.metadata.name.clone(),
            labels: job.request.metadata.labels.clone(),
            annotations: job.request.metadata.annotations.clone(),
        };
        metadata.annotations.insert("campaign".into(), id.into());
        metadata
            .annotations
            .insert("campaign-ordinal".into(), ordinal.to_string());
        let request = SubmitRequest {
            options,
            metadata,
            request_id: format!("{id}-{ordinal}"),
            spec,
            fault_profile,
        };
        let (resource, _) = self.manager.submit(request)?;
        let member = job.members.get_mut(&ordinal).expect("planned");
        member.simulation = Some(resource.metadata.id.clone());
        member.program = resource
            .spec
            .program
            .iter()
            .map(timed)
            .collect::<Result<_, _>>()?;
        member.inherited = inherited;
        Ok((resource.status.phase == Phase::Queued).then_some(resource.metadata.id))
    }

    fn finish_member(
        &self,
        job: &mut CampaignJob,
        ordinal: u32,
        resource: &seedbox_api::Simulation,
    ) -> Result<(), Error> {
        let simulation = resource.metadata.id.clone();
        let record = self.run_record(&simulation)?;
        let report = record.as_ref().and_then(|r| r.report.as_ref());
        let ledger = report.and_then(|r| r.properties.clone());
        let choices: Vec<plan::ObservedChoice> = report
            .and_then(|r| r.choices.as_ref())
            .map(|c| {
                c.records
                    .iter()
                    .map(|r| plan::ObservedChoice {
                        machine: r.machine.clone(),
                        incarnation: r.incarnation,
                        session: r.session,
                        site: r.site.clone(),
                        occurrence: r.occurrence,
                        alternatives: r.alternatives,
                        selected: r.selected,
                        requested_at_ns: r.requested_at_ns,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let faults = report
            .map(|r| {
                r.faults
                    .iter()
                    .map(|f| AppliedFault {
                        action: f.action.clone(),
                        machine: f.service.clone(),
                        applied_at_ns: f.applied_at_ns,
                    })
                    .collect()
            })
            .unwrap_or_default();
        let verdict = resource.status.verdict();
        // Captured anchors, with the times their inputs named.
        let member = job.members.get_mut(&ordinal).expect("planned");
        let anchors: Vec<(String, u64)> = member
            .program
            .iter()
            .filter(|i| {
                matches!(
                    i.action,
                    seedbox_engine_protocol::input_program::InputAction::Branch(_)
                )
            })
            .filter(|i| {
                self.manager
                    .captured_checkpoint(&simulation, &format!("anchor-{}", i.key))
                    .is_ok()
            })
            .map(|i| (i.key.clone(), i.at_ns))
            .collect();
        let outcome = match verdict {
            TestVerdict::Passed => plan::Outcome::Passed,
            TestVerdict::Failed => plan::Outcome::Failed,
            TestVerdict::Incomplete | TestVerdict::Pending => plan::Outcome::Incomplete,
        };
        let complete = ledger.as_ref().is_none_or(|l| l.error_count == 0)
            && verdict != TestVerdict::Incomplete
            && record.is_some();
        member.verdict = Some(verdict);
        let features = plan::features(ledger.as_ref(), &choices);
        let moments = moments(
            ledger.as_ref(),
            report.map(|r| r.faults.as_slice()).unwrap_or(&[]),
        );
        member.evidence = Some(Evidence {
            ledger,
            inherited: member.inherited.clone(),
            faults,
            complete,
        });
        job.planner
            .report(plan::Feedback {
                ordinal,
                outcome,
                features,
                anchors,
                choices,
                moments,
            })
            .map_err(invalid)?;
        self.release_captures(job)
    }

    /// A finished campaign branches from nothing: free every capture.
    fn release_all(&self, job: &mut CampaignJob) -> Result<(), Error> {
        let all: Vec<u32> = job.members.keys().copied().collect();
        self.release(job, all)
    }

    /// Delete the anchor captures of members no plan will branch from.
    fn release_captures(&self, job: &mut CampaignJob) -> Result<(), Error> {
        let released = job.planner.take_released();
        self.release(job, released)
    }

    fn release(&self, job: &CampaignJob, ordinals: Vec<u32>) -> Result<(), Error> {
        for ordinal in ordinals {
            let Some(member) = job.members.get(&ordinal) else {
                continue;
            };
            let Some(simulation) = member.simulation.clone() else {
                continue;
            };
            for input in &member.program {
                let name = format!("anchor-{}", input.key);
                if self.manager.captured_checkpoint(&simulation, &name).is_ok() {
                    self.delete_checkpoint(&simulation, &name)?;
                }
            }
        }
        Ok(())
    }

    /// Export each finding's representative once, durably, and keep the
    /// directory's index current. A branched representative first reruns
    /// cold, from its full program and a fresh start; when that run's record
    /// matches the representative's, the cold run is exported instead, so the
    /// bundle carries no checkpoint. Otherwise the representative is.
    fn export_findings(
        &self,
        id: &str,
        job: &mut CampaignJob,
        queued: &mut Vec<SimulationId>,
    ) -> Result<(), Error> {
        let current = findings(&job.context());
        for finding in &current {
            let state = match job.exports.get(&finding.group) {
                None => {
                    let branched = job
                        .members
                        .get(&finding.ordinal)
                        .is_some_and(|m| matches!(m.plan.kind, plan::PlanKind::Branch(_)));
                    match branched.then(|| self.submit_cold(id, job, finding)) {
                        Some(Ok((simulation, is_queued))) => {
                            if is_queued {
                                queued.push(simulation.clone());
                            }
                            ExportState::Reproducing { simulation }
                        }
                        Some(Err(Error::Catalogue(catalogue::Error::Unavailable))) => {
                            return Err(Error::Catalogue(catalogue::Error::Unavailable));
                        }
                        Some(Err(_)) => self.export(job, finding, &finding.representative, true),
                        None => self.export(job, finding, &finding.representative, false),
                    }
                }
                Some(ExportState::Reproducing { simulation }) => {
                    let simulation = simulation.clone();
                    if self.manager.describe(&simulation)?.status.phase != Phase::Completed {
                        continue;
                    }
                    if self.cold_matches(&finding.representative, &simulation)? {
                        self.export(job, finding, &simulation, false)
                    } else {
                        self.export(job, finding, &finding.representative, true)
                    }
                }
                Some(_) => continue,
            };
            job.exports.insert(finding.group.clone(), state);
        }
        write_index(&job.findings_dir, id, &findings(&job.context()))
    }

    fn export(
        &self,
        job: &CampaignJob,
        finding: &Finding,
        simulation: &SimulationId,
        from_checkpoint: bool,
    ) -> ExportState {
        match self.publish_bundle(simulation, &job.findings_dir, &finding.group) {
            Ok(path) => ExportState::Published {
                path: path.display().to_string(),
                simulation: simulation.clone(),
                from_checkpoint,
            },
            Err(error) => ExportState::Failed {
                message: error.to_string(),
            },
        }
    }

    /// Submit a representative's full program from a fresh start, capturing
    /// nothing. Returns the simulation and whether it was queued.
    fn submit_cold(
        &self,
        id: &str,
        job: &CampaignJob,
        finding: &Finding,
    ) -> Result<(SimulationId, bool), Error> {
        let mut spec = self.manager.describe(&finding.representative)?.spec;
        let InputSource::Resolved { definition } = &mut spec.inputs else {
            return Err(invalid("representative has no resolved definition"));
        };
        definition.start = seedbox_spec::definition::Start::Fresh;
        let mut options = job.request.options.clone();
        options.capture_anchors.clear();
        let mut metadata = seedbox_spec::definition::Metadata {
            name: job.request.metadata.name.clone(),
            labels: job.request.metadata.labels.clone(),
            annotations: job.request.metadata.annotations.clone(),
        };
        metadata.annotations.insert("campaign".into(), id.into());
        metadata
            .annotations
            .insert("cold-rerun-of".into(), finding.representative.to_string());
        let request = SubmitRequest {
            options,
            metadata,
            request_id: format!("{id}-cold-{}", finding.ordinal),
            spec,
            fault_profile: None,
        };
        let (resource, _) = self.manager.submit(request)?;
        let is_queued = resource.status.phase == Phase::Queued;
        Ok((resource.metadata.id, is_queued))
    }

    /// The cold rerun reproduced the representative: same outcome, result,
    /// fault history and witness.
    fn cold_matches(
        &self,
        representative: &SimulationId,
        cold: &SimulationId,
    ) -> Result<bool, Error> {
        let (Some(first), Some(second)) =
            (self.run_record(representative)?, self.run_record(cold)?)
        else {
            return Ok(false);
        };
        Ok(seedbox_artifacts::record::compare(&first, &second).status
            == seedbox_artifacts::record::ReplayStatus::Match)
    }

    fn publish_bundle(
        &self,
        simulation: &SimulationId,
        dir: &Path,
        group: &str,
    ) -> Result<PathBuf, Error> {
        let name: String = group
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(96)
            .collect();
        let path = dir.join(format!("{name}-{simulation}.seedbox.tar"));
        let temporary = dir.join(format!(".{name}-{simulation}.partial"));
        let mut file = fs::File::create(&temporary)?;
        let options = seedbox_artifacts::archive::Options {
            max_bytes: seedbox_api::BUNDLE_MAX_BYTES,
            cancellation: CancellationToken::default(),
        };
        let result = self
            .export_bundle_with(simulation, &mut file, &options, false)
            .and_then(|()| file.sync_all().map_err(Error::from))
            .and_then(|()| fs::hard_link(&temporary, &path).map_err(Error::from));
        let _ = fs::remove_file(&temporary);
        result?;
        fs::File::open(dir)?.sync_all()?;
        Ok(path)
    }

    fn advance_shrink(
        &self,
        id: &str,
        job: &mut ShrinkJob,
        queued: &mut Vec<SimulationId>,
    ) -> Result<(), Error> {
        if let Some(running) = job.running.clone() {
            let resource = self.manager.describe(&running)?;
            if resource.status.phase != Phase::Completed {
                return Ok(());
            }
            let reproduced = job.identity.matches(&resource.status);
            if let Some((probe, _)) = job.probes.last_mut() {
                probe.reproduced = Some(reproduced);
            }
            job.running = None;
            match &mut job.simplifier {
                Some(simplifier) => simplifier.report(reproduced).map_err(invalid)?,
                None => job.shrinker.report(reproduced).map_err(invalid)?,
            }
        }
        let spent = job.probes.len() as u32;
        let remaining = job.request.max_probes.saturating_sub(spent);
        if job.shrinker.result() == Some(&shrink::Result::NotReproduced) && remaining > 0 {
            if let Some((spec, pending)) = job.fallback.take() {
                // The fresh start did not reproduce: shrink from the checkpoint.
                let timed: Vec<TimedInput> =
                    job.inputs.iter().map(timed).collect::<Result<_, _>>()?;
                let candidates = shrink::candidates(&timed, pending);
                job.removable = candidates.atoms.iter().map(Vec::len).sum::<usize>() as u32;
                job.shrinker = Shrinker::new(candidates, remaining);
                job.spec = spec;
                job.pending = pending;
                job.from_checkpoint = true;
            }
        }
        if job.simplifier.is_none() {
            if let Some(shrink::Result::Reduced { keep } | shrink::Result::Minimal { keep }) =
                job.shrinker.result()
            {
                let program: Vec<TimedInput> = job
                    .inputs
                    .iter()
                    .filter(|i| keep.contains(&i.key))
                    .map(timed)
                    .collect::<Result<_, _>>()?;
                job.simplifier = Some(shrink::Simplifier::new(program, job.pending, remaining));
            }
        }
        let (program, edit) = match &mut job.simplifier {
            Some(simplifier) => match simplifier.next_probe() {
                Some((program, summary)) => (
                    program
                        .iter()
                        .map(program_input)
                        .collect::<Result<Vec<_>, _>>()?,
                    Some(summary),
                ),
                None => return Ok(()),
            },
            None => {
                let Some(keep) = job.shrinker.next_probe() else {
                    return Ok(());
                };
                let program = job
                    .inputs
                    .iter()
                    .filter(|i| keep.contains(&i.key))
                    .cloned()
                    .collect();
                (program, None)
            }
        };
        let keep: Vec<String> = program.iter().map(|i| i.key.clone()).collect();
        let mut spec = job.spec.clone();
        spec.program = program;
        let mut metadata = seedbox_spec::definition::Metadata {
            name: format!("{}-shrink", job.source),
            ..Default::default()
        };
        metadata.annotations.insert("shrink".into(), id.into());
        metadata
            .annotations
            .insert("derived-from".into(), job.source.to_string());
        let request = SubmitRequest {
            options: job.request.options.clone(),
            metadata,
            request_id: format!("{id}-{}", job.probes.len()),
            spec,
            fault_profile: None,
        };
        let (resource, _) = self.manager.submit(request)?;
        job.probes.push((
            ShrinkProbe {
                simulation: resource.metadata.id.clone(),
                inputs: keep.len() as u32,
                edit,
                reproduced: None,
            },
            keep,
        ));
        job.running = Some(resource.metadata.id.clone());
        if resource.status.phase == Phase::Queued {
            queued.push(resource.metadata.id);
        }
        Ok(())
    }
}

/// "crash store for 300ms, 1ms after phase=compacting #1 on store".
fn mutation_summary(m: &plan::Mutation) -> String {
    use seedbox_control::EventSelector;
    let (what, machine) = match &m.when {
        EventSelector::Event { name, machine } => (format!("event {name}"), machine.as_ref()),
        EventSelector::State {
            name,
            value,
            machine,
        } => (
            format!("{name}={}", value.as_deref().unwrap_or("*")),
            machine.as_ref(),
        ),
        other => (other.name().to_owned(), None),
    };
    let on = machine.map(|m| format!(" on {m}")).unwrap_or_default();
    format!(
        "{} {} for {}ms, {}ms after {what} #{}{on}",
        format!("{:?}", m.kind).to_lowercase(),
        m.target,
        m.duration_ns / 1_000_000,
        m.delay_ns / 1_000_000,
        m.nth
    )
}

/// The mutations a fault profile allows, on its targets (every machine but
/// the oracle when it names none). No profile, no mutations.
fn mutations(
    profile: Option<&seedbox_spec::fault_profile::FaultProfile>,
    machines: &[String],
    oracle: &str,
) -> plan::Mutations {
    use seedbox_spec::fault_profile::FaultKind;
    let Some(profile) = profile else {
        return plan::Mutations::default();
    };
    let mut kinds = Vec::new();
    for fault in &profile.faults {
        let kind = match fault {
            FaultKind::Crash { .. } => plan::MutationKind::Crash,
            FaultKind::Partition { .. } => plan::MutationKind::Isolate,
            FaultKind::Pause { .. } => plan::MutationKind::Pause,
            _ => continue,
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    let mut all: Vec<String> = machines.to_vec();
    if !oracle.is_empty() && !all.iter().any(|m| m == oracle) {
        all.push(oracle.into());
    }
    let targets = if profile.targets.is_empty() {
        machines.iter().filter(|m| *m != oracle).cloned().collect()
    } else {
        profile.targets.clone()
    };
    plan::Mutations {
        kinds,
        targets,
        machines: all,
    }
}

/// What a member did that a trigger can select: its first SDK events and
/// state values, and its machines' crashes and reboots.
fn moments(
    ledger: Option<&Ledger>,
    faults: &[seedbox_artifacts::execution::FaultEvent],
) -> Vec<(seedbox_control::EventSelector, u64)> {
    use seedbox_control::{EventSelector, Transition};
    let mut out = Vec::new();
    if let Some(ledger) = ledger {
        for (name, origin) in &ledger.event_first {
            out.push((
                EventSelector::Event {
                    name: name.clone(),
                    machine: Some(origin.machine.clone()),
                },
                origin.time_ns,
            ));
        }
        for (key, origin) in &ledger.states {
            let Some((machine_name, value)) = key.split_once('=') else {
                continue;
            };
            let Some((machine, name)) = machine_name.split_once('/') else {
                continue;
            };
            out.push((
                EventSelector::State {
                    name: name.into(),
                    value: Some(value.into()),
                    machine: Some(machine.into()),
                },
                origin.time_ns,
            ));
        }
    }
    let mut seen = BTreeSet::new();
    for fault in faults {
        let transition = match fault.action.as_str() {
            "crash" => Transition::Crashed,
            "reboot" => Transition::Rebooted,
            _ => continue,
        };
        if fault.service.is_empty() || !seen.insert((fault.service.clone(), fault.action.clone())) {
            continue;
        }
        out.push((
            EventSelector::Lifecycle {
                machine: fault.service.clone(),
                transition,
            },
            fault.applied_at_ns,
        ));
    }
    out
}

fn view(id: &str, job: &CampaignJob) -> Campaign {
    let members = job
        .members
        .values()
        .map(|m| {
            let node = job.planner.nodes().get(m.plan.ordinal as usize);
            CampaignMember {
                ordinal: m.plan.ordinal,
                generation: m.plan.generation,
                plan: match &m.plan.kind {
                    plan::PlanKind::Root { seed, kinds } => MemberPlan::Root {
                        seed: *seed,
                        kinds: kinds.clone(),
                    },
                    plan::PlanKind::Branch(b) => MemberPlan::Branch {
                        parent: b.parent,
                        anchor: b.anchor.clone(),
                        reseed: b.reseed,
                        choices: b.choices.len() as u32,
                        mutation: b.mutation.as_ref().map(mutation_summary),
                    },
                },
                simulation: m.simulation.clone(),
                verdict: m.verdict,
                novelty: node.map_or(0, |n| n.novelty),
                in_corpus: node.is_some_and(|n| n.in_corpus),
                error: m.error.clone(),
            }
        })
        .collect();
    let findings = findings(&job.context());
    let not_observed = job.baseline.as_ref().map_or_else(Vec::new, |groups| {
        groups
            .iter()
            .filter(|g| !findings.iter().any(|f| &f.group == *g))
            .cloned()
            .collect()
    });
    Campaign {
        id: id.into(),
        name: job.request.metadata.name.clone(),
        created_at_unix_ms: job.created_at_unix_ms,
        plan: job.request.plan.clone(),
        phase: if job.completion.is_some() {
            CampaignPhase::Completed
        } else {
            CampaignPhase::Running
        },
        completion: job.completion.clone(),
        generations: job
            .members
            .values()
            .map(|m| m.plan.generation + 1)
            .max()
            .unwrap_or(0),
        features: job.planner.features_seen() as u32,
        corpus: job.planner.corpus_len() as u32,
        members,
        assessment: assessment(&job.context()),
        findings,
        findings_dir: job.findings_dir.display().to_string(),
        not_observed,
    }
}

/// Every property across completed members, each counted once per member's
/// own evaluations.
fn assessment(ctx: &Context<'_>) -> CampaignAssessment {
    let done: Vec<&Member> = ctx
        .members
        .values()
        .filter(|m| m.verdict.is_some() || m.error.is_some())
        .collect();
    let mut kinds: BTreeMap<String, Kind> = BTreeMap::new();
    for m in &done {
        if let Some(ledger) = m.evidence.as_ref().and_then(|e| e.ledger.as_ref()) {
            for (id, p) in &ledger.properties {
                kinds.entry(id.clone()).or_insert(p.kind);
            }
        }
    }
    let properties: Vec<CampaignProperty> = kinds
        .into_iter()
        .map(|(id, kind)| {
            let (mut passes, mut failures, mut evaluated, mut violating) = (0, 0, 0, 0);
            for e in done.iter().filter_map(|m| m.evidence.as_ref()) {
                let (p, f) = own(e, &id);
                passes += p;
                failures += f;
                evaluated += u32::from(p + f > 0);
                let mine = seedbox_engine_protocol::property::Property {
                    kind,
                    message: None,
                    declared: true,
                    passes: p,
                    failures: f,
                    first_pass: None,
                    first_failure: None,
                };
                violating += u32::from(p + f > 0 && mine.status() == PropertyStatus::Violated);
            }
            let total = seedbox_engine_protocol::property::Property {
                kind,
                message: None,
                declared: true,
                passes,
                failures,
                first_pass: None,
                first_failure: None,
            };
            CampaignProperty {
                id,
                kind,
                status: total.status(),
                passes,
                failures,
                members_evaluated: evaluated,
                members_violating: violating,
            }
        })
        .collect();
    let members_incomplete = done
        .iter()
        .filter(|m| m.error.is_some() || m.evidence.as_ref().is_some_and(|e| !e.complete))
        .count() as u32;
    let members_failed = done
        .iter()
        .filter(|m| m.verdict == Some(TestVerdict::Failed))
        .count() as u32;
    let status = if properties
        .iter()
        .any(|p| p.status == PropertyStatus::Violated)
        || members_failed > 0
    {
        AssessmentStatus::Violated
    } else if members_incomplete > 0 {
        AssessmentStatus::Incomplete
    } else {
        AssessmentStatus::Passed
    };
    CampaignAssessment {
        status,
        members_completed: done.len() as u32,
        members_incomplete,
        members_failed,
        properties,
    }
}

fn ancestry(ctx: &Context<'_>, ordinal: u32) -> Vec<u32> {
    let mut chain = Vec::new();
    let mut current = ordinal;
    while let Some(plan::PlanKind::Branch(b)) = ctx.members.get(&current).map(|m| &m.plan.kind) {
        chain.push(b.parent);
        current = b.parent;
    }
    chain.reverse();
    chain
}

/// The group a violation belongs to: its property and selected detail values.
fn group_of(ctx: &Context<'_>, id: &str, details: Option<&serde_json::Value>) -> String {
    let mut group = id.to_owned();
    for field in ctx.group_by {
        let value = details
            .and_then(|d| d.get(field))
            .map_or("-".into(), |v| v.to_string());
        group.push_str(&format!(" {field}={value}"));
    }
    group
}

/// Each member's own violations, in ordinal order.
fn encounters(ctx: &Context<'_>) -> Vec<Encounter> {
    let mut out = Vec::new();
    for (ordinal, m) in ctx.members {
        let (Some(evidence), Some(simulation)) = (&m.evidence, &m.simulation) else {
            continue;
        };
        let moment = |machine: &str, incarnation: u32, time_ns: u64| Moment {
            ordinal: *ordinal,
            simulation: simulation.clone(),
            machine: machine.into(),
            incarnation,
            time_ns,
        };
        if let Some(ledger) = &evidence.ledger {
            for (id, p) in &ledger.properties {
                let (passes, failures) = own(evidence, id);
                let mine = seedbox_engine_protocol::property::Property {
                    passes,
                    failures,
                    ..p.clone()
                };
                if passes + failures == 0 || mine.status() != PropertyStatus::Violated {
                    continue;
                }
                let example = match p.kind {
                    Kind::Unreachable => p.first_pass.as_ref(),
                    _ => p.first_failure.as_ref(),
                };
                let Some(example) = example else { continue };
                out.push(Encounter {
                    group: group_of(ctx, id, example.details.as_ref()),
                    moment: moment(
                        &example.origin.machine,
                        example.origin.incarnation,
                        example.origin.time_ns,
                    ),
                    ancestry: ancestry(ctx, *ordinal),
                    faults: evidence.faults.clone(),
                    history_complete: evidence.complete,
                });
            }
        }
        if m.verdict == Some(TestVerdict::Failed)
            && !out.iter().any(|e| e.moment.ordinal == *ordinal)
        {
            out.push(Encounter {
                group: "oracle".into(),
                moment: moment(ctx.oracle, 0, 0),
                ancestry: ancestry(ctx, *ordinal),
                faults: evidence.faults.clone(),
                history_complete: evidence.complete,
            });
        }
    }
    out
}

fn matches(e: &Encounter, q: &FindingQuery) -> bool {
    let property = e.group.split(' ').next().unwrap_or("");
    q.property.as_ref().is_none_or(|p| p == property)
        && q.machine
            .as_ref()
            .is_none_or(|m| &e.moment.machine == m || e.faults.iter().any(|f| &f.machine == m))
        && q.fault
            .as_ref()
            .is_none_or(|a| e.faults.iter().any(|f| &f.action == a))
        && q.from_ns.is_none_or(|t| e.moment.time_ns >= t)
        && q.to_ns.is_none_or(|t| e.moment.time_ns <= t)
        && q.ancestor
            .is_none_or(|a| a == e.moment.ordinal || e.ancestry.contains(&a))
        && q.preceded_by.as_ref().is_none_or(|a| {
            let within = q.within_ns.unwrap_or(u64::MAX);
            e.faults.iter().any(|f| {
                &f.action == a
                    && f.applied_at_ns <= e.moment.time_ns
                    && e.moment.time_ns - f.applied_at_ns <= within
            })
        })
}

fn findings(ctx: &Context<'_>) -> Vec<Finding> {
    let all = encounters(ctx);
    let mut groups: BTreeMap<String, Vec<&Encounter>> = BTreeMap::new();
    for e in &all {
        groups.entry(e.group.clone()).or_default().push(e);
    }
    groups
        .into_iter()
        .map(|(group, list)| {
            let first = list[0];
            let property = group
                .split(' ')
                .next()
                .filter(|p| *p != "oracle")
                .map(str::to_owned);
            let passing = property.as_ref().and_then(|id| {
                ctx.members.iter().find_map(|(ordinal, m)| {
                    let p = m.evidence.as_ref()?.ledger.as_ref()?.properties.get(id)?;
                    let example = p.first_pass.as_ref()?;
                    Some(Moment {
                        ordinal: *ordinal,
                        simulation: m.simulation.clone()?,
                        machine: example.origin.machine.clone(),
                        incarnation: example.origin.incarnation,
                        time_ns: example.origin.time_ns,
                    })
                })
            });
            let label = ctx.baseline.map(|b| {
                if b.contains(&group) {
                    FindingLabel::ObservedAgain
                } else {
                    FindingLabel::New
                }
            });
            Finding {
                export: ctx
                    .exports
                    .get(&group)
                    .cloned()
                    .unwrap_or(ExportState::Pending),
                reproduction: format!("seedbox replay {}", first.moment.simulation),
                group,
                property,
                label,
                representative: first.moment.simulation.clone(),
                ordinal: first.moment.ordinal,
                ancestry: first.ancestry.clone(),
                failing: (first.moment.time_ns > 0).then(|| first.moment.clone()),
                passing,
                members: list.iter().map(|e| e.moment.ordinal).collect(),
                faults: first.faults.clone(),
                evidence_incomplete: !first.history_complete,
            }
        })
        .collect()
}

fn shrink_view(id: &str, job: &ShrinkJob) -> Shrink {
    let (outcome, kept) = match job.shrinker.result() {
        None => (None, Vec::new()),
        Some(shrink::Result::NotReproduced) => (Some(ShrinkOutcome::NotReproduced), Vec::new()),
        Some(shrink::Result::Reduced { keep }) => (Some(ShrinkOutcome::Reduced), keep.clone()),
        Some(shrink::Result::Minimal { keep }) => (Some(ShrinkOutcome::Minimal), keep.clone()),
    };
    // The last probe that ran the kept inputs, as simplified so far (an
    // edit may reorder them).
    let kept_set: BTreeSet<&String> = kept.iter().collect();
    let reproduction = job
        .probes
        .iter()
        .rev()
        .find(|(probe, keys)| {
            probe.reproduced == Some(true) && keys.iter().collect::<BTreeSet<_>>() == kept_set
        })
        .map(|(probe, _)| probe.simulation.clone());
    let finished = job.error.is_some()
        || outcome == Some(ShrinkOutcome::NotReproduced)
        || job.simplifier.as_ref().is_some_and(|s| s.done());
    Shrink {
        id: id.into(),
        source: job.source.clone(),
        target: job.target.clone(),
        phase: if finished {
            CampaignPhase::Completed
        } else {
            CampaignPhase::Running
        },
        inputs: job.inputs.len() as u32,
        removable: job.removable,
        probes: job.probes.iter().map(|(p, _)| p.clone()).collect(),
        outcome,
        kept,
        edits: job
            .simplifier
            .as_ref()
            .map(|s| s.accepted())
            .unwrap_or_default(),
        from_checkpoint: job.from_checkpoint,
        reproduction,
        error: job.error.clone(),
    }
}

/// The findings directory's durable index, for later baselines.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Index {
    campaign: String,
    findings: Vec<Finding>,
}

fn write_index(dir: &Path, id: &str, findings: &[Finding]) -> Result<(), Error> {
    let index = Index {
        campaign: id.into(),
        findings: findings.to_vec(),
    };
    let temporary = dir.join(".findings.json.partial");
    let mut file = fs::File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut file, &index).map_err(std::io::Error::other)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join("findings.json"))?;
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

fn read_index(dir: &Path) -> Result<BTreeSet<String>, Error> {
    let path = dir.join("findings.json");
    let file =
        fs::File::open(&path).map_err(|e| invalid(format!("baseline {}: {e}", path.display())))?;
    let index: Index = serde_json::from_reader(file)
        .map_err(|e| invalid(format!("baseline {}: {e}", path.display())))?;
    Ok(index.findings.into_iter().map(|f| f.group).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_oracle_shrink_keeps_its_exit_code_and_violations() {
        let status = |code: Option<u8>, violated: &[&str]| -> seedbox_api::SimulationStatus {
            let mut status: seedbox_api::SimulationStatus =
                serde_json::from_value(serde_json::json!({
                    "phase": "completed", "stage": null, "simulatedTimeNs": 0, "playback": null,
                    "stageTimings": {}, "oracleExitCode": code, "termination": null,
                    "stopRequested": false, "engine": null, "properties": null
                }))
                .unwrap();
            let mut ledger = Ledger::default();
            for id in violated {
                ledger.properties.insert(
                    (*id).into(),
                    seedbox_engine_protocol::property::Property {
                        kind: Kind::Always,
                        message: None,
                        declared: true,
                        passes: 0,
                        failures: 1,
                        first_pass: None,
                        first_failure: None,
                    },
                );
            }
            status.properties = Some(ledger.assessment());
            status
        };
        let source = status(Some(1), &["lost writes"]);
        let identity = FailureIdentity::of("oracle", &source);
        assert!(identity.matches(&source));
        // The same exit code from a setup failure is a different failure.
        assert!(!identity.matches(&status(Some(1), &[])));
        assert!(!identity.matches(&status(Some(2), &["lost writes"])));
        assert!(identity.matches(&status(Some(1), &["lost writes", "other"])));
        let property = FailureIdentity::of("lost writes", &source);
        assert!(property.matches(&status(Some(0), &["lost writes"])));
        assert!(!property.matches(&status(Some(1), &[])));
    }
    use seedbox_engine_protocol::property::{Example, Origin, Property};

    fn ledger(passes: u64, failures: u64, failed_at: u64) -> Ledger {
        let example = |time_ns| Example {
            origin: Origin {
                machine: "db".into(),
                incarnation: 0,
                session: 1,
                seq: 1,
                time_ns,
            },
            details: Some(serde_json::json!({"key": "k7"})),
            details_truncated: false,
        };
        let mut ledger = Ledger::default();
        ledger.properties.insert(
            "p".into(),
            Property {
                kind: Kind::Always,
                message: None,
                declared: true,
                passes,
                failures,
                first_pass: (passes > 0).then(|| example(10)),
                first_failure: (failures > 0).then(|| example(failed_at)),
            },
        );
        ledger
    }

    fn member(
        ordinal: u32,
        parent: Option<u32>,
        total: Ledger,
        inherited: Option<Ledger>,
        verdict: TestVerdict,
        faults: Vec<AppliedFault>,
    ) -> Member {
        let kind = match parent {
            None => plan::PlanKind::Root {
                seed: 1,
                kinds: None,
            },
            Some(parent) => plan::PlanKind::Branch(Box::new(plan::Branch {
                parent,
                anchor: "a0".into(),
                at_ns: 5,
                reseed: 1,
                identity: "c".into(),
                choices: Vec::new(),
                mutation: None,
            })),
        };
        Member {
            plan: plan::Plan {
                ordinal,
                generation: ordinal,
                kind,
            },
            simulation: Some(
                ["k7m2p9qx", "k7m2p9qy", "k7m2p9qz"][ordinal as usize]
                    .parse()
                    .unwrap(),
            ),
            verdict: Some(verdict),
            error: None,
            evidence: Some(Evidence {
                ledger: Some(total),
                inherited,
                faults,
                complete: true,
            }),
            program: Vec::new(),
            inherited: None,
        }
    }

    fn members() -> BTreeMap<u32, Member> {
        let crash = AppliedFault {
            action: "crash".into(),
            machine: "db".into(),
            applied_at_ns: 40,
        };
        BTreeMap::from([
            (
                0,
                member(
                    0,
                    None,
                    ledger(3, 0, 0),
                    None,
                    TestVerdict::Passed,
                    Vec::new(),
                ),
            ),
            (
                1,
                member(
                    1,
                    Some(0),
                    ledger(3, 1, 50),
                    Some(ledger(2, 0, 0)),
                    TestVerdict::Failed,
                    vec![crash],
                ),
            ),
            // Its violation happened before its anchor: the parent's, not its own.
            (
                2,
                member(
                    2,
                    Some(1),
                    ledger(3, 1, 50),
                    Some(ledger(3, 1, 50)),
                    TestVerdict::Failed,
                    Vec::new(),
                ),
            ),
        ])
    }

    #[test]
    fn inherited_encounters_count_once_and_queries_filter_by_history() {
        let members = members();
        let baseline = BTreeSet::from(["oracle".to_string()]);
        let exports = BTreeMap::new();
        let group_by = vec!["key".to_string()];
        let ctx = Context {
            members: &members,
            group_by: &group_by,
            oracle: "oracle",
            exports: &exports,
            baseline: Some(&baseline),
        };
        let a = assessment(&ctx);
        let p = &a.properties[0];
        assert_eq!((p.passes, p.failures, p.members_violating), (4, 1, 1));
        assert_eq!(p.status, PropertyStatus::Violated);
        assert_eq!((a.members_completed, a.members_failed), (3, 2));

        let all = encounters(&ctx);
        let summary: Vec<(&str, u32, Vec<u32>)> = all
            .iter()
            .map(|e| (e.group.as_str(), e.moment.ordinal, e.ancestry.clone()))
            .collect();
        assert_eq!(
            summary,
            [("p key=\"k7\"", 1, vec![0]), ("oracle", 2, vec![0, 1])]
        );

        let query = |q: FindingQuery| -> Vec<u32> {
            all.iter()
                .filter(|e| matches(e, &q))
                .map(|e| e.moment.ordinal)
                .collect()
        };
        let preceded = |within| FindingQuery {
            preceded_by: Some("crash".into()),
            within_ns: Some(within),
            ..Default::default()
        };
        assert_eq!(query(preceded(10)), [1]);
        assert!(query(preceded(9)).is_empty());
        let ancestor = FindingQuery {
            ancestor: Some(1),
            ..Default::default()
        };
        assert_eq!(query(ancestor), [1, 2]);
        let property = FindingQuery {
            property: Some("p".into()),
            from_ns: Some(50),
            to_ns: Some(50),
            ..Default::default()
        };
        assert_eq!(query(property), [1]);

        let findings = findings(&ctx);
        let labels: Vec<(&str, Option<FindingLabel>, u32)> = findings
            .iter()
            .map(|f| (f.group.as_str(), f.label, f.ordinal))
            .collect();
        assert_eq!(
            labels,
            [
                ("oracle", Some(FindingLabel::ObservedAgain), 2),
                ("p key=\"k7\"", Some(FindingLabel::New), 1),
            ]
        );
        assert_eq!(findings[1].passing.as_ref().unwrap().ordinal, 0);
        assert_eq!(findings[1].export, ExportState::Pending);
    }
}
