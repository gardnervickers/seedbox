//! Observation-only explanations. Never use the worker's PrepareFault path here.
use crate::catalogue::Error;
use seedbox_api::{FaultPreview, FaultPreviewLink, WorldState};
use seedbox_control::Action;

pub(crate) fn preview(
    world: &WorldState,
    configuration: &seedbox_spec::Simulation,
    action: &Action,
) -> Result<FaultPreview, Error> {
    let targets = seedbox_application::engine_adapter::fault_action_targets(configuration, action)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    let mut result = FaultPreview {
        simulation: world.simulation.clone(),
        revision: world.revision,
        simulated_time_ns: world.simulated_time_ns,
        category: crate::fault_catalogue::category(action.name()),
        machines: targets.machines,
        links: targets.links.into_iter().map(|(from, to)| FaultPreviewLink { from, to }).collect(),
        expected_effect: String::new(),
        observations: Vec::new(),
        preconditions: Vec::new(),
        limitations: vec!["Expected targets from this observation; submission is checked again at its execution boundary. Effects are not guaranteed.".into()],
    };
    result.expected_effect = match action {
        Action::Partition { .. } => {
            result
                .limitations
                .push("Other active partitions still apply after this fault is healed.".into());
            "Cut traffic in both directions between the selected groups; links within each group are unchanged.".into()
        }
        Action::Shape {
            latency_ns,
            jitter_ns,
            loss_ppm,
            bandwidth_bps,
            queue_bound_ns,
            network_faults,
            ..
        } => {
            result.limitations.push("Frame outcomes are sampled only during execution. Partitions and other active policies may mask this policy.".into());
            let mut details = vec![
                format!("latency {latency_ns} ns"),
                format!("jitter {jitter_ns} ns"),
                format!("loss {loss_ppm} ppm"),
            ];
            if let Some(rate) = bandwidth_bps {
                details.push(format!("bandwidth {rate} bit/s"));
            }
            if let Some(bound) = queue_bound_ns {
                details.push(format!("queue bound {bound} ns"));
            }
            if network_faults.reorder_ppm > 0 {
                details.push(format!(
                    "reorder {} ppm with {} ns extra delay",
                    network_faults.reorder_ppm, network_faults.reorder_delay_ns
                ));
            }
            if network_faults.duplicate_ppm > 0 {
                details.push(format!("duplicate {} ppm", network_faults.duplicate_ppm));
            }
            if network_faults.burst_loss_ppm > 0 {
                details.push(format!(
                    "burst loss {} ppm for {} frames",
                    network_faults.burst_loss_ppm, network_faults.burst_length
                ));
            }
            if network_faults.latency_spike_ppm > 0 {
                details.push(format!(
                    "latency spikes {} ppm of {} ns",
                    network_faults.latency_spike_ppm, network_faults.latency_spike_ns
                ));
            }
            if network_faults.tcp_reset_ppm > 0 {
                details.push(format!("TCP reset {} ppm", network_faults.tcp_reset_ppm));
            }
            format!("Apply a directed link policy: {}.", details.join(", "))
        }
        Action::DropConnection { .. } => {
            result
                .preconditions
                .push("The exact TCP incarnation must still be available at admission.".into());
            result.limitations.push("Live connection availability is not present in this observation; arming the fault does not prove any packet will be dropped.".into());
            "Drop future packets in the selected direction of the exact TCP incarnation until healed; in-flight frames retain their delivery decisions.".into()
        }
        Action::Heal { handle } => {
            // Resolve the original target set, including network layers masked
            // by newer faults. Visible link handles alone are insufficient.
            let original = world
                .faults
                .iter()
                .rev()
                .filter(|fault| fault.active)
                .filter_map(|fault| fault.plan.clone())
                .filter_map(|plan| serde_json::from_value::<Action>(plan).ok())
                .find(|plan| healable_handle(plan) == Some(handle.as_str()));
            if let Some(original) = original {
                let original_targets = seedbox_application::engine_adapter::fault_action_targets(
                    configuration,
                    &original,
                )
                .map_err(|error| Error::Invalid(error.to_string()))?;
                result.category = crate::fault_catalogue::category(original.name());
                result.machines = original_targets.machines;
                result.links = original_targets
                    .links
                    .into_iter()
                    .map(|(from, to)| FaultPreviewLink { from, to })
                    .collect();
            } else {
                result.limitations.push("This observation has no active fault with that handle; targets are unknown and admission may reject it.".into());
            }
            result.preconditions.push(
                "The handle must still be active and any guest clock effect must be settled."
                    .into(),
            );
            result.limitations.push("Other active faults remain in effect; healing does not guarantee connectivity or guest acknowledgement.".into());
            format!(
                "Remove the active policy or reverse the clock change associated with {handle}."
            )
        }
        Action::Stop { .. } => {
            result.preconditions.push(
                "The application must not already be stopped; its machine must not be paused."
                    .into(),
            );
            "Ask the guest supervisor to stop the application process group; its machine keeps running.".into()
        }
        Action::Restart { .. } => {
            result
                .preconditions
                .push("The application must be stopped; its machine must not be paused.".into());
            "Ask the guest supervisor to start the stopped application; this does not reboot the machine.".into()
        }
        Action::PauseProcess { .. } => {
            result.preconditions.push("The application must not be stopped or already paused; its machine must not be paused.".into());
            "Send SIGSTOP to the application process group; the guest kernel continues running."
                .into()
        }
        Action::ResumeProcess { .. } => {
            result
                .preconditions
                .push("The application must be paused; its machine must not be paused.".into());
            "Send SIGCONT to the paused application process group.".into()
        }
        Action::Pause { .. } => {
            result
                .preconditions
                .push("The machine must be running.".into());
            "Freeze the machine CPU and devices; retain its memory and application state while the world continues.".into()
        }
        Action::Resume { .. } => {
            result
                .preconditions
                .push("The machine must be paused.".into());
            "Resume the machine and catch up its timers and held ingress; an independently paused application stays paused.".into()
        }
        Action::Crash { .. } => {
            result
                .preconditions
                .push("The machine must not already be down.".into());
            "Destroy the VM's RAM, devices, queued I/O and sockets; its application becomes unavailable.".into()
        }
        Action::Reboot { .. } => {
            result
                .preconditions
                .push("The machine must be down.".into());
            "Boot a new machine incarnation from its prepared inputs and retained disk state; application readiness must be observed again.".into()
        }
        Action::ClockSkew {
            offset_ns,
            max_offset_ns,
            ..
        } => {
            result
                .preconditions
                .push("The machine must not be paused or down.".into());
            match offset_ns {
                Some(offset) => format!("Step CLOCK_REALTIME by {offset} ns; monotonic time is unchanged. Heal reverses this offset."),
                None => format!("Step CLOCK_REALTIME by a seeded offset within ±{} ns. The draw occurs only during execution; heal reverses it.", max_offset_ns.unwrap_or(0)),
            }
        }
        Action::ClockDrift {
            rate_ppb,
            max_rate_ppb,
            ..
        } => {
            result.preconditions.push("The machine must not be paused or down, and must not already have an active drift.".into());
            match rate_ppb {
                Some(rate) => format!("Change realtime and monotonic clock rate by {rate} ppb; heal restores the nominal rate."),
                None => format!("Change clock rate by a seeded amount within ±{} ppb. The draw occurs only during execution; heal restores the nominal rate.", max_rate_ppb.unwrap_or(0)),
            }
        }
        Action::DiskError { op, ppm, .. } => {
            result.preconditions.push(
                "The machine must be up, have a retained disk and no active disk errors.".into(),
            );
            result.limitations.push(
                "Only requests the guest issues while active can fail; cached reads and unflushed writes may never reach the disk.".into(),
            );
            let op = match op {
                seedbox_control::DiskOp::Read => "reads",
                seedbox_control::DiskOp::Write => "writes",
                seedbox_control::DiskOp::Flush => "flushes",
                seedbox_control::DiskOp::Any => "reads, writes and flushes",
            };
            format!(
                "Fail the machine's disk {op} with an I/O error, {ppm} per million, until healed."
            )
        }
        Action::Throttle { speed_hz, .. } => {
            result
                .preconditions
                .push("The machine must not already be throttled.".into());
            format!(
                "Run the machine's CPU at {speed_hz} Hz, below the machine's configured speed, until healed."
            )
        }
    };
    if action.is_process() || action.is_lifecycle() {
        for name in &result.machines {
            if let Some(machine) = world.machines.iter().find(|m| &m.name == name) {
                let condition = match machine.condition {
                    seedbox_api::MachineCondition::Starting => "starting",
                    seedbox_api::MachineCondition::Ready => "ready",
                    seedbox_api::MachineCondition::Paused => "paused",
                    seedbox_api::MachineCondition::Crashed => "crashed",
                    seedbox_api::MachineCondition::Exited => "exited",
                };
                let application = match machine.application.condition {
                    seedbox_api::ApplicationCondition::Unknown => "unknown",
                    seedbox_api::ApplicationCondition::Starting => "starting",
                    seedbox_api::ApplicationCondition::Running => "running",
                    seedbox_api::ApplicationCondition::Stopped => "stopped",
                    seedbox_api::ApplicationCondition::Paused => "paused",
                    seedbox_api::ApplicationCondition::Exited => "exited",
                    seedbox_api::ApplicationCondition::Unavailable => "unavailable",
                };
                result.observations.push(format!(
                    "{name}: machine {condition}, application {application}, incarnation {}.",
                    machine.incarnation
                ));
                if let Some(at) = machine.application.changed_at_ns {
                    result
                        .observations
                        .push(format!("Application state observed at {at} ns."));
                }
            } else {
                result
                    .observations
                    .push(format!("{name}: machine and application state unknown."));
            }
        }
    }
    if action.is_process() {
        result.limitations.push("Guest acknowledgement is required. A down machine makes the request unavailable; readiness and outstanding requests may change its outcome.".into());
    }
    if action.is_process() || action.is_lifecycle() {
        result.limitations.push("Lifecycle preconditions describe required engine state, not a guarantee derived from this observation.".into());
    }
    Ok(result)
}

