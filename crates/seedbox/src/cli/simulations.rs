//! Simulation presentation and interaction through the public client API.

use super::{daemon, defaults, output, ComposeArg, MAC};
use futures_util::StreamExt;
use seedbox_api::{
    ExecutionTermination, InputSource, Phase, Simulation, SimulationDocument, SimulationId,
    SimulationSpec, SubmitRequest,
};
use seedbox_application::{execution::CancellationToken, Error, Result};
use seedbox_client::Client;
use std::{io::Read, time::Duration};

pub(super) fn capacity(json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        let capacity = client.capacity().await.map_err(client_error)?;
        if json {
            output::json(&capacity)?;
        } else {
            output::field("slots", capacity.slots);
            output::field("occupied", capacity.occupying.len());
            output::field("paused", capacity.paused);
            let occupants = capacity
                .occupying
                .iter()
                .map(|o| {
                    [
                        o.simulation.to_string(),
                        o.phase
                            .as_ref()
                            .map(output::word)
                            .unwrap_or_else(|| "cleanup".into()),
                        o.stage
                            .as_ref()
                            .map(output::word)
                            .unwrap_or_else(|| "-".into()),
                        o.paused.to_string(),
                        o.guest_memory_bytes
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| "-".into()),
                    ]
                })
                .collect();
            output::table(
                [
                    "SIMULATION",
                    "PHASE",
                    "STAGE",
                    "PAUSED",
                    "GUEST RAM (BYTES)",
                ],
                occupants,
            )?;
            let queued = capacity
                .queued
                .iter()
                .map(|q| [q.simulation.to_string(), queue_reason(q.reason).into()])
                .collect();
            output::table(["QUEUED", "REASON"], queued)?;
        }
        Ok(0)
    })
}

fn queue_reason(reason: seedbox_api::QueueReason) -> &'static str {
    match reason {
        seedbox_api::QueueReason::ResidentCapacity => "resident slots occupied; stop or delete an occupant to free capacity (pause retains RAM)",
        seedbox_api::QueueReason::AwaitingDispatch => "awaiting dispatch to an available slot",
    }
}

pub(super) fn tui(id: Option<SimulationId>) -> Result<u8> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(Error::Setup(
            "seedbox tui needs an interactive terminal; use seedbox events or seedbox get from scripts".into(),
        ));
    }
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        seedbox_tui::run(client, id, daemon::state_directory(None)?.join("timelines"))
            .await
            .map_err(|e| Error::Setup(format!("terminal: {e}")))
    })?;
    Ok(0)
}

fn load_spec(
    compose: &ComposeArg,
    seed: Option<u64>,
) -> Result<(SimulationSpec, seedbox_spec::definition::Metadata)> {
    let path = defaults::compose_file(&compose.compose)?;
    let path = path
        .canonicalize()
        .map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?;
    let file =
        std::fs::File::open(&path).map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?;
    let mut source = String::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut source)
        .map_err(|e| Error::Setup(format!("{}: {e}", path.display())))?;
    if source.len() > 16 * 1024 * 1024 {
        return Err(Error::Setup("simulation document exceeds 16 MiB".into()));
    }
    let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(&source)?;
    let mut spec = if document.get("apiVersion").is_some() {
        serde_yaml_ng::from_str::<SimulationDocument>(&source)?.spec
    } else {
        let architecture = if MAC {
            seedbox_application::guest::Architecture::Arm64
        } else {
            seedbox_application::guest::Architecture::Amd64
        };
        SimulationSpec {
            program: Vec::new(),
            start_paused: false,
            seed: None,
            inputs: InputSource::Project,
            configuration: seedbox_application::config::parse_for_architecture(
                &source,
                &path,
                &std::env::vars().collect(),
                architecture,
            )?,
        }
    };
    if document.get("apiVersion").is_none()
        && (document.get("machines").is_some() || document.get("oracle").is_some())
    {
        for service in spec.configuration.services.values_mut() {
            service.platform.clear();
        }
    }
    if seed.is_some() {
        spec.seed = seed;
    }
    let metadata = seedbox_spec::definition::Metadata {
        name: document
            .get("metadata")
            .and_then(|m| m.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or(&spec.configuration.name)
            .into(),
        labels: document
            .get("metadata")
            .and_then(|m| m.get("labels"))
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()?
            .unwrap_or_default(),
        annotations: document
            .get("metadata")
            .and_then(|m| m.get("annotations"))
            .cloned()
            .map(serde_yaml_ng::from_value)
            .transpose()?
            .unwrap_or_default(),
    };
    Ok((spec, metadata))
}

