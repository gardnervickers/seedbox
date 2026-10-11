//! Reading a simulation without changing it: retained logs and events, the
//! observed world and fault timeline, and waiting for a condition. Every
//! command reads what has happened and exits unless asked to follow.
use super::{daemon, output};
use futures_util::StreamExt;
use seedbox_api::{
    EventDelivery, FaultStatus, LogDelivery, LogEntry, MachineCondition, Phase, PlaybackRate,
    SimulationId, Stage, WorldState,
};
use seedbox_application::{execution::CancellationToken, Error, Result};
use seedbox_client::Client;
use std::time::{Duration, Instant};

/// Which application output `logs` prints.
pub(super) struct LogFilter {
    pub stream: Option<seedbox_api::LogStream>,
    pub since_ns: Option<u64>,
    pub until_ns: Option<u64>,
    pub incarnation: Option<u32>,
}

impl LogFilter {
    fn admits(&self, entry: &LogEntry) -> bool {
        let (time, incarnation) = match entry {
            LogEntry::Bytes {
                simulated_time_ns,
                incarnation,
                stream,
                ..
            } => {
                if self.stream.is_some_and(|selected| selected != *stream) {
                    return false;
                }
                (*simulated_time_ns, *incarnation)
            }
            LogEntry::MachineGap {
                simulated_time_ns,
                incarnation,
                ..
            } => (*simulated_time_ns, *incarnation),
            LogEntry::SourceGap { .. } | LogEntry::SourceUnavailable => return true,
        };
        self.since_ns.is_none_or(|since| time >= since)
            && self.until_ns.is_none_or(|until| time < until)
            && self.incarnation.is_none_or(|wanted| wanted == incarnation)
    }
}

pub(super) struct LogRequest<'a> {
    pub machine: Option<&'a str>,
    pub filter: LogFilter,
    pub follow: bool,
    pub tail: Option<usize>,
    pub json: bool,
}

async fn log_stream(
    client: &Client,
    id: &SimulationId,
    machine: Option<&str>,
    follow: bool,
) -> Result<seedbox_client::LogStream> {
    match machine {
        Some(machine) => client.logs_with(id, machine, follow).await,
        None => client.all_logs_with(id, follow).await,
    }
    .map_err(super::simulations::client_error)
}

fn sequence(delivery: &LogDelivery) -> Option<u64> {
    match delivery {
        LogDelivery::Entry { sequence, .. } => Some(*sequence),
        LogDelivery::Gap { last, .. } => Some(*last),
        LogDelivery::Closed => None,
    }
}

fn print_log(delivery: &LogDelivery, json: bool) -> Result<()> {
    if json {
        return output::json_line(delivery);
    }
    match delivery {
        LogDelivery::Entry {
            entry: LogEntry::Bytes { bytes, .. },
            ..
        } => output::bytes(bytes)?,
        LogDelivery::Entry {
            entry: LogEntry::MachineGap {
                machine, records, ..
            },
            ..
        } => eprintln!("omission: {records} application output records from {machine} were lost"),
        LogDelivery::Entry {
            entry: LogEntry::SourceGap { first, last },
            ..
        } => eprintln!("omission: worker observations {first}..{last} were lost"),
        LogDelivery::Entry {
            entry: LogEntry::SourceUnavailable,
            ..
        } => eprintln!("omission: application output is unavailable or incomplete"),
        LogDelivery::Gap { first, last } => {
            eprintln!("omission: client log entries {first}..{last} were lost")
        }
        LogDelivery::Closed => {}
    }
    Ok(())
}

/// The last `lines` lines of the retained text, as whole lines.
fn tail_text(deliveries: &[LogDelivery], lines: usize) -> Vec<u8> {
    let mut text: Vec<u8> = Vec::new();
    for delivery in deliveries {
        if let LogDelivery::Entry {
            entry: LogEntry::Bytes { bytes, .. },
            ..
        } = delivery
        {
            text.extend_from_slice(bytes);
        }
    }
    if lines == 0 {
        return Vec::new();
    }
    let body = text.strip_suffix(b"\n").unwrap_or(&text);
    let start = body
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, b)| **b == b'\n')
        .nth(lines - 1)
        .map_or(0, |(i, _)| i + 1);
    text[start..].to_vec()
}

