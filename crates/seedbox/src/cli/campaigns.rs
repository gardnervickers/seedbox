//! Guided campaigns, their findings, and input shrinking, through the server.
use super::simulations::{client_error, load_spec, runtime};
use super::{daemon, output, ComposeArg};
use seedbox_api::{
    Campaign, CampaignCompletion, CampaignPhase, CampaignPlan, CampaignRequest, FindingQuery,
    MemberPlan, Shrink, ShrinkOutcome, SimulationId,
};
use seedbox_application::{execution::CancellationToken, Error, Result};
use seedbox_client::Client;
use seedbox_spec::fault_profile::FaultProfile;
use std::time::Duration;

/// What a campaign request asks for, beyond the root simulation.
pub(super) struct Settings {
    pub seed: Option<u64>,
    pub planner_seed: Option<u64>,
    pub budget: u32,
    pub batch: u32,
    pub fresh_permille: u32,
    pub corpus: u32,
    pub anchors: Vec<u64>,
    pub group_by: Vec<String>,
    pub findings_dir: Option<std::path::PathBuf>,
    pub baseline: Option<std::path::PathBuf>,
    pub options: seedbox_api::ExecutionOptions,
    pub fault_profile: Option<FaultProfile>,
}

/// Default candidate anchors: 2s and 8s within the time limit.
fn default_anchors(limit_ns: u64) -> Vec<u64> {
    [2_000_000_000, 8_000_000_000]
        .into_iter()
        .filter(|&at| at < limit_ns)
        .collect()
}

fn absolute(path: &std::path::Path, flag: &str) -> Result<String> {
    let path = std::path::absolute(path)
        .map_err(|e| Error::Setup(format!("{flag} {}: {e}", path.display())))?;
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| Error::Setup(format!("{flag} {}: not UTF-8", path.display())))
}

/// A camelCase serde name as a SCREAMING_SNAKE_CASE status word.
fn screaming(value: &impl serde::Serialize) -> String {
    let name = match serde_json::to_value(value) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(serde_json::Value::Object(map)) => map
            .get("reason")
            .and_then(|r| r.as_str())
            .unwrap_or("unknown")
            .to_owned(),
        _ => "unknown".into(),
    };
    let mut word = String::new();
    for c in name.chars() {
        if c.is_ascii_uppercase() && !word.is_empty() {
            word.push('_');
        }
        word.push(c.to_ascii_uppercase());
    }
    word
}

/// The same name as lowercase words.
fn words(value: &impl serde::Serialize) -> String {
    screaming(value).to_lowercase().replace('_', " ")
}

/// The status line and exit code a finished campaign earns.
fn outcome(campaign: &Campaign) -> (&'static str, u8) {
    use seedbox_engine_protocol::property::AssessmentStatus;
    match (&campaign.completion, campaign.assessment.status) {
        (Some(CampaignCompletion::Failed { .. }), _) => ("ERROR", 2),
        (_, AssessmentStatus::Violated) => ("FAIL", 1),
        (Some(CampaignCompletion::Cancelled), _) | (_, AssessmentStatus::Incomplete) => {
            ("INCOMPLETE", 3)
        }
        _ => ("PASS", 0),
    }
}

pub(super) fn campaign(
    compose: &ComposeArg,
    settings: Settings,
    detach: bool,
    json: bool,
) -> Result<u8> {
    let (spec, metadata) = load_spec(compose, settings.seed)?;
    let anchors = if settings.anchors.is_empty() {
        default_anchors(spec.configuration.limits.simulated_time_ns)
    } else {
        settings.anchors
    };
    let first = spec.seed.or(spec.configuration.seed);
    super::defaults::warn_dirty_build();
    let request = CampaignRequest {
        request_id: Client::request_id().map_err(client_error)?,
        metadata,
        plan: CampaignPlan {
            seed: settings.planner_seed.or(first).unwrap_or(1),
            budget: settings.budget,
            batch: settings.batch,
            fresh_permille: settings.fresh_permille,
            corpus_limit: settings.corpus,
            anchors_ns: anchors,
        },
        spec,
        options: settings.options,
        fault_profile: settings.fault_profile,
        group_by: settings.group_by,
        findings_dir: settings
            .findings_dir
            .as_deref()
            .map(|p| absolute(p, "--findings-dir"))
            .transpose()?,
        baseline: settings
            .baseline
            .as_deref()
            .map(|p| absolute(p, "--baseline"))
            .transpose()?,
    };
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let mut campaign = client
            .submit_campaign(&request)
            .await
            .map_err(client_error)?;
        if detach {
            if json {
                output::json(&campaign)?;
            } else {
                output::status("CREATED", &campaign.id);
            }
            return Ok(0);
        }
        eprintln!("campaign: {}", campaign.id);
        let (mut stopped, mut reported) = (false, 0);
        while campaign.phase != CampaignPhase::Completed {
            if cancellation.is_cancelled() && !stopped {
                campaign = client
                    .stop_campaign(&campaign.id)
                    .await
                    .map_err(client_error)?;
                stopped = true;
                continue;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
            campaign = client.campaign(&campaign.id).await.map_err(client_error)?;
            let done = campaign.assessment.members_completed;
            if done != reported {
                reported = done;
                eprintln!(
                    "campaign: {done} of {} members, {} findings, {} features",
                    campaign.plan.budget,
                    campaign.findings.len(),
                    campaign.features
                );
            }
        }
        report(&campaign, json)
    })
}

