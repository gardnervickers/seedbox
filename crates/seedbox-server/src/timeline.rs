//! Public timeline projection and server-owned derivation. Engine journals remain
//! evidence; authored inputs have stable keys independent of execution fault IDs.
use crate::catalogue::Error;
use seedbox_api::{
    DeriveRequest, FaultTimeline, Phase, ProgramInput, Simulation, SubmitRequest, TimelineEntry,
    TimelineOrigin, WorldState,
};
use seedbox_engine_protocol::intervention::History;
use std::collections::BTreeSet;

pub(crate) fn project(
    resource: &Simulation,
    history: &History,
    world: &WorldState,
) -> FaultTimeline {
    let mut entries: Vec<_> = resource
        .spec
        .program
        .iter()
        .map(|input| TimelineEntry {
            id: format!("program:{}", input.key),
            at_ns: input.at_ns,
            action: input.action.clone(),
            origin: TimelineOrigin::Program,
            position: history
                .entries
                .iter()
                .find(|entry| entry.request_id == input.key)
                .map(|entry| entry.position),
            fault_id: world
                .faults
                .iter()
                .find(|fault| fault.input_key.as_deref() == Some(&input.key))
                .map(|fault| fault.id)
                .or_else(|| {
                    history
                        .entries
                        .iter()
                        .find(|entry| entry.request_id == input.key)
                        .map(|entry| entry.plan.id)
                }),
        })
        .collect();
    for entry in &history.entries {
        let id = format!("program:{}", entry.request_id);
        if entries.iter().any(|e| e.id == id) {
            continue;
        }
        entries.push(TimelineEntry {
            id,
            at_ns: entry.plan.at_ns,
            action: serde_json::to_value(&entry.plan.action).expect("actions serialize"),
            origin: TimelineOrigin::Program,
            position: Some(entry.position),
            fault_id: Some(entry.plan.id),
        });
    }
    for fault in world.faults.iter().filter(|f| f.input_key.is_none()) {
        if let Some(plan) = fault
            .plan
            .as_ref()
            .and_then(|v| serde_json::from_value::<seedbox_control::Plan>(v.clone()).ok())
        {
            entries.push(TimelineEntry {
                id: format!("oracle:{}", fault.id),
                at_ns: plan.at_ns,
                action: serde_json::to_value(plan.action).expect("actions serialize"),
                origin: TimelineOrigin::Recorded,
                position: None,
                fault_id: Some(fault.id),
            });
        }
    }
    // Preserve input document order for equal-time future inputs. Once admitted,
    // execution IDs order observed actions at that time. No ID is invented here.
    entries.sort_by_key(|entry| (entry.at_ns, entry.fault_id.unwrap_or(u32::MAX)));
    FaultTimeline {
        simulation: resource.metadata.id.clone(),
        entries,
    }
}

pub(crate) fn derive(
    resource: Simulation,
    timeline: FaultTimeline,
    request: DeriveRequest,
) -> Result<SubmitRequest, Error> {
    if resource.metadata.created_at_unix_ms != request.source_created_at_unix_ms {
        return Err(Error::Invalid(
            "saved timeline belongs to a different server session".into(),
        ));
    }
    if resource.status.phase != Phase::Completed {
        return Err(Error::Invalid(
            "stop or finish the source simulation before deriving an edited run".into(),
        ));
    }
    let included: BTreeSet<_> = request.included.iter().cloned().collect();
    if included.len() != request.included.len() {
        return Err(Error::Invalid(
            "timeline selection contains duplicate entry IDs".into(),
        ));
    }
    if let Some(unknown) = included.iter().find(|id| {
        !timeline
            .entries
            .iter()
            .any(|entry| entry.id.as_str() == id.as_str())
    }) {
        return Err(Error::Invalid(format!(
            "unknown source timeline entry {unknown}"
        )));
    }
    let metadata = seedbox_spec::definition::Metadata {
        name: resource.metadata.name.clone(),
        labels: resource.metadata.labels.clone(),
        annotations: resource.metadata.annotations.clone(),
    };
    let mut options = resource.options.clone();
    options.start_paused = request.start_paused;
    let mut spec = resource.spec;
    // Oracle actions remain observations; only external overrides form the edited program.
    spec.program
        .retain(|entry| included.contains(&format!("program:{}", entry.key)));
    Ok(SubmitRequest {
        metadata,
        options,
        request_id: request.request_id,
        fault_profile: None,
        spec,
    })
}

pub(crate) fn merge_external(
    program: &[ProgramInput],
    history: &History,
) -> Result<Vec<ProgramInput>, Error> {
    let mut result = program.to_vec();
    for entry in &history.entries {
        let input = ProgramInput {
            key: entry.request_id.clone(),
            at_ns: entry.plan.at_ns,
            action: serde_json::to_value(&entry.plan.action).expect("actions serialize"),
            timeout_ns: entry.plan.deadline_ns.saturating_sub(entry.plan.at_ns),
        };
        if let Some(existing) = result.iter_mut().find(|p| p.key == input.key) {
            if existing != &input {
                return Err(Error::Invalid(format!(
                    "external override {} conflicts with its authored identity",
                    input.key
                )));
            }
        } else {
            result.push(input);
        }
    }
    result.sort_by_key(|p| p.at_ns);
    Ok(result)
}