/// Print retained output, then keep streaming with `follow`.
pub(super) fn logs(id: &SimulationId, request: LogRequest) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    super::simulations::runtime()?.block_on(async {
        // History first, so --tail can choose from all of it.
        let mut history = log_stream(&client, id, request.machine, false).await?;
        let mut retained = Vec::new();
        let mut last = 0;
        while let Some(next) = history.next().await {
            let delivery = next.map_err(super::simulations::client_error)?;
            if let Some(sequence) = sequence(&delivery) {
                last = last.max(sequence);
            }
            match &delivery {
                LogDelivery::Closed => break,
                LogDelivery::Entry { entry, .. } if !request.filter.admits(entry) => {}
                _ => retained.push(delivery),
            }
        }
        match (request.tail, request.json) {
            (Some(lines), false) => output::bytes(&tail_text(&retained, lines))?,
            (Some(records), true) => {
                for delivery in &retained[retained.len().saturating_sub(records)..] {
                    print_log(delivery, true)?;
                }
            }
            (None, json) => {
                for delivery in &retained {
                    print_log(delivery, json)?;
                }
            }
        }
        if !request.follow {
            return Ok(0);
        }
        let mut live = log_stream(&client, id, request.machine, true).await?;
        loop {
            tokio::select! {
                next = live.next() => match next {
                    Some(Ok(delivery)) => {
                        // The live stream replays history first; skip what was printed.
                        if sequence(&delivery).is_some_and(|sequence| sequence <= last) {
                            continue;
                        }
                        match &delivery {
                            LogDelivery::Closed => {
                                if request.json {
                                    print_log(&delivery, true)?;
                                }
                                return Ok(0);
                            }
                            LogDelivery::Entry { entry, .. } if !request.filter.admits(entry) => {}
                            _ => print_log(&delivery, request.json)?,
                        }
                    }
                    Some(Err(error)) => return Err(super::simulations::client_error(error)),
                    None => return Ok(0),
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancellation.is_cancelled() { return Ok(0); }
                }
            }
        }
    })
}

/// Print retained events as newline-delimited JSON, then follow with `follow`.
pub(super) fn events(id: &SimulationId, follow: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    super::simulations::runtime()?.block_on(async {
        let mut stream = client
            .events_with(id, follow)
            .await
            .map_err(super::simulations::client_error)?;
        loop {
            tokio::select! {
                next = stream.next() => match next {
                    // Only a followed stream's Closed means nothing more will happen.
                    Some(Ok(EventDelivery::Closed)) if follow => {
                        output::json_line(&EventDelivery::Closed)?;
                        return Ok(0);
                    }
                    Some(Ok(EventDelivery::Closed)) | None => return Ok(0),
                    Some(Ok(delivery)) => output::json_line(&delivery)?,
                    Some(Err(error)) => return Err(super::simulations::client_error(error)),
                },
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    if cancellation.is_cancelled() { return Ok(0); }
                }
            }
        }
    })
}