fn report(campaign: &Campaign, json: bool) -> Result<u8> {
    let (word, code) = outcome(campaign);
    if json {
        output::json(campaign)?;
        return Ok(code);
    }
    output::status(word, &campaign.id);
    let a = &campaign.assessment;
    output::field(
        "members",
        format!(
            "{} completed, {} failed, {} incomplete",
            a.members_completed, a.members_failed, a.members_incomplete
        ),
    );
    for p in a.properties.iter().filter(|p| p.members_violating > 0) {
        output::field(
            "violated",
            format!(
                "{} in {} of {} members (observed campaign rate)",
                p.id, p.members_violating, a.members_completed
            ),
        );
    }
    for finding in &campaign.findings {
        let label = finding
            .label
            .map(|l| format!(" ({})", words(&l)))
            .unwrap_or_default();
        output::field(
            "finding",
            format!("{}{label}: {}", finding.group, finding.representative),
        );
        match &finding.export {
            seedbox_api::ExportState::Published { path } => output::field("exported", path),
            seedbox_api::ExportState::Failed { message } => output::field("export-failed", message),
            seedbox_api::ExportState::Pending => {}
        }
    }
    for group in &campaign.not_observed {
        output::field("not-observed", group);
    }
    if let Some(CampaignCompletion::Failed { message }) = &campaign.completion {
        output::field("error", message);
    }
    Ok(code)
}

pub(super) fn list(json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        let campaigns = client.campaigns().await.map_err(client_error)?;
        if json {
            output::json(&campaigns)?;
            return Ok(0);
        }
        output::table(
            ["ID", "NAME", "PHASE", "COMPLETED", "BUDGET", "FINDINGS"],
            campaigns
                .iter()
                .map(|c| {
                    [
                        c.id.clone(),
                        c.name.clone(),
                        match &c.completion {
                            Some(done) => words(done),
                            None => words(&c.phase),
                        },
                        c.completed.to_string(),
                        c.budget.to_string(),
                        c.findings.to_string(),
                    ]
                })
                .collect(),
        )?;
        Ok(0)
    })
}

pub(super) fn get(id: &str, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        let campaign = client.campaign(id).await.map_err(client_error)?;
        if json {
            output::json(&campaign)?;
            return Ok(0);
        }
        output::table(
            [
                "ORDINAL",
                "GEN",
                "PLAN",
                "SIMULATION",
                "VERDICT",
                "NOVELTY",
                "CORPUS",
            ],
            campaign
                .members
                .iter()
                .map(|m| {
                    [
                        m.ordinal.to_string(),
                        m.generation.to_string(),
                        match &m.plan {
                            MemberPlan::Root { seed, kinds: None } => format!("root seed={seed}"),
                            MemberPlan::Root {
                                seed,
                                kinds: Some(kinds),
                            } => format!("root seed={seed} kinds={kinds:?}"),
                            MemberPlan::Branch {
                                parent,
                                anchor,
                                choices,
                                mutation,
                                ..
                            } => {
                                let mut plan =
                                    format!("branch {parent}@{anchor} choices={choices}");
                                if let Some(m) = mutation {
                                    plan.push_str(&format!(" {m}"));
                                }
                                plan
                            }
                        },
                        m.simulation
                            .as_ref()
                            .map_or("-".into(), ToString::to_string),
                        m.verdict.map_or_else(
                            || {
                                if m.error.is_some() {
                                    "error".into()
                                } else {
                                    "-".into()
                                }
                            },
                            |v| output::word(&v).to_lowercase(),
                        ),
                        m.novelty.to_string(),
                        if m.in_corpus { "yes" } else { "-" }.into(),
                    ]
                })
                .collect(),
        )?;
        report(&campaign, false)?;
        Ok(0)
    })
}