pub(super) fn run(
    compose: &ComposeArg,
    seed: Option<u64>,
    paused: bool,
    detach: bool,
    json: bool,
    wall_time_ns: Option<u64>,
) -> Result<u8> {
    let (spec, metadata) = load_spec(compose, seed)?;
    submit(
        spec,
        seedbox_api::ExecutionOptions {
            start_paused: paused,
            wall_time_ns,
        },
        metadata,
        detach,
        json,
    )
}

pub(super) fn explore(
    compose: &ComposeArg,
    seed: Option<u64>,
    count: u32,
    paused: bool,
    detach: bool,
    json: bool,
    wall_time_ns: Option<u64>,
) -> Result<u8> {
    let (spec, metadata) = load_spec(compose, seed)?;
    if spec
        .seed
        .or(spec.configuration.seed)
        .is_some_and(|seed| seed.checked_add(u64::from(count - 1)).is_none())
    {
        return Err(Error::Setup("seed range overflows u64".into()));
    }
    let request = seedbox_api::SeedRangeRequest {
        options: seedbox_api::ExecutionOptions {
            start_paused: paused,
            wall_time_ns,
        },
        metadata,
        request_id: Client::request_id().map_err(client_error)?,
        spec,
        count,
    };
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let mut range = client.submit_seeds(&request).await.map_err(client_error)?;
        if detach {
            if json {
                output::json(&range)?;
            } else {
                output::status("CREATED", &range.source);
            }
            return Ok(0);
        }
        eprintln!("simulations: {}", range.source);
        let mut stopped = false;
        while !range.complete() {
            if cancellation.is_cancelled() && !stopped {
                range = client
                    .stop_seeds(&range.source)
                    .await
                    .map_err(client_error)?;
                stopped = true;
            } else {
                tokio::time::sleep(Duration::from_millis(200)).await;
                range = client
                    .seed_range(&range.source)
                    .await
                    .map_err(client_error)?;
            }
        }
        let codes: Vec<_> = range
            .members
            .iter()
            .map(|member| status_outcome(&member.status).1)
            .collect();
        let (word, code) = if range.error.is_some() || codes.contains(&2) {
            ("ERROR", 2)
        } else if codes.contains(&124) {
            ("TIMEOUT", 124)
        } else if range.members.len() != count as usize || codes.contains(&3) {
            ("INCOMPLETE", 3)
        } else if codes.contains(&1) {
            ("FAIL", 1)
        } else {
            ("PASS", 0)
        };
        if json {
            output::json(&range)?;
        } else {
            output::status(word, &range.source);
        }
        Ok(code)
    })
}

pub(super) fn seed_range(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let mut range = runtime()?
        .block_on(client.seed_range(id))
        .map_err(client_error)?;
    if json {
        output::json(&range)?;
    } else {
        let completed = range
            .members
            .iter()
            .filter(|member| member.status.phase == Phase::Completed)
            .count();
        let state = if range.error.is_some() {
            "error"
        } else if range.complete() {
            "completed"
        } else if range.stop_requested {
            "stopping"
        } else {
            "running"
        };
        output::table(
            [
                "SOURCE",
                "FIRST-SEED",
                "COUNT",
                "COMPLETED",
                "DELETED",
                "STATE",
            ],
            vec![[
                range.source.to_string(),
                range.first_seed.to_string(),
                range.count.to_string(),
                completed.to_string(),
                range.deleted_members.to_string(),
                state.into(),
            ]],
        )?;
        range.members.sort_by_key(|item| {
            (
                std::cmp::Reverse(item.metadata.created_at_unix_ms),
                item.metadata.id.clone(),
            )
        });
        simulation_table(range.members.iter().map(summary_row).collect())?;
        if let Some(error) = &range.error {
            eprintln!("seed range {}: {error}", range.source);
        }
    }
    Ok(0)
}