/// A short description of what a fault action targets.
fn target(action: &serde_json::Value) -> String {
    let Some(fields) = action.as_object() else {
        return String::new();
    };
    fields
        .iter()
        .filter(|(key, _)| key.as_str() != "action")
        .map(|(key, value)| {
            let value = match value {
                serde_json::Value::String(text) => text.clone(),
                serde_json::Value::Array(items) => items
                    .iter()
                    .map(|item| {
                        item.as_str()
                            .map_or_else(|| item.to_string(), str::to_owned)
                    })
                    .collect::<Vec<_>>()
                    .join(","),
                other => other.to_string(),
            };
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn seconds(ns: u64) -> String {
    let text = format!("{:.3}", ns as f64 / 1e9);
    format!("{}s", text.trim_end_matches('0').trim_end_matches('.'))
}

/// Machines, links with their counters and faults as the server observes them now.
pub(super) fn world(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let world = super::simulations::runtime()?
        .block_on(client.world(id))
        .map_err(super::simulations::client_error)?;
    if json {
        output::json(&world)?;
        return Ok(0);
    }
    output::table(
        ["MACHINE", "CONDITION", "INCARNATION", "EXIT", "ORACLE"],
        world
            .machines
            .iter()
            .map(|m| {
                [
                    m.name.clone(),
                    output::word(&m.condition).to_lowercase(),
                    m.incarnation.to_string(),
                    m.exit_code
                        .map_or_else(|| "-".into(), |code| code.to_string()),
                    if m.oracle { "yes".into() } else { "-".into() },
                ]
            })
            .collect(),
    )?;
    if !world.links.is_empty() {
        println!();
        output::table(
            [
                "FROM", "TO", "LATENCY", "LOSS", "FRAMES", "BYTES", "DROPPED", "FAULT",
            ],
            world
                .links
                .iter()
                .map(|l| {
                    let c = &l.counters;
                    [
                        l.from.clone(),
                        l.to.clone(),
                        seconds(l.policy.latency_ns),
                        format!("{}ppm", l.policy.loss_ppm),
                        c.scheduled.to_string(),
                        c.bytes.to_string(),
                        (c.partition_dropped
                            + c.loss_dropped
                            + c.queue_dropped
                            + c.receiver_dropped)
                            .to_string(),
                        match (&l.partitioned_by, &l.shaped_by) {
                            (Some(handle), _) => format!("cut {handle}"),
                            (None, Some(handle)) => format!("shaped {handle}"),
                            (None, None) => "-".into(),
                        },
                    ]
                })
                .collect(),
        )?;
    }
    if !world.faults.is_empty() {
        println!();
        output::table(
            ["FAULT", "ACTION", "TARGET", "STATUS", "APPLIED", "ACTIVE"],
            world
                .faults
                .iter()
                .map(|f| {
                    [
                        f.id.to_string(),
                        f.action.clone(),
                        f.plan
                            .as_ref()
                            .map(target)
                            .or_else(|| f.machine.clone())
                            .unwrap_or_default(),
                        output::word(&f.status).to_lowercase(),
                        f.applied_time_ns.map_or_else(|| "-".into(), seconds),
                        if f.active { "yes".into() } else { "-".into() },
                    ]
                })
                .collect(),
        )?;
    }
    println!();
    output::field("simulated time", seconds(world.simulated_time_ns));
    Ok(0)
}

/// The fault timeline: recorded and scheduled faults with what became of each.
pub(super) fn timeline(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let (timeline, world) = super::simulations::runtime()?.block_on(async {
        let timeline = client.timeline(id).await;
        let world = client.world(id).await.ok();
        (timeline, world)
    });
    let timeline = timeline.map_err(super::simulations::client_error)?;
    if json {
        output::json(&timeline)?;
        return Ok(0);
    }
    let outcome = |entry: &seedbox_api::TimelineEntry| -> String {
        let Some(world) = &world else {
            return "-".into();
        };
        if entry
            .id
            .strip_prefix("program:")
            .is_some_and(|key| world.override_rejections.contains_key(key))
        {
            return "rejected".into();
        }
        entry
            .fault_id
            .and_then(|id| world.faults.iter().find(|f| f.id == id))
            .map_or_else(
                || "upcoming".into(),
                |fault| output::word(&fault.status).to_lowercase(),
            )
    };
    output::table(
        ["AT", "ACTION", "TARGET", "ORIGIN", "OUTCOME", "ENTRY"],
        timeline
            .entries
            .iter()
            .map(|entry| {
                [
                    seconds(entry.at_ns),
                    entry.action["action"].as_str().unwrap_or("-").to_owned(),
                    target(&entry.action),
                    output::word(&entry.origin).to_lowercase(),
                    outcome(entry),
                    entry.id.clone(),
                ]
            })
            .collect(),
    )?;
    Ok(0)
}

/// A condition `wait` can observe.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Condition {
    /// The simulation has completed, for any reason.
    Completed,
    /// Guest execution has started.
    Executing,
    /// Playback is paused at a committed boundary.
    Paused,
    /// Simulated time has reached this many nanoseconds.
    SimTime(u64),
    /// A machine is in this condition.
    Machine(String, MachineCondition),
    /// A fault, by numeric ID or handle, has this status.
    Fault(String, FaultStatus),
}

impl std::fmt::Display for Condition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Condition::Completed => write!(f, "completed"),
            Condition::Executing => write!(f, "executing"),
            Condition::Paused => write!(f, "paused"),
            Condition::SimTime(ns) => write!(f, "sim-time={}", seconds(*ns)),
            Condition::Machine(name, state) => {
                write!(f, "machine/{name}={}", output::word(state).to_lowercase())
            }
            Condition::Fault(key, status) => {
                write!(f, "fault/{key}={}", output::word(status).to_lowercase())
            }
        }
    }
}

