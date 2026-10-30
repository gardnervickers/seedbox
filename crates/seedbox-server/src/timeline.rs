//! Public timeline projection and server-owned derivation. Engine journals remain
//! evidence; authored inputs have stable keys independent of execution fault IDs.
use crate::catalogue::Error;
use seedbox_api::{
    DeriveRequest, FaultTimeline, Phase, Simulation, SubmitRequest, TimelineEntry, TimelineOrigin,
    WorldState,
};
use seedbox_engine_protocol::intervention::History;
use seedbox_spec::definition::Override;
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
            id: format!("program:{}", input.id),
            at_ns: input.at_ns,
            action: input.action.clone(),
            origin: TimelineOrigin::Program,
            position: history
                .entries
                .iter()
                .find(|entry| entry.request_id == input.id)
                .map(|entry| entry.position),
            fault_id: world
                .faults
                .iter()
                .find(|fault| fault.input_key.as_deref() == Some(&input.id))
                .map(|fault| fault.id)
                .or_else(|| {
                    history
                        .entries
                        .iter()
                        .find(|entry| entry.request_id == input.id)
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
    // Oracle actions: world faults carry the admitted action (not a plan) and
    // the time it took effect. Interactive admissions are listed above.
    for fault in world.faults.iter().filter(|f| f.input_key.is_none()) {
        if entries.iter().any(|e| e.fault_id == Some(fault.id)) {
            continue;
        }
        if let (Some(action), Some(at_ns)) = (
            fault.plan.as_ref().and_then(|v| {
                serde_json::from_value::<seedbox_engine_protocol::control::Action>(v.clone()).ok()
            }),
            fault.applied_time_ns,
        ) {
            entries.push(TimelineEntry {
                id: format!("oracle:{}", fault.id),
                at_ns,
                action: serde_json::to_value(action).expect("actions serialize"),
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
    if let Some(recorded) = included.iter().find(|id| id.starts_with("oracle:")) {
        return Err(Error::Invalid(format!(
            "timeline entry {recorded} is an oracle action, which the derived run's oracle decides again; select only program entries"
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
        .retain(|entry| included.contains(&format!("program:{}", entry.id)));
    Ok(SubmitRequest {
        metadata,
        options,
        request_id: request.request_id,
        spec,
    })
}

pub(crate) fn merge_external(
    program: &[Override],
    history: &History,
) -> Result<Vec<Override>, Error> {
    let mut result = program.to_vec();
    for entry in &history.entries {
        let input = Override {
            id: entry.request_id.clone(),
            at_ns: entry.plan.at_ns,
            action: serde_json::to_value(&entry.plan.action).expect("actions serialize"),
            timeout_ns: entry.plan.deadline_ns.saturating_sub(entry.plan.at_ns),
        };
        if let Some(existing) = result.iter_mut().find(|p| p.id == input.id) {
            if existing != &input {
                return Err(Error::Invalid(format!(
                    "external override {} conflicts with its authored identity",
                    input.id
                )));
            }
        } else {
            result.push(input);
        }
    }
    result.sort_by_key(|p| p.at_ns);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{FaultState, FaultStatus, Metadata, SimulationSpec};
    use seedbox_spec::definition::Override;

    #[test]
    fn oracle_actions_appear_as_recorded_entries_once() {
        let configuration = seedbox_application::config::parse(
            "name: timeline\nmachines:\n  n1:\n    image: example/n1\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let mut resource = Simulation::queued(
            Metadata {
                name: String::new(),
                labels: Default::default(),
                annotations: Default::default(),
                id: "abcdefgh".parse().unwrap(),
                revision: 1,
                created_at_unix_ms: 0,
                deletion_requested_at_unix_ms: None,
                parent: None,
                checkpoint: None,
            },
            SimulationSpec {
                seed: Some(1),
                inputs: seedbox_api::InputSource::Project,
                configuration: configuration.clone(),
                program: Vec::new(),
            },
        );
        resource.spec.program.push(Override {
            id: "cut".into(),
            at_ns: 5,
            action: serde_json::json!({"action": "partition", "handle": "cut", "left": ["n1"], "right": ["oracle"]}),
            timeout_ns: 10,
        });
        let mut world =
            crate::world::Tracker::new("abcdefgh".parse().unwrap(), &configuration).snapshot();
        let fault = |id, input_key: Option<&str>, at, plan: serde_json::Value| FaultState {
            id,
            input_key: input_key.map(str::to_owned),
            action: plan["action"].as_str().unwrap().into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(at),
            plan: Some(plan),
            active: false,
        };
        world.faults = vec![
            fault(
                1,
                Some("cut"),
                5,
                serde_json::json!({"action": "partition", "handle": "cut", "left": ["n1"], "right": ["oracle"]}),
            ),
            fault(
                2,
                None,
                7,
                serde_json::json!({"action": "partition", "handle": "p1", "left": ["n1"], "right": ["oracle"]}),
            ),
            fault(
                3,
                None,
                9,
                serde_json::json!({"action": "heal", "handle": "p1"}),
            ),
        ];
        let timeline = project(&resource, &History::default(), &world);
        let entries: Vec<_> = timeline
            .entries
            .iter()
            .map(|e| (e.id.as_str(), e.at_ns, e.origin, e.fault_id))
            .collect();
        assert_eq!(
            entries,
            [
                ("program:cut", 5, TimelineOrigin::Program, Some(1)),
                ("oracle:2", 7, TimelineOrigin::Recorded, Some(2)),
                ("oracle:3", 9, TimelineOrigin::Recorded, Some(3)),
            ]
        );
        assert_eq!(
            timeline.entries[2].action,
            serde_json::json!({"action": "heal", "handle": "p1"})
        );
        // A derived run cannot replay an oracle action as a program input.
        resource.status.phase = Phase::Completed;
        let request = |included: &[&str]| DeriveRequest {
            request_id: "derived".into(),
            source_created_at_unix_ms: 0,
            included: included.iter().map(|id| (*id).to_owned()).collect(),
            start_paused: false,
        };
        let error = derive(resource.clone(), timeline.clone(), request(&["oracle:2"]))
            .unwrap_err()
            .to_string();
        assert!(error.contains("oracle:2 is an oracle action"), "{error}");
        let derived = derive(resource, timeline, request(&["program:cut"])).unwrap();
        assert_eq!(derived.spec.program.len(), 1);
    }
}