pub(super) fn stop_seeds(id: &SimulationId) -> Result<u8> {
    let client = daemon::connect(None)?;
    let range = runtime()?
        .block_on(client.stop_seeds(id))
        .map_err(client_error)?;
    output::status("STOP_REQUESTED", &range.source);
    Ok(0)
}

pub(super) fn compare(
    baseline: &SimulationId,
    simulation: &SimulationId,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let comparison = runtime()?
        .block_on(client.compare(baseline, simulation))
        .map_err(client_error)?;
    report_comparison(&comparison, json)
}

fn report_comparison(comparison: &seedbox_api::SimulationComparison, json: bool) -> Result<u8> {
    if json {
        output::json(&comparison)?;
    } else {
        output::status(&output::word(&comparison.status), &comparison.simulation);
    }
    Ok(match comparison.status {
        seedbox_api::ComparisonStatus::Match => 0,
        seedbox_api::ComparisonStatus::Mismatch => 1,
        seedbox_api::ComparisonStatus::NotComparable => 2,
    })
}

pub(super) fn replay(id: &SimulationId, json: bool, wall_time_ns: Option<u64>) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    if !matches!(resource.spec.inputs, InputSource::Resolved { .. }) {
        return Err(Error::Setup(format!(
            "simulation {id} has no prepared inputs; run its project again"
        )));
    }
    let metadata = resource_metadata(&resource);
    submit_with_comparison(
        resource.spec,
        seedbox_api::ExecutionOptions {
            start_paused: false,
            wall_time_ns,
        },
        metadata,
        false,
        json,
        Some(id),
    )
}

pub(super) fn rerun(
    id: &SimulationId,
    detach: bool,
    json: bool,
    wall_time_ns: Option<u64>,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    if !matches!(resource.spec.inputs, InputSource::Resolved { .. }) {
        return Err(Error::Setup(format!(
            "simulation {id} has no prepared inputs; run its project again"
        )));
    }
    let metadata = resource_metadata(&resource);
    submit(
        resource.spec,
        seedbox_api::ExecutionOptions {
            start_paused: false,
            wall_time_ns,
        },
        metadata,
        detach,
        json,
    )
}

pub(super) fn branch(
    id: &SimulationId,
    checkpoint: String,
    detach: bool,
    json: bool,
    wall_time_ns: Option<u64>,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.checkpoint_definition(id, &checkpoint))
        .map_err(client_error)?;
    let metadata = resource_metadata(&resource);
    submit(
        resource.spec,
        seedbox_api::ExecutionOptions {
            start_paused: false,
            wall_time_ns,
        },
        metadata,
        detach,
        json,
    )
}
fn resource_metadata(resource: &Simulation) -> seedbox_spec::definition::Metadata {
    let mut annotations = resource.metadata.annotations.clone();
    annotations.insert("derived-from".into(), resource.metadata.id.to_string());
    seedbox_spec::definition::Metadata {
        name: resource.metadata.name.clone(),
        labels: resource.metadata.labels.clone(),
        annotations,
    }
}
fn submit(
    spec: SimulationSpec,
    options: seedbox_api::ExecutionOptions,
    metadata: seedbox_spec::definition::Metadata,
    detach: bool,
    json: bool,
) -> Result<u8> {
    submit_with_comparison(spec, options, metadata, detach, json, None)
}