fn healable_handle(action: &Action) -> Option<&str> {
    match action {
        Action::Partition { handle, .. }
        | Action::Shape { handle, .. }
        | Action::ClockSkew { handle, .. }
        | Action::ClockDrift { handle, .. }
        | Action::Throttle { handle, .. }
        | Action::DiskError { handle, .. }
        | Action::DropConnection { handle, .. } => Some(handle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_api::{
        ApplicationCondition, FaultCategory, FaultState, FaultStatus, MachineCondition,
    };
    use serde_json::json;

    fn fixture() -> (seedbox_spec::Simulation, WorldState) {
        let configuration = seedbox_application::config::parse(
            "name: test\nservices:\n  n1:\n    image: example/n1\n  n2:\n    image: example/n2\n  oracle:\n    image: example/oracle\n",
            std::path::Path::new("/project/compose.yaml"), &Default::default(),
        ).unwrap();
        let world =
            crate::world::Tracker::new("abcdefgh".parse().unwrap(), &configuration).snapshot();
        (configuration, world)
    }

    #[test]
    fn partition_and_shape_preview_use_canonical_directed_targets_without_mutation() {
        let (configuration, world) = fixture();
        let before = world.clone();
        let partition = Action::Partition {
            handle: "split".into(),
            left: vec!["n1".into()],
            right: vec!["n2".into(), "oracle".into()],
        };
        let first = preview(&world, &configuration, &partition).unwrap();
        assert_eq!(first.category, FaultCategory::Network);
        assert_eq!(first.machines, ["n1", "n2", "oracle"]);
        assert_eq!(first.links.len(), 4);
        assert!(first.links.contains(&FaultPreviewLink {
            from: "n2".into(),
            to: "n1".into()
        }));
        assert!(!first.links.contains(&FaultPreviewLink {
            from: "n2".into(),
            to: "oracle".into()
        }));
        for _ in 0..5 {
            assert_eq!(preview(&world, &configuration, &partition).unwrap(), first);
        }
        assert_eq!(world, before);
        let shape: Action = serde_json::from_value(json!({"action":"shape", "handle":"slow", "from":["n1"], "to":["n2"], "latency_ns":configuration.network.latency_ns.max(1), "jitter_ns":0, "loss_ppm":50})).unwrap();
        assert_eq!(
            preview(&world, &configuration, &shape).unwrap().links,
            vec![FaultPreviewLink {
                from: "n1".into(),
                to: "n2".into()
            }]
        );
        let invalid = Action::Partition {
            handle: "bad".into(),
            left: vec!["n1".into()],
            right: vec!["n1".into()],
        };
        assert!(preview(&world, &configuration, &invalid).is_err());
    }

    #[test]
    fn heal_resolves_active_original_targets_even_when_link_layer_is_masked() {
        let (configuration, mut world) = fixture();
        world.faults.push(FaultState {
            counts: None,
            id: 1,
            input_key: None,
            action: "partition".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(10),
            active: true,
            plan: Some(
                json!({"action":"partition", "handle":"old", "left":["n1"], "right":["n2"]}),
            ),
        });
        for link in &mut world.links {
            link.partitioned_by = Some("newer".into());
        }
        let result = preview(
            &world,
            &configuration,
            &Action::Heal {
                handle: "old".into(),
            },
        )
        .unwrap();
        assert_eq!(result.category, FaultCategory::Network);
        assert_eq!(result.machines, ["n1", "n2"]);
        assert_eq!(result.links.len(), 2);
        let unknown = preview(
            &world,
            &configuration,
            &Action::Heal {
                handle: "missing".into(),
            },
        )
        .unwrap();
        assert_eq!(unknown.category, FaultCategory::Recovery);
        assert!(unknown.machines.is_empty());
        assert!(unknown
            .limitations
            .iter()
            .any(|s| s.contains("targets are unknown")));
    }

    #[test]
    fn selected_connection_preview_describes_directed_loss_and_heal() {
        let (configuration, mut world) = fixture();
        let action: Action = serde_json::from_value(json!({"action":"drop_connection", "handle":"selected", "connection":{
            "from":"n1", "to":"n2", "src_addr":"10.0.0.1", "dst_addr":"10.0.0.2", "src_port":1234, "dst_port":80, "incarnation":1
        }})).unwrap();
        let result = preview(&world, &configuration, &action).unwrap();
        assert!(result
            .expected_effect
            .contains("Drop future packets in the selected direction"));
        assert!(!result.expected_effect.contains("reset"));
        assert_eq!(
            result.links,
            [FaultPreviewLink {
                from: "n1".into(),
                to: "n2".into()
            }]
        );
        world.faults.push(FaultState {
            counts: None,
            id: 1,
            input_key: None,
            action: "drop_connection".into(),
            machine: None,
            status: FaultStatus::Applied,
            applied_time_ns: Some(10),
            plan: Some(serde_json::to_value(action).unwrap()),
            active: true,
        });
        let heal = preview(
            &world,
            &configuration,
            &Action::Heal {
                handle: "selected".into(),
            },
        )
        .unwrap();
        assert_eq!(heal.category, FaultCategory::Network);
        assert_eq!(heal.links, result.links);
    }

    #[test]
    fn lifecycle_observations_keep_machine_pause_separate_and_unknown_explicit() {
        let (configuration, mut world) = fixture();
        let action = Action::Resume {
            machine: "n1".into(),
        };
        let result = preview(&world, &configuration, &action).unwrap();
        assert!(result
            .observations
            .iter()
            .any(|s| s.contains("application unknown")));
        let machine = world.machines.iter_mut().find(|m| m.name == "n1").unwrap();
        machine.condition = MachineCondition::Paused;
        machine.application.condition = ApplicationCondition::Paused;
        machine.application.changed_at_ns = Some(42);
        let result = preview(&world, &configuration, &action).unwrap();
        assert!(result.expected_effect.contains("application stays paused"));
        assert!(result
            .observations
            .iter()
            .any(|s| s.contains("machine paused, application paused")));
        assert!(result
            .limitations
            .iter()
            .any(|s| s.contains("not guaranteed")));
        let restart = preview(
            &world,
            &configuration,
            &Action::Restart {
                machine: "n1".into(),
            },
        )
        .unwrap();
        assert_eq!(restart.category, FaultCategory::Application);
        assert!(restart.preconditions.iter().any(|s| s.contains("stopped")));
        assert!(restart
            .limitations
            .iter()
            .any(|s| s.contains("acknowledgement")));
    }
}
