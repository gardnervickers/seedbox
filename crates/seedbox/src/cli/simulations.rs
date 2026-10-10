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

pub(super) fn logs(
    id: &SimulationId,
    machine: Option<&str>,
    selected: Option<super::ApplicationLogStream>,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        let mut stream = match machine {
            Some(machine) => client.logs(id, machine).await,
            None => client.all_logs(id).await,
        }.map_err(client_error)?;
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(delivery)) => {
                        use seedbox_api::{LogDelivery, LogEntry};
                        if let LogDelivery::Entry { entry: LogEntry::Bytes { stream, .. }, .. } = &delivery {
                            if selected.is_some_and(|s| seedbox_api::LogStream::from(s) != *stream) { continue; }
                        }
                        if json { output::json_line(&delivery)?; }
                        else {
                            match delivery {
                                LogDelivery::Entry { entry: LogEntry::Bytes { bytes, .. }, .. } => output::bytes(&bytes)?,
                                LogDelivery::Entry { entry: LogEntry::MachineGap { machine, records, .. }, .. } => eprintln!("omission: {records} application output records from {machine} were lost"),
                                LogDelivery::Entry { entry: LogEntry::SourceGap { first, last }, .. } => eprintln!("omission: worker observations {first}..{last} were lost"),
                                LogDelivery::Entry { entry: LogEntry::SourceUnavailable, .. } => eprintln!("omission: application output is unavailable or incomplete"),
                                LogDelivery::Gap { first, last } => eprintln!("omission: client log entries {first}..{last} were lost"),
                                LogDelivery::Closed => {},
                            }
                        }
                    }
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
        seedbox_tui::run(client, id)
            .await
            .map_err(|e| Error::Setup(format!("terminal: {e}")))
    })?;
    Ok(0)
}

pub(super) fn events(id: &SimulationId) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    runtime()?.block_on(async {
        let mut stream = client.events(id).await.map_err(client_error)?;
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(delivery)) => output::json_line(&delivery)?,
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

fn load_spec(compose: &ComposeArg, seed: Option<u64>) -> Result<SimulationSpec> {
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
    if seed.is_some() {
        spec.seed = seed;
    }
    Ok(spec)
}

pub(super) fn run(compose: &ComposeArg, seed: Option<u64>, detach: bool, json: bool) -> Result<u8> {
    submit(load_spec(compose, seed)?, detach, json)
}

pub(super) fn explore(
    compose: &ComposeArg,
    seed: Option<u64>,
    count: u32,
    detach: bool,
    json: bool,
) -> Result<u8> {
    let spec = load_spec(compose, seed)?;
    if spec
        .seed
        .or(spec.configuration.seed)
        .is_some_and(|seed| seed.checked_add(u64::from(count - 1)).is_none())
    {
        return Err(Error::Setup("seed range overflows u64".into()));
    }
    let request = seedbox_api::SeedRangeRequest {
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
    let range = runtime()?
        .block_on(client.seed_range(id))
        .map_err(client_error)?;
    if json {
        output::json(&range)?;
    } else {
        output::yaml(&range)?;
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

pub(super) fn replay(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    if !matches!(resource.spec.inputs, InputSource::Retained { .. }) {
        return Err(Error::Setup(format!(
            "simulation {id} has no prepared inputs; run its project again"
        )));
    }
    submit_with_comparison(resource.spec, false, json, Some(id))
}

pub(super) fn rerun(id: &SimulationId, detach: bool, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    if !matches!(resource.spec.inputs, InputSource::Retained { .. }) {
        return Err(Error::Setup(format!(
            "simulation {id} has no prepared inputs; run its project again"
        )));
    }
    submit(resource.spec, detach, json)
}

pub(super) fn branch(
    id: &SimulationId,
    checkpoint: String,
    detach: bool,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let mut resource = runtime()?
        .block_on(client.describe(id))
        .map_err(client_error)?;
    resource.spec.inputs = InputSource::Checkpoint {
        simulation: id.clone(),
        checkpoint,
    };
    submit(resource.spec, detach, json)
}

fn submit(spec: SimulationSpec, detach: bool, json: bool) -> Result<u8> {
    submit_with_comparison(spec, detach, json, None)
}

fn submit_with_comparison(
    spec: SimulationSpec,
    detach: bool,
    json: bool,
    baseline: Option<&SimulationId>,
) -> Result<u8> {
    let request = SubmitRequest {
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
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    Some(Ok(resource)) => {
                        if json { output::json_line(&resource)?; } else { output::yaml_document(&resource)?; }
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
            output::yaml(&items)?;
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
        let value = match name {
            Some(name) => {
                serde_json::to_value(client.checkpoint(id, name).await.map_err(client_error)?)?
            }
            None => serde_json::to_value(client.checkpoints(id).await.map_err(client_error)?)?,
        };
        if json {
            output::json(&value)?;
        } else {
            output::yaml(&value)?;
        }
        Ok(0)
    })
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

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Setup(format!("client runtime: {e}")))
}

fn client_error(error: seedbox_client::Error) -> Error {
    Error::Setup(error.to_string())
}