fn submit_with_comparison(
    spec: SimulationSpec,
    options: seedbox_api::ExecutionOptions,
    metadata: seedbox_spec::definition::Metadata,
    detach: bool,
    json: bool,
    baseline: Option<&SimulationId>,
) -> Result<u8> {
    let request = SubmitRequest {
        options,
        metadata,
        request_id: Client::request_id().map_err(client_error)?,
        spec,
    };
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let resource = client.submit(&request).await.map_err(client_error)?;
        if detach {
            if json { output::json(&resource)?; } else { output::status("CREATED", &resource.metadata.id); }
            return Ok(0);
        }
        eprintln!("simulation: {}", resource.metadata.id);
        let mut watch = client.watch(&resource.metadata.id).await.map_err(client_error)?;
        let mut stop_sent = false;
        loop {
            tokio::select! {
                next = watch.next() => match next {
                    Some(Ok(resource)) if resource.status.phase == Phase::Completed => {
                        if let Some(baseline) = baseline {
                            let comparison = client.compare(baseline, &resource.metadata.id).await.map_err(client_error)?;
                            return report_comparison(&comparison, json);
                        }
                        return report(&resource, json);
                    },
                    Some(Ok(_)) => {},
                    Some(Err(error)) => return Err(client_error(error)),
                    None => return Err(Error::Setup("simulation watch ended without final status; use seedbox describe simulation to reconnect".into())),
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancellation.is_cancelled() && !stop_sent {
                        client.stop(&resource.metadata.id).await.map_err(client_error)?;
                        stop_sent = true;
                    }
                }
            }
        }
    })
}

pub(super) fn watch(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        let mut stream = client.watch(id).await.map_err(client_error)?;
        let mut table = None;
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(resource)) => {
                        if json {
                            output::json_line(&resource)?;
                        } else {
                            if table.is_none() {
                                table = Some(output::Table::new(SIMULATION_HEADERS, [8, 10, 10, 9, 10, resource.spec.configuration.check.service.chars().count().max(24), 10, 8])?);
                            }
                            if let Some(table) = &table {
                                table.row(simulation_row(&resource))?;
                            }
                        }
                        if resource.status.phase == Phase::Completed { return Ok(0); }
                    },
                    Some(Err(error)) => return Err(client_error(error)),
                    None => return Ok(0),
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancellation.is_cancelled() { return Ok(0); }
                }
            }
        }
    })
}

pub(super) fn list(json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let mut items = vec![];
        let mut after = None;
        loop {
            let page = client.list(after.as_ref()).await.map_err(client_error)?;
            items.extend(page.items);
            after = page.next_cursor;
            if after.is_none() {
                break;
            }
        }
        if json {
            output::json(&items)?;
        } else {
            items.sort_by_key(|item| {
                (
                    std::cmp::Reverse(item.metadata.created_at_unix_ms),
                    item.metadata.id.clone(),
                )
            });
            simulation_table(items.iter().map(summary_row).collect())?;
        }
        Ok(0)
    })
}

pub(super) fn delete(id: &SimulationId) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?
        .block_on(client.delete(id))
        .map_err(client_error)?;
    output::status("DELETED", id);
    Ok(0)
}

pub(super) fn delete_checkpoint(id: &SimulationId, name: &str) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?
        .block_on(client.delete_checkpoint(id, name))
        .map_err(client_error)?;
    output::status("DELETED", format!("{id}/{name}"));
    Ok(0)
}

pub(super) fn checkpoints(id: &SimulationId, name: Option<&str>, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        if let Some(name) = name {
            let receipt = client.checkpoint(id, name).await.map_err(client_error)?;
            if json {
                output::json(&receipt)?;
            } else {
                output::yaml(&receipt)?;
            }
        } else {
            let mut receipts = client.checkpoints(id).await.map_err(client_error)?;
            if json {
                output::json(&receipts)?;
            } else {
                receipts.sort_by_key(|receipt| {
                    let time = match &receipt.state {
                        seedbox_api::ControlState::Acknowledged {
                            checkpoint: Some(checkpoint),
                            ..
                        } => Some(checkpoint.position.simulated_time_ns),
                        _ => None,
                    };
                    (std::cmp::Reverse(time), receipt.request.request_id.clone())
                });
                output::table(
                    ["NAME", "STATE", "SIM-TIME", "BOUNDARY"],
                    receipts.iter().map(checkpoint_row).collect(),
                )?;
            }
        }
        Ok(0)
    })
}

