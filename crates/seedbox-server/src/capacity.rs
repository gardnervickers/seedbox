//! Projection of the execution owner's reservations. Paused guests retain RAM
//! and their reservation until the owned task (including cleanup) is joined.
use seedbox_api::{
    ExecutionCapacity, ExecutionOccupant, Phase, PlaybackRate, QueueReason, QueuedSimulation,
    Simulation, SimulationId,
};
use std::collections::BTreeSet;

pub(crate) fn project(
    slots: usize,
    occupying: &BTreeSet<SimulationId>,
    resources: impl Iterator<Item = Simulation>,
) -> ExecutionCapacity {
    let mut result = ExecutionCapacity {
        slots,
        occupying: occupying
            .iter()
            .map(|id| ExecutionOccupant {
                simulation: id.clone(),
                phase: None,
                stage: None,
                paused: false,
                guest_memory_bytes: None,
            })
            .collect(),
        paused: 0,
        queued: Vec::new(),
    };
    for resource in resources {
        let id = resource.metadata.id;
        if let Some(occupant) = result.occupying.iter_mut().find(|o| o.simulation == id) {
            let paused = resource
                .status
                .playback
                .as_ref()
                .is_some_and(|p| p.state.rate == PlaybackRate::Paused && !p.state.terminal);
            result.paused += usize::from(paused);
            let memory =
                match &resource.spec.inputs {
                    seedbox_api::InputSource::Resolved { definition } => definition
                        .machines
                        .values()
                        .chain(std::iter::once(&definition.oracle))
                        .fold(0u64, |total, m| total.saturating_add(m.memory_bytes)),
                    _ => resource.spec.configuration.services.values().fold(
                        0u64,
                        |total, service| {
                            total.saturating_add(
                                u64::from(
                                    service
                                        .memory_mb
                                        .unwrap_or(resource.spec.configuration.machine.memory_mb),
                                ) * 1024
                                    * 1024,
                            )
                        },
                    ),
                };
            *occupant = ExecutionOccupant {
                simulation: id,
                phase: Some(resource.status.phase),
                stage: resource.status.stage,
                paused,
                guest_memory_bytes: Some(memory),
            };
        } else if resource.status.phase == Phase::Queued {
            result.queued.push(QueuedSimulation {
                simulation: id,
                reason: if occupying.len() >= slots {
                    QueueReason::ResidentCapacity
                } else {
                    QueueReason::AwaitingDispatch
                },
            });
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{Metadata, SimulationSpec};
    use std::path::Path;

    fn resource(id: &str) -> Simulation {
        let configuration = seedbox_application::config::parse(
            "name: capacity\nmachines: {}\noracle:\n  image: example/oracle\n",
            Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        Simulation::queued(
            Metadata {
                name: String::new(),
                labels: Default::default(),
                annotations: Default::default(),
                id: id.parse().unwrap(),
                revision: 1,
                created_at_unix_ms: 0,
                deletion_requested_at_unix_ms: None,
                parent: None,
                checkpoint: None,
            },
            SimulationSpec {
                start_paused: false,
                seed: Some(1),
                inputs: seedbox_api::InputSource::Project,
                configuration,
                program: Vec::new(),
            },
        )
    }

    #[test]
    fn capacity_sums_machine_memory_overrides_and_uniform_defaults() {
        let mut resource = resource("aaaaaaaa");
        let mut app = resource.spec.configuration.services["oracle"].clone();
        app.memory_mb = Some(2048);
        resource
            .spec
            .configuration
            .services
            .insert("app".into(), app);
        resource
            .spec
            .configuration
            .services
            .get_mut("oracle")
            .unwrap()
            .memory_mb = Some(256);
        let ids = BTreeSet::from([resource.metadata.id.clone()]);
        let capacity = project(1, &ids, std::iter::once(resource));
        assert_eq!(
            capacity.occupying[0].guest_memory_bytes,
            Some((2048 + 256) * 1024 * 1024)
        );
    }

    #[test]
    fn paused_reservation_blocks_admission_and_reports_configured_memory() {
        let mut paused = resource("aaaaaaaa");
        paused.status.phase = Phase::Running;
        paused.status.playback = Some(seedbox_api::PlaybackObservation {
            age_ns: 0,
            state: seedbox_api::PlaybackStatus {
                position: Default::default(),
                rate: PlaybackRate::Paused,
                achieved_rate: None,
                stepping: false,
                terminal: false,
                application_ready: false,
            },
        });
        let queued = resource("bbbbbbbb");
        let ids = BTreeSet::from([paused.metadata.id.clone()]);
        let full = project(1, &ids, vec![paused.clone(), queued.clone()].into_iter());
        assert_eq!(full.paused, 1);
        assert_eq!(
            full.occupying[0].guest_memory_bytes,
            Some(1024 * 1024 * 1024)
        );
        assert_eq!(full.queued[0].reason, QueueReason::ResidentCapacity);
        // Completion is not enough: the task still owns cleanup until joined.
        paused.status.phase = Phase::Completed;
        paused.status.playback = None;
        let cleaning = project(1, &ids, vec![paused.clone(), queued.clone()].into_iter());
        assert_eq!(cleaning.occupying.len(), 1);
        assert_eq!(cleaning.paused, 0);
        assert_eq!(cleaning.queued[0].reason, QueueReason::ResidentCapacity);
        let joined = project(1, &BTreeSet::new(), vec![paused, queued].into_iter());
        let removed = project(1, &ids, std::iter::empty());
        assert_eq!(removed.occupying.len(), 1);
        assert_eq!(removed.occupying[0].phase, None);
        assert!(joined.occupying.is_empty());
        assert_eq!(joined.queued[0].reason, QueueReason::AwaitingDispatch);
    }
}