/// Parse `--for`: completed, executing, paused, sim-time=DURATION,
/// machine/NAME=CONDITION or fault/ID-OR-HANDLE=STATUS.
pub(super) fn condition(value: &str) -> std::result::Result<Condition, String> {
    let enum_value = |text: &str| serde_json::Value::String(text.to_owned());
    match value {
        "completed" => return Ok(Condition::Completed),
        "executing" => return Ok(Condition::Executing),
        "paused" => return Ok(Condition::Paused),
        _ => {}
    }
    if let Some(time) = value.strip_prefix("sim-time=") {
        return seedbox_application::config::duration_ns(time)
            .map(Condition::SimTime)
            .map_err(|error| format!("sim-time: {error}"));
    }
    if let Some(rest) = value.strip_prefix("machine/") {
        let (name, state) = rest
            .split_once('=')
            .ok_or("expected machine/NAME=CONDITION, e.g. machine/n3=ready")?;
        let state: MachineCondition = serde_json::from_value(enum_value(state)).map_err(|_| {
            format!("unknown machine condition {state:?}; use starting, ready, paused, crashed or exited")
        })?;
        if name.is_empty() {
            return Err("machine name is empty".into());
        }
        return Ok(Condition::Machine(name.to_owned(), state));
    }
    if let Some(rest) = value.strip_prefix("fault/") {
        let (key, status) = rest
            .split_once('=')
            .ok_or("expected fault/ID-OR-HANDLE=STATUS, e.g. fault/p1=applied")?;
        let status: FaultStatus = serde_json::from_value(enum_value(status)).map_err(|_| {
            format!("unknown fault status {status:?}; use unrequested, requested, applied, failed or unavailable")
        })?;
        if key.is_empty() {
            return Err("fault ID or handle is empty".into());
        }
        return Ok(Condition::Fault(key.to_owned(), status));
    }
    Err(format!(
        "unknown condition {value:?}; use completed, executing, paused, sim-time=DURATION, machine/NAME=CONDITION or fault/ID-OR-HANDLE=STATUS"
    ))
}

#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum WaitOutcome {
    Met,
    Timeout,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct WaitReport {
    outcome: WaitOutcome,
    simulation: SimulationId,
    condition: String,
    simulated_time_ns: u64,
}

fn fault_matches(world: &WorldState, key: &str, status: FaultStatus) -> bool {
    let by_id = key.parse::<u32>().ok();
    // A handle names its newest non-heal fault.
    let fault = match by_id {
        Some(id) => world.faults.iter().find(|f| f.id == id),
        None => world
            .faults
            .iter()
            .filter(|f| {
                f.action != "heal"
                    && f.plan
                        .as_ref()
                        .is_some_and(|plan| plan["handle"].as_str() == Some(key))
            })
            .max_by_key(|f| f.id),
    };
    fault.is_some_and(|f| f.status == status)
}