pub(super) fn findings(id: &str, query: FindingQuery, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        let page = client
            .campaign_encounters(id, &query)
            .await
            .map_err(client_error)?;
        if json {
            output::json(&page)?;
            return Ok(0);
        }
        output::table(
            [
                "GROUP",
                "ORDINAL",
                "SIMULATION",
                "MACHINE",
                "AT",
                "ANCESTRY",
                "FAULTS",
            ],
            page.encounters
                .iter()
                .map(|e| {
                    [
                        e.group.clone(),
                        e.moment.ordinal.to_string(),
                        e.moment.simulation.to_string(),
                        e.moment.machine.clone(),
                        format!("{:.3}s", e.moment.time_ns as f64 / 1e9),
                        e.ancestry
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(">"),
                        format!(
                            "{}{}",
                            e.faults.len(),
                            if e.history_complete {
                                ""
                            } else {
                                " (incomplete history)"
                            }
                        ),
                    ]
                })
                .collect(),
        )?;
        if let Some(next) = page.next_offset {
            eprintln!("more: --offset {next} ({} in total)", page.total);
        }
        if page.incomplete {
            eprintln!("history is incomplete: some members are running or lost evidence");
        }
        Ok(0)
    })
}

pub(super) fn stop(id: &str) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        let campaign = client.stop_campaign(id).await.map_err(client_error)?;
        output::status("STOP_REQUESTED", &campaign.id);
        Ok(0)
    })
}

pub(super) fn shrink(
    id: &SimulationId,
    max_probes: u32,
    property: Option<String>,
    options: seedbox_api::ExecutionOptions,
    detach: bool,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let request = seedbox_api::ShrinkRequest {
        request_id: Client::request_id().map_err(client_error)?,
        max_probes,
        property,
        options,
    };
    runtime()?.block_on(async {
        let mut shrink = client
            .submit_shrink(id, &request)
            .await
            .map_err(client_error)?;
        if detach {
            if json {
                output::json(&shrink)?;
            } else {
                output::status("CREATED", &shrink.id);
            }
            return Ok(0);
        }
        eprintln!(
            "shrink: {} ({} of {} inputs removable)",
            shrink.id, shrink.removable, shrink.inputs
        );
        let mut reported = 0;
        while shrink.phase != CampaignPhase::Completed {
            tokio::time::sleep(Duration::from_millis(500)).await;
            shrink = client.shrink(&shrink.id).await.map_err(client_error)?;
            let done = shrink
                .probes
                .iter()
                .filter(|p| p.reproduced.is_some())
                .count();
            if done != reported {
                reported = done;
                let last = &shrink.probes[done - 1];
                eprintln!(
                    "probe {done}: {} inputs, {}",
                    last.inputs,
                    if last.reproduced == Some(true) {
                        "reproduced"
                    } else {
                        "passed"
                    }
                );
            }
        }
        shrink_report(&shrink, json)
    })
}

fn shrink_report(shrink: &Shrink, json: bool) -> Result<u8> {
    let code = match (&shrink.error, shrink.outcome) {
        (Some(_), _) | (None, None) => 2,
        (None, Some(ShrinkOutcome::NotReproduced)) => 1,
        _ => 0,
    };
    if json {
        output::json(shrink)?;
        return Ok(code);
    }
    match (&shrink.error, shrink.outcome) {
        (Some(error), _) => {
            output::status("ERROR", &shrink.id);
            output::field("error", error);
        }
        (None, Some(outcome)) => {
            let detail = shrink.reproduction.as_ref().unwrap_or(&shrink.source);
            output::status(&screaming(&outcome), detail);
            output::field(
                "result",
                match outcome {
                    ShrinkOutcome::Minimal => "1-minimal under these transformations",
                    ShrinkOutcome::Reduced => "reduced; the probe budget ran out first",
                    ShrinkOutcome::NotReproduced => {
                        "the unchanged inputs did not fail the same way"
                    }
                },
            );
            output::field("target", &shrink.target);
            output::field("probes", shrink.probes.len());
            output::field(
                "kept",
                format!("{} of {} inputs", shrink.kept.len(), shrink.inputs),
            );
            for key in &shrink.kept {
                output::field("input", key);
            }
        }
        (None, None) => output::status("ERROR", &shrink.id),
    }
    Ok(code)
}
