//! Reading a simulation without changing it: retained logs and events, the
//! observed world and fault timeline, and waiting for a condition. Every
//! command reads what has happened and exits unless asked to follow.
use super::{daemon, output, output::seconds};
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
    if action["action"] == "trigger" {
        return trigger_rule(action);
    }
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

/// A trigger rule in one line: `event name=commit nth=2 -> crash machine=b`.
fn trigger_rule(rule: &serde_json::Value) -> String {
    let mut when = rule["when"].clone();
    let on = when
        .as_object_mut()
        .and_then(|fields| fields.remove("on"))
        .and_then(|on| on.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mut parts = vec![on];
    parts.extend(Some(target(&when)).filter(|t| !t.is_empty()));
    if let Some(nth) = rule["nth"].as_u64() {
        parts.push(format!("nth={nth}"));
    }
    if let Some(delay) = rule["delay_ns"].as_u64() {
        parts.push(format!("delay={}", seconds(delay)));
    }
    if let Some(within) = rule["within_ns"].as_u64() {
        parts.push(format!("within={}", seconds(within)));
    }
    let then = &rule["then"];
    parts.push(format!(
        "-> {} {}",
        then["action"].as_str().unwrap_or("-"),
        target(then)
    ));
    parts.join(" ").trim_end().to_owned()
}

/// A trigger's receipt, with a fired trigger's effect as its fault reports it.
fn trigger_outcome(trigger: &seedbox_api::TriggerState, world: &seedbox_api::WorldState) -> String {
    use seedbox_api::{FaultStatus, TriggerStatus};
    match trigger.status {
        TriggerStatus::Armed if trigger.seen > 0 => format!("armed (seen {})", trigger.seen),
        TriggerStatus::Fired => {
            let fault = trigger
                .fault_id
                .and_then(|id| world.faults.iter().find(|f| f.id == id));
            match fault {
                Some(f) if f.status == FaultStatus::Applied => {
                    with_counts("applied".into(), f.counts.as_ref())
                }
                Some(f) if f.status == FaultStatus::Failed => "ineffective".into(),
                _ => "fired".into(),
            }
        }
        TriggerStatus::CutOff => "cut off".into(),
        status => output::word(&status).to_lowercase(),
    }
}

/// Guest property results reported beside the oracle verdict.
pub(super) fn properties(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let resource = super::simulations::runtime()?
        .block_on(client.describe(id))
        .map_err(super::simulations::client_error)?;
    let Some(assessment) = resource.status.properties else {
        return Err(Error::Setup(format!(
            "{id}: no property evidence yet; it arrives with the final report"
        )));
    };
    if json {
        output::json(&assessment)?;
        return Ok(0);
    }
    output::table(
        [
            "PROPERTY", "KIND", "STATUS", "PASSES", "FAILURES", "EXAMPLE",
        ],
        assessment
            .properties
            .iter()
            .map(|property| {
                [
                    property.id.clone(),
                    output::word(&property.kind).to_lowercase(),
                    output::word(&property.status).to_lowercase(),
                    property.passes.to_string(),
                    property.failures.to_string(),
                    property.example.as_ref().map_or_else(
                        || "-".into(),
                        |example| {
                            format!(
                                "{} at {}",
                                example.origin.machine,
                                seconds(example.origin.time_ns)
                            )
                        },
                    ),
                ]
            })
            .collect(),
    )?;
    super::simulations::property_fields(&assessment);
    Ok(0)
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
                    match (m.exit_code, m.exited_out_of_memory()) {
                        (Some(code), Some(_)) => format!("{code} (out of memory)"),
                        (Some(code), None) => code.to_string(),
                        (None, _) => "-".into(),
                    },
                    if m.oracle { "yes".into() } else { "-".into() },
                ]
            })
            .collect(),
    )?;
    if !world.links.is_empty() {
        println!();
        output::table(
            [
                "FROM", "TO", "LATENCY", "JITTER", "LOSS", "FRAMES", "BYTES", "DROPPED", "FAULT",
            ],
            world
                .links
                .iter()
                .map(|l| {
                    let c = &l.counters;
                    [
                        l.from.clone(),
                        l.to.clone(),
                        seedbox_tui::latency(l.policy.latency_ns),
                        seedbox_tui::latency(l.policy.jitter_ns),
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
                        with_counts(output::word(&f.status).to_lowercase(), f.counts.as_ref()),
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

/// A fault status with what it matched and did, when the engine counted it:
/// `applied (matched 3, failed 1)`. Zero counts are left out.
fn with_counts(status: String, counts: Option<&serde_json::Value>) -> String {
    let Some(kinds) = counts.and_then(|c| c.as_object()) else {
        return status;
    };
    let parts: Vec<String> = kinds
        .values()
        .filter_map(|kind| kind.as_object())
        .flat_map(|kind| kind.iter())
        .filter_map(|(name, value)| {
            let value = value.as_u64().filter(|v| *v > 0)?;
            Some(format!("{} {value}", name.replace('_', " ")))
        })
        .collect();
    if parts.is_empty() {
        format!("{status} (nothing matched)")
    } else {
        format!("{status} ({})", parts.join(", "))
    }
}

/// Kinds of failure-timeline entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum ExplainKind {
    /// Anchors, reseeds, trigger and choice inputs.
    Input,
    /// Faults, with the input or trigger behind each.
    Fault,
    /// Trigger matches.
    Trigger,
    /// Crashes, reboots, pauses and throttles.
    Lifecycle,
    /// Machines becoming ready.
    Ready,
    /// Machine and main-process exits.
    Exit,
    /// Oracle control operations.
    Control,
    /// Answered structured choices.
    Choice,
    /// Workload setup completion.
    Setup,
    /// First occurrence of each SDK event.
    Event,
    /// First time each state value was reached.
    State,
    /// First time each state transition happened.
    Transition,
    /// First pass and first failure of each property.
    Property,
}

impl ExplainKind {
    fn of(kind: seedbox_api::ExplanationKind) -> Self {
        use seedbox_api::ExplanationKind as K;
        match kind {
            K::Input => Self::Input,
            K::Fault => Self::Fault,
            K::Trigger => Self::Trigger,
            K::Lifecycle => Self::Lifecycle,
            K::Ready => Self::Ready,
            K::Exit => Self::Exit,
            K::Control => Self::Control,
            K::Choice => Self::Choice,
            K::Setup => Self::Setup,
            K::Event => Self::Event,
            K::State => Self::State,
            K::Transition => Self::Transition,
            K::Property => Self::Property,
        }
    }
}

/// A completed simulation's failure timeline from retained evidence.
pub(super) fn explain(
    id: &SimulationId,
    kinds: &[ExplainKind],
    machine: Option<&str>,
    json: bool,
) -> Result<u8> {
    let client = daemon::connect(None)?;
    let mut explanation = super::simulations::runtime()?
        .block_on(client.explanation(id))
        .map_err(super::simulations::client_error)?;
    explanation.entries.retain(|e| {
        (kinds.is_empty() || kinds.contains(&ExplainKind::of(e.kind)))
            && machine.is_none_or(|m| e.machine.as_deref().is_none_or(|em| em == m))
    });
    if json {
        output::json(&explanation)?;
        return Ok(0);
    }
    output::table(
        ["AT", "MACHINE", "KIND", "ID", "DETAIL"],
        explanation
            .entries
            .iter()
            .map(|e| {
                let kind = output::word(&e.kind).to_lowercase();
                [
                    seconds(e.at_ns),
                    match (&e.machine, e.incarnation) {
                        (Some(m), Some(i)) if i > 0 => format!("{m}#{i}"),
                        (Some(m), _) => m.clone(),
                        (None, _) => "-".into(),
                    },
                    if e.violation {
                        format!("{kind} VIOLATED")
                    } else {
                        kind
                    },
                    e.id.clone(),
                    if e.links.is_empty() {
                        e.detail.clone()
                    } else {
                        format!("{} [{}]", e.detail, e.links.join(", "))
                    },
                ]
            })
            .collect(),
    )?;
    for omission in &explanation.omissions {
        output::field("omitted", omission);
    }
    Ok(0)
}

/// The fault timeline: recorded and scheduled faults with what became of each.
pub(super) fn timeline(id: &SimulationId, json: bool) -> Result<u8> {
    let client = daemon::connect(None)?;
    let (timeline, world, completed) = super::simulations::runtime()?.block_on(async {
        let timeline = client.timeline(id).await;
        let world = client.world(id).await.ok();
        let completed = client
            .describe(id)
            .await
            .is_ok_and(|r| r.status.phase == seedbox_api::Phase::Completed);
        (timeline, world, completed)
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
        if let Some(reason) = entry
            .id
            .strip_prefix("program:")
            .and_then(|key| world.override_rejections.get(key))
        {
            // Gated generated faults were skipped by design, not refused.
            return if reason.starts_with("skipped") {
                "skipped"
            } else {
                "rejected"
            }
            .into();
        }
        if let Some(trigger) = entry
            .id
            .strip_prefix("program:")
            .and_then(|key| world.triggers.get(key))
        {
            return trigger_outcome(trigger, world);
        }
        if let Some(choice) = entry
            .id
            .strip_prefix("program:")
            .and_then(|key| world.choice_inputs.get(key))
        {
            return match &choice.chosen {
                Some(chosen) => format!("selected {chosen}"),
                None if completed => "unused".into(),
                None => "armed".into(),
            };
        }
        if matches!(entry.action["action"].as_str(), Some("trigger" | "choice")) {
            return if completed { "not reached" } else { "upcoming" }.into();
        }
        entry
            .fault_id
            .and_then(|id| world.faults.iter().find(|f| f.id == id))
            .map_or_else(
                // A finished run never reaches entries it had not admitted.
                || if completed { "not reached" } else { "upcoming" }.into(),
                |fault| {
                    with_counts(
                        output::word(&fault.status).to_lowercase(),
                        fault.counts.as_ref(),
                    )
                },
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
    #[test]
    fn trigger_rules_render_on_one_line() {
        let rule = serde_json::json!({
            "action": "trigger",
            "when": {"on": "event", "name": "commit", "machine": "a"},
            "nth": 2,
            "delay_ns": 500_000_000,
            "then": {"action": "crash", "machine": "b"},
        });
        assert_eq!(
            super::target(&rule),
            "event machine=a name=commit nth=2 delay=0.5s -> crash machine=b"
        );
    }
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

    #[test]
    fn fault_counts_summarize_nonzero_values() {
        let counts = serde_json::json!({"disk_error_counts": {"failed": 1, "matched": 3}});
        assert_eq!(
            with_counts("applied".into(), Some(&counts)),
            "applied (failed 1, matched 3)"
        );
        let none = serde_json::json!({"disk_error_counts": {"failed": 0, "matched": 0}});
        assert_eq!(
            with_counts("applied".into(), Some(&none)),
            "applied (nothing matched)"
        );
        assert_eq!(with_counts("failed".into(), None), "failed");
        let selection = serde_json::json!({"selection_counts": {"dropped": 2, "matched": 5}});
        assert_eq!(
            with_counts("applied".into(), Some(&selection)),
            "applied (dropped 2, matched 5)"
        );
    }
}

/// Rerun a completed simulation to a moment and run a command in one of its
/// machines there; print the command's output and exit as `EXEC_HELP` says.
pub(super) fn exec(
    id: &SimulationId,
    mut request: seedbox_api::ExecRequest,
    keep: bool,
) -> Result<u8> {
    use super::simulations::client_error;
    let client = daemon::connect(None)?;
    request.request_id = Client::request_id().map_err(client_error)?;
    let machine = request.machine.clone();
    let cancellation = CancellationToken::default();
    let _signals = super::signals::Signals::interrupt(&cancellation)?;
    super::simulations::runtime()?.block_on(async {
        let simulation = client
            .submit_exec(id, &request)
            .await
            .map_err(client_error)?;
        let rerun = simulation.metadata.id.clone();
        eprintln!(
            "exec: rerun {rerun} to {} on {machine}",
            seconds(request.at_ns)
        );
        let mut stream = log_stream(&client, &rerun, Some(&machine), true).await?;
        let mut pending = Vec::new();
        let mut outcome = None;
        while outcome.is_none() && !cancellation.is_cancelled() {
            let Some(next) = stream.next().await else {
                break;
            };
            match next.map_err(client_error)? {
                LogDelivery::Entry {
                    entry:
                        LogEntry::Bytes {
                            stream: seedbox_api::LogStream::Stderr,
                            bytes,
                            ..
                        },
                    ..
                } => {
                    pending.extend(bytes);
                    while let Some(end) = pending.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = pending.drain(..=end).collect();
                        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
                        match exec_line(&text) {
                            Some(ExecLine::Output(out)) => {
                                output::bytes(format!("{out}\n").as_bytes())?
                            }
                            Some(ExecLine::Exit(code, note)) => {
                                eprintln!("exec: {note}");
                                outcome = Some(code);
                            }
                            None => {}
                        }
                    }
                }
                LogDelivery::Closed => break,
                _ => {}
            }
        }
        if !keep {
            let _ = client.stop(&rerun).await;
        }
        Ok(outcome.unwrap_or_else(|| {
            eprintln!("exec: the rerun ended before the command ran or finished");
            2
        }))
    })
}

enum ExecLine<'a> {
    Output(&'a str),
    /// The exit status and the supervisor's note.
    Exit(u8, &'a str),
}

/// A supervisor line about the command: `seedbox exec ID| OUTPUT` or
/// `seedbox exec ID: NOTE` (`exited N`, `killed by signal N`, `NAME: not
/// found`; `start` is not an exit).
fn exec_line(line: &str) -> Option<ExecLine<'_>> {
    let rest = line.strip_prefix("seedbox exec ")?;
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let (number, rest) = rest.split_at(digits);
    if number.is_empty() {
        return None;
    }
    if let Some(out) = rest.strip_prefix("| ") {
        return Some(ExecLine::Output(out));
    }
    let note = rest.strip_prefix(": ")?;
    match note {
        "exited 0" => Some(ExecLine::Exit(0, note)),
        _ if note.starts_with("exited ") || note.starts_with("killed by signal ") => {
            Some(ExecLine::Exit(1, note))
        }
        _ if note.ends_with(": not found") => Some(ExecLine::Exit(2, note)),
        _ => None,
    }
}

#[cfg(test)]
mod exec_tests {
    use super::*;

    #[test]
    fn supervisor_exec_lines_are_output_or_an_exit() {
        assert!(matches!(
            exec_line("seedbox exec 4| a: b | c"),
            Some(ExecLine::Output("a: b | c"))
        ));
        assert!(matches!(
            exec_line("seedbox exec 4: exited 0"),
            Some(ExecLine::Exit(0, _))
        ));
        assert!(matches!(
            exec_line("seedbox exec 4: exited 3"),
            Some(ExecLine::Exit(1, _))
        ));
        assert!(matches!(
            exec_line("seedbox exec 4: killed by signal 9"),
            Some(ExecLine::Exit(1, _))
        ));
        assert!(matches!(
            exec_line("seedbox exec 4: ls: not found"),
            Some(ExecLine::Exit(2, _))
        ));
        assert!(exec_line("seedbox exec 4: start /bin/ls").is_none());
        assert!(exec_line("seedbox exec x| y").is_none());
        assert!(exec_line("application line").is_none());
    }
}