/// Poll until `condition` holds, the simulation can no longer meet it, or
/// `timeout` (host time) passes.
pub(super) fn wait(
    id: &SimulationId,
    condition: &Condition,
    timeout_ns: Option<u64>,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    let deadline = timeout_ns.map(|ns| Instant::now() + Duration::from_nanos(ns));
    let report = |outcome, simulated_time_ns| WaitReport {
        outcome,
        simulation: id.clone(),
        condition: condition.to_string(),
        simulated_time_ns,
    };
    let (outcome, time) = super::simulations::runtime()?.block_on(async {
        loop {
            let resource = client
                .describe(id)
                .await
                .map_err(super::simulations::client_error)?;
            let status = &resource.status;
            let completed = status.phase == Phase::Completed;
            let time = status.simulated_time_ns;
            let met = match condition {
                Condition::Completed => completed,
                Condition::Executing => {
                    status.phase == Phase::Running && status.stage == Some(Stage::Executing)
                }
                Condition::Paused => status
                    .playback
                    .as_ref()
                    .is_some_and(|p| matches!(p.state.rate, PlaybackRate::Paused)),
                Condition::SimTime(ns) => time >= *ns,
                Condition::Machine(name, state) => client.world(id).await.ok().is_some_and(|w| {
                    w.machines
                        .iter()
                        .any(|m| &m.name == name && m.condition == *state)
                }),
                Condition::Fault(key, wanted) => client
                    .world(id)
                    .await
                    .ok()
                    .is_some_and(|w| fault_matches(&w, key, *wanted)),
            };
            if met {
                return Ok((WaitOutcome::Met, time));
            }
            if completed {
                return Err(Error::Setup(format!(
                    "{id} completed before {condition}; nothing more will change"
                )));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok((WaitOutcome::Timeout, time));
            }
            if cancellation.is_cancelled() {
                return Err(Error::Setup(format!("{id}: wait cancelled")));
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })?;
    let report = report(outcome, time);
    if json {
        output::json(&report)?;
    } else {
        output::status(&output::word(&report.outcome), id);
        output::field("condition", &report.condition);
        output::field("simulated time", seconds(report.simulated_time_ns));
    }
    Ok(match report.outcome {
        WaitOutcome::Met => 0,
        WaitOutcome::Timeout => 124,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditions_parse_and_print_round_trip() {
        for text in [
            "completed",
            "executing",
            "paused",
            "sim-time=1.5s",
            "machine/n3=ready",
            "fault/p1=applied",
            "fault/7=failed",
        ] {
            assert_eq!(condition(text).unwrap().to_string(), text);
        }
        assert_eq!(
            condition("sim-time=250ms").unwrap(),
            Condition::SimTime(250_000_000)
        );
        for bad in [
            "ready",
            "machine/n3",
            "machine/=ready",
            "machine/n3=up",
            "fault/p1=done",
            "sim-time=soon",
        ] {
            assert!(condition(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn tail_keeps_whole_last_lines_across_chunks() {
        let chunk = |sequence, bytes: &[u8]| LogDelivery::Entry {
            sequence,
            entry: LogEntry::Bytes {
                machine: "a".into(),
                incarnation: 0,
                simulated_time_ns: sequence,
                stream: seedbox_api::LogStream::Stdout,
                bytes: bytes.to_vec(),
            },
        };
        let deliveries = [
            chunk(1, b"one\ntw"),
            chunk(2, b"o\nthree\n"),
            chunk(3, b"four\n"),
        ];
        assert_eq!(tail_text(&deliveries, 2), b"three\nfour\n");
        assert_eq!(tail_text(&deliveries, 10), b"one\ntwo\nthree\nfour\n");
        assert_eq!(tail_text(&deliveries, 0), b"");
        let unterminated = [chunk(1, b"a\nb")];
        assert_eq!(tail_text(&unterminated, 1), b"b");
    }

    #[test]
    fn log_filter_selects_time_window_incarnation_and_stream() {
        let entry = |time, incarnation, stream| LogEntry::Bytes {
            machine: "a".into(),
            incarnation,
            simulated_time_ns: time,
            stream,
            bytes: vec![],
        };
        let filter = LogFilter {
            stream: Some(seedbox_api::LogStream::Stderr),
            since_ns: Some(10),
            until_ns: Some(20),
            incarnation: Some(1),
        };
        assert!(filter.admits(&entry(10, 1, seedbox_api::LogStream::Stderr)));
        assert!(!filter.admits(&entry(20, 1, seedbox_api::LogStream::Stderr)));
        assert!(!filter.admits(&entry(15, 0, seedbox_api::LogStream::Stderr)));
        assert!(!filter.admits(&entry(15, 1, seedbox_api::LogStream::Stdout)));
        assert!(filter.admits(&LogEntry::SourceUnavailable));
    }
}
