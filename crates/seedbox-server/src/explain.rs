//! A completed simulation's failure timeline from retained evidence. Every
//! entry names a stable identity and the identities it links to; what the
//! evidence does not keep becomes an omission rather than a guess.
use seedbox_api::{
    EventKind, Explanation, ExplanationEntry, ExplanationKind as Kind, MachineTransition,
    ProgramInput, SimulationEvent, SimulationId, WorldState,
};
use seedbox_artifacts::execution::ExecutionReport;
use seedbox_engine_protocol::property::{Kind as PropKind, Origin, PropertyStatus};
use serde_json::Value;

/// Longest detail kept from guest-supplied JSON.
const MAX_DETAIL: usize = 240;

fn clip(mut text: String) -> String {
    if text.len() > MAX_DETAIL {
        let mut end = MAX_DETAIL;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

fn entry(at_ns: u64, kind: Kind, id: String, detail: String) -> ExplanationEntry {
    ExplanationEntry {
        at_ns,
        kind,
        machine: None,
        incarnation: None,
        id,
        detail: clip(detail),
        links: Vec::new(),
        violation: false,
    }
}

fn at(origin: &Origin, kind: Kind, id: String, detail: String) -> ExplanationEntry {
    ExplanationEntry {
        machine: Some(origin.machine.clone()),
        incarnation: Some(origin.incarnation),
        ..entry(origin.time_ns, kind, id, detail)
    }
}

fn transition(t: &MachineTransition) -> String {
    match t {
        MachineTransition::Paused => "paused".into(),
        MachineTransition::Resumed => "resumed".into(),
        MachineTransition::Crashed => "crashed".into(),
        MachineTransition::Rebooted => "rebooted".into(),
        MachineTransition::Throttled { speed_hz: Some(hz) } => format!("throttled to {hz} Hz"),
        MachineTransition::Throttled { speed_hz: None } => "throttle healed".into(),
    }
}

fn word(value: &impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "-".into())
}

/// Program inputs, faults, triggers, lifecycle events, choices and ledger
/// records, ordered by simulated time. `gaps` counts event records lost
/// before they were retained.
pub(crate) fn project(
    simulation: &SimulationId,
    program: &[ProgramInput],
    world: &WorldState,
    events: &[SimulationEvent],
    gaps: u64,
    report: Option<&ExecutionReport>,
) -> Explanation {
    let mut entries = Vec::new();
    let mut omissions = Vec::new();
    let fault_of = |key: &str| {
        world
            .faults
            .iter()
            .find(|f| f.input_key.as_deref() == Some(key))
            .map(|f| format!("fault:{}", f.id))
    };

    // Inputs that are not faults themselves: their faults follow below.
    for input in program {
        let action = &input.action;
        let name = action["action"].as_str().unwrap_or("-");
        let detail = match name {
            "anchor" => "anchor".to_owned(),
            "reseed" => format!("reseed {}", action["seed"]),
            "choice" => format!(
                "choice input {} {} selects {}",
                action["machine"].as_str().unwrap_or("any"),
                action["site"].as_str().unwrap_or("-"),
                action["select"]
            ),
            "trigger" => {
                let state = world.triggers.get(&input.key);
                let status = state.map_or("not reached".to_owned(), |t| word(&t.status));
                let reason = state
                    .and_then(|t| t.reason.as_deref())
                    .map(|r| format!(": {r}"))
                    .unwrap_or_default();
                format!(
                    "arm {} on {} then {} ({status}{reason})",
                    input.key,
                    serde_json::to_string(&action["when"]).unwrap_or_default(),
                    action["then"]["action"].as_str().unwrap_or("-"),
                )
            }
            _ => continue,
        };
        let mut e = entry(
            input.at_ns,
            Kind::Input,
            format!("input:{}", input.key),
            detail,
        );
        e.links.extend(fault_of(&input.key));
        entries.push(e);
    }
    for (key, trigger) in &world.triggers {
        let Some(matched) = trigger.matched_at_ns else {
            continue;
        };
        let mut e = entry(
            matched,
            Kind::Trigger,
            format!("trigger:{key}"),
            format!(
                "matched occurrence {}; effect due at {}",
                trigger.seen,
                trigger
                    .effect_at_ns
                    .map_or("-".to_owned(), |t| format!("{t} ns"))
            ),
        );
        e.links.push(format!("input:{key}"));
        e.links
            .extend(trigger.fault_id.map(|id| format!("fault:{id}")));
        entries.push(e);
    }
    for fault in &world.faults {
        let scheduled = fault
            .input_key
            .as_deref()
            .and_then(|k| program.iter().find(|i| i.key == k))
            .map(|i| i.at_ns);
        let Some(time) = fault.applied_time_ns.or(scheduled) else {
            omissions.push(format!(
                "fault {} ({}) has no recorded time; status {}",
                fault.id,
                fault.action,
                word(&fault.status)
            ));
            continue;
        };
        let mut e = entry(
            time,
            Kind::Fault,
            format!("fault:{}", fault.id),
            format!(
                "{} {}: {}",
                fault.action,
                fault
                    .machine
                    .as_deref()
                    .or_else(|| fault.plan.as_ref()?.get("handle")?.as_str())
                    .unwrap_or("network"),
                word(&fault.status)
            ),
        );
        e.machine = fault.machine.clone();
        if let Some(key) = &fault.input_key {
            let kind = if world.triggers.contains_key(key) {
                "trigger"
            } else {
                "input"
            };
            e.links.push(format!("{kind}:{key}"));
        }
        entries.push(e);
    }

    for event in events {
        let Some(time) = event.simulated_time_ns else {
            continue;
        };
        let e = match &event.event {
            EventKind::MachineLifecycle {
                machine,
                incarnation,
                transition: t,
            } => {
                let mut e = entry(
                    time,
                    Kind::Lifecycle,
                    format!("machine:{machine}#{incarnation}"),
                    transition(t),
                );
                e.machine = Some(machine.clone());
                e.incarnation = Some(*incarnation);
                e.links.extend(
                    world
                        .faults
                        .iter()
                        .filter(|f| {
                            f.applied_time_ns == Some(time) && f.machine.as_deref() == Some(machine)
                        })
                        .map(|f| format!("fault:{}", f.id)),
                );
                e
            }
            EventKind::MachineReady {
                machine,
                incarnation,
            } => ExplanationEntry {
                machine: Some(machine.clone()),
                incarnation: Some(*incarnation),
                ..entry(
                    time,
                    Kind::Ready,
                    format!("machine:{machine}#{incarnation}"),
                    "ready".into(),
                )
            },
            EventKind::MachineExited {
                machine,
                incarnation,
                code,
            } => ExplanationEntry {
                machine: Some(machine.clone()),
                incarnation: Some(*incarnation),
                ..entry(
                    time,
                    Kind::Exit,
                    format!("machine:{machine}#{incarnation}"),
                    format!("exited with code {code}"),
                )
            },
            EventKind::OverrideRejected {
                input_key,
                at_ns,
                reason,
            } => entry(
                *at_ns,
                Kind::Input,
                format!("input:{input_key}"),
                format!("rejected: {reason}"),
            ),
            EventKind::ControlChanged { control } => entry(
                time,
                Kind::Control,
                "control".into(),
                serde_json::to_string(control).unwrap_or_default(),
            ),
            EventKind::SourceGap { first, last } => {
                omissions.push(format!(
                    "worker records {first} to {last} were lost before the server kept them"
                ));
                continue;
            }
            EventKind::SourceUnavailable => {
                omissions.push("a worker observation was malformed or incomplete".into());
                continue;
            }
            _ => continue,
        };
        entries.push(e);
    }
    if gaps > 0 {
        omissions.push(format!(
            "{gaps} lifecycle and fault events fell outside the retained event history"
        ));
    }

    let Some(report) = report else {
        omissions.push("no execution report was retained: no choices or properties".into());
        return finish(simulation, entries, omissions);
    };
    if let Some(choices) = &report.choices {
        for c in &choices.records {
            let how = match &c.input_key {
                Some(key) => format!("by input {key}"),
                None => "drawn".into(),
            };
            let mut e = ExplanationEntry {
                machine: Some(c.machine.clone()),
                incarnation: Some(c.incarnation),
                ..entry(
                    c.requested_at_ns,
                    Kind::Choice,
                    format!("choice:{}/{}#{}", c.machine, c.site, c.occurrence),
                    format!(
                        "chose {} ({} of {}) {how}",
                        c.chosen,
                        c.selected + 1,
                        c.alternatives
                    ),
                )
            };
            e.links
                .extend(c.input_key.iter().map(|k| format!("input:{k}")));
            entries.push(e);
        }
        if choices.untracked > 0 {
            omissions.push(format!(
                "{} later choices were answered but not kept",
                choices.untracked
            ));
        }
    }
    let Some(ledger) = &report.properties else {
        return finish(simulation, entries, omissions);
    };
    for (machine, origin) in &ledger.setup {
        entries.push(at(
            origin,
            Kind::Setup,
            format!("machine:{machine}#{}", origin.incarnation),
            "setup complete".into(),
        ));
    }
    for exit in &ledger.exits {
        let how = match (exit.code, exit.signal) {
            (_, Some(signal)) => format!("main process killed by signal {signal}"),
            (Some(code), None) => format!("main process exited with code {code}"),
            (None, None) => "main process exited".into(),
        };
        entries.push(at(
            &exit.origin,
            Kind::Exit,
            format!(
                "machine:{}#{}",
                exit.origin.machine, exit.origin.incarnation
            ),
            how,
        ));
    }
    let mut repeated = Vec::new();
    for (name, origin) in &ledger.event_first {
        let count = ledger.events.get(name).copied().unwrap_or(1);
        if count > 1 {
            repeated.push(format!("{name} ×{count}"));
        }
        entries.push(at(
            origin,
            Kind::Event,
            format!("event:{name}"),
            format!("first of {count}"),
        ));
    }
    if !repeated.is_empty() {
        omissions.push(format!(
            "only each event name's first occurrence is kept: {}",
            repeated.join(", ")
        ));
    }
    for (key, origin) in &ledger.states {
        entries.push(at(
            origin,
            Kind::State,
            format!("state:{key}"),
            "first reached".into(),
        ));
    }
    for (key, origin) in &ledger.transitions {
        entries.push(at(
            origin,
            Kind::Transition,
            format!("transition:{key}"),
            "first happened".into(),
        ));
    }
    if ledger.states_dropped > 0 {
        omissions.push(format!(
            "{} states and transitions beyond the kept limit",
            ledger.states_dropped
        ));
    }
    let mut counted = Vec::new();
    for (id, property) in &ledger.properties {
        let details = |d: &Option<Value>| {
            d.as_ref()
                .map(|d| format!(": {}", serde_json::to_string(d).unwrap_or_default()))
                .unwrap_or_default()
        };
        if property.passes + property.failures > 2 {
            counted.push(format!(
                "{id} ({} passed, {} failed)",
                property.passes, property.failures
            ));
        }
        // A reachability property's encounter is a reach, and reaching an
        // unreachable one is its violation.
        let reach = matches!(property.kind, PropKind::Reachable | PropKind::Unreachable);
        if let Some(example) = &property.first_pass {
            let mut e = at(
                &example.origin,
                Kind::Property,
                format!("property:{id}"),
                format!(
                    "{}{}",
                    if reach {
                        "first reached"
                    } else {
                        "first passed"
                    },
                    details(&example.details)
                ),
            );
            e.violation = property.kind == PropKind::Unreachable
                && property.status() == PropertyStatus::Violated;
            entries.push(e);
        }
        if let Some(example) = &property.first_failure {
            let mut e = at(
                &example.origin,
                Kind::Property,
                format!("property:{id}"),
                format!("first failed{}", details(&example.details)),
            );
            e.violation = property.kind != PropKind::Unreachable
                && property.status() == PropertyStatus::Violated;
            entries.push(e);
        }
    }
    if !counted.is_empty() {
        omissions.push(format!(
            "only each property's first pass and first failure are kept: {}",
            counted.join(", ")
        ));
    }
    if ledger.error_count > 0 {
        omissions.push(format!(
            "{} property evidence errors; the evidence is incomplete",
            ledger.error_count
        ));
    }
    finish(simulation, entries, omissions)
}

fn finish(
    simulation: &SimulationId,
    mut entries: Vec<ExplanationEntry>,
    omissions: Vec<String>,
) -> Explanation {
    entries.sort_by_key(|e| e.at_ns);
    Explanation {
        simulation: simulation.clone(),
        entries,
        omissions,
    }
}

#[cfg(test)]
#[path = "explain_tests.rs"]
mod tests;