pub(super) fn get(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    if json {
        output::json(&resource)?;
    } else {
        simulation_table(vec![simulation_row(&resource)])?;
        if resource.status.phase == Phase::Queued {
            let capacity = runtime()?
                .block_on(client.capacity())
                .map_err(client_error)?;
            if let Some(queued) = capacity.queued.iter().find(|q| q.simulation == *id) {
                output::field("queue", queue_reason(queued.reason));
                output::field(
                    "capacity",
                    format!(
                        "{}/{} occupied, {} paused; seedbox get capacity lists occupants",
                        capacity.occupying.len(),
                        capacity.slots,
                        capacity.paused
                    ),
                );
            }
        }
    }
    Ok(0)
}

const SIMULATION_HEADERS: [&str; 8] = [
    "ID", "PHASE", "STAGE", "PLAYBACK", "VERDICT", "ORACLE", "SIM-TIME", "AGE",
];

fn simulation_table(rows: Vec<[String; 8]>) -> Result<()> {
    output::table(SIMULATION_HEADERS, rows)
}

fn simulation_row(resource: &Simulation) -> [String; 8] {
    resource_row(
        &resource.metadata,
        &resource.status,
        &resource.spec.configuration.check.service,
    )
}

fn summary_row(resource: &seedbox_api::SimulationSummary) -> [String; 8] {
    resource_row(&resource.metadata, &resource.status, &resource.oracle)
}

fn resource_row(
    metadata: &seedbox_api::Metadata,
    status: &seedbox_api::SimulationStatus,
    oracle: &str,
) -> [String; 8] {
    let age_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|now| {
            now.as_millis()
                .saturating_sub(u128::from(metadata.created_at_unix_ms))
        })
        .unwrap_or(0);
    [
        metadata.id.to_string(),
        output::word(&status.phase),
        status
            .stage
            .as_ref()
            .map(output::word)
            .unwrap_or_else(|| "-".into()),
        playback_cell(status),
        output::word(&status.verdict()),
        oracle.into(),
        format!("{:.3}s", status.simulated_time_ns as f64 / 1_000_000_000.0),
        age(age_ms / 1000),
    ]
}

/// Requested playback while running: paused, stepping, unlimited or a rate.
fn playback_cell(status: &seedbox_api::SimulationStatus) -> String {
    use seedbox_api::PlaybackRate;
    match &status.playback {
        Some(p) if status.phase == Phase::Running && !p.state.terminal => {
            if p.state.stepping {
                return "stepping".into();
            }
            match p.state.rate {
                PlaybackRate::Paused => "paused".into(),
                PlaybackRate::Unlimited => "unlimited".into(),
                PlaybackRate::Limited {
                    numerator,
                    denominator,
                } if denominator.get() == 1 => format!("{numerator}x"),
                PlaybackRate::Limited {
                    numerator,
                    denominator,
                } => format!("{numerator}/{denominator}x"),
            }
        }
        _ => "-".into(),
    }
}

fn age(seconds: u128) -> String {
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86400 => format!("{}h", seconds / 3600),
        _ => format!("{}d", seconds / 86400),
    }
}

fn checkpoint_row(receipt: &seedbox_api::ControlReceipt) -> [String; 4] {
    use seedbox_api::ControlState;
    let (state, position) = match &receipt.state {
        ControlState::Accepted => ("accepted".into(), None),
        ControlState::Acknowledged {
            error: Some(error), ..
        } => (format!("error/{error:?}"), None),
        ControlState::Acknowledged {
            checkpoint: Some(checkpoint),
            ..
        } => (output::word(&checkpoint.state), Some(&checkpoint.position)),
        ControlState::Acknowledged { .. } => ("acknowledged".into(), None),
        ControlState::Unsupported => ("unsupported".into(), None),
        ControlState::Interrupted => ("interrupted".into(), None),
    };
    [
        receipt.request.request_id.clone(),
        state,
        position
            .map(|p| format!("{:.3}s", p.simulated_time_ns as f64 / 1_000_000_000.0))
            .unwrap_or_else(|| "-".into()),
        position
            .map(|p| p.boundary_index.to_string())
            .unwrap_or_else(|| "-".into()),
    ]
}

pub(super) fn describe(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let resource = client.describe(id).await.map_err(client_error)?;
        if json {
            output::json(&resource)?;
        } else {
            output::yaml(&resource)?;
        }
        Ok(0)
    })
}

pub(super) fn stop(id: &SimulationId) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let resource = client.stop(id).await.map_err(client_error)?;
        output::status(
            if resource.status.phase == Phase::Completed {
                "COMPLETED"
            } else {
                "STOP_REQUESTED"
            },
            id,
        );
        Ok(0)
    })
}

pub(super) fn playback(id: &SimulationId, request_id: Option<&str>, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        if let Some(request_id) = request_id {
            let receipt = client
                .control_receipt(id, request_id)
                .await
                .map_err(client_error)?;
            if json {
                output::json(&receipt)?;
            } else {
                output::yaml(&receipt)?;
            }
            return Ok(0);
        }
        let status = client.playback(id).await.map_err(client_error)?;
        if json {
            output::json(&status)?;
        } else {
            output::yaml(&status)?;
        }
        Ok(0)
    })
}

pub(super) fn mutate_control(
    mutation: super::playback_args::Mutation,
    command: seedbox_api::ControlCommand,
) -> Result<u8> {
    use seedbox_api::{ControlRequest, ControlState};
    let client = daemon::connect(None)?;
    let request_id = match mutation.request_id {
        Some(id) => id,
        None => Client::request_id().map_err(client_error)?,
    };
    // Retain this ID even if the response is lost or the client is interrupted.
    eprintln!("request: {request_id}");
    runtime()?.block_on(async {
        client.info().await.map_err(client_error)?;
        let request = ControlRequest {
            request_id,
            command,
        };
        let mut receipt = client
            .request_control(&mutation.id, &request)
            .await
            .map_err(client_error)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while receipt.state == ControlState::Accepted && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
            receipt = client
                .control_receipt(&mutation.id, &request.request_id)
                .await
                .map_err(client_error)?;
        }
        if mutation.json {
            output::json(&receipt)?;
        } else {
            output::yaml(&receipt)?;
        }
        Ok(match receipt.state {
            ControlState::Accepted | ControlState::Acknowledged { error: None, .. } => 0,
            _ => 2,
        })
    })
}

fn status_outcome(status: &seedbox_api::SimulationStatus) -> (&'static str, u8) {
    match status.oracle_exit_code {
        Some(0) => ("PASS", 0),
        Some(_) => ("FAIL", 1),
        None => match status.termination {
            Some(ExecutionTermination::Error { .. }) => ("ERROR", 2),
            Some(
                ExecutionTermination::SimulatedTimeLimit
                | ExecutionTermination::WallTimeLimit
                | ExecutionTermination::PreparationTimeLimit,
            ) => ("TIMEOUT", 124),
            _ => ("INCOMPLETE", 3),
        },
    }
}

fn report(resource: &Simulation, json: bool) -> Result<u8> {
    let (word, code) = status_outcome(&resource.status);
    if json {
        output::json(resource)?;
    } else {
        output::status(word, &resource.metadata.id);
    }
    if let Some(ExecutionTermination::Error { message }) = &resource.status.termination {
        eprintln!("execution: {message}");
    }
    Ok(code)
}

pub(super) fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Setup(format!("client runtime: {e}")))
}

pub(super) fn client_error(error: seedbox_client::Error) -> Error {
    Error::Setup(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_acceptance_projects_round_trip_through_submission_json() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../harness/acceptance/workloads");
        let mut tested = 0;
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|v| v.to_str()) != Some("yaml") {
                continue;
            }
            let (spec, _) = load_spec(
                &ComposeArg {
                    compose: path.clone(),
                },
                None,
            )
            .unwrap();
            let bytes =
                serde_json::to_vec(&spec).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let restored: SimulationSpec = serde_json::from_slice(&bytes).unwrap();
            let project: seedbox_spec::project::Project =
                serde_yaml_ng::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            assert_eq!(
                project.machines.len() + 1,
                spec.configuration.services.len()
            );
            assert!(matches!(restored.inputs, InputSource::Project));
            assert_eq!(
                serde_json::to_value(&restored).unwrap(),
                serde_json::to_value(&spec).unwrap(),
                "{}",
                path.display()
            );
            tested += 1;
        }
        assert!(tested >= 7);
    }
}
