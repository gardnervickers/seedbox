//! World JSON generated from verified execution inputs.

use super::*;
use seedbox_engine_protocol::disk::DiskFaultModel;
use seedbox_engine_protocol::world_config::{
    CheckpointConfig, ControlChannel, ControlConfig, DiskConfig, LinkConfig, MachineConfig,
    NetworkConfig, WorldConfig,
};

pub(super) fn control_latency(request: &ExecutionRequest<'_>) -> Result<u64> {
    request
        .network
        .latency_ns
        .checked_mul(2)
        .and_then(|n| n.checked_add(request.schedule.stop_tolerance_ns.checked_mul(2)?))
        .and_then(|n| n.checked_add(1_000_000))
        .ok_or_else(|| Error::Execution("control latency overflow".into()))
}

/// Size of the retained disk of a machine whose service mounts named volumes.
/// Part of the execution semantics: changing it changes runs.
pub const VOLUME_DISK_MIB: u64 = 1024;

/// Translate an execution request without consulting registry tags or source
/// paths. Exposed for preparation tools and report inspection without KVM.
pub fn world(request: &ExecutionRequest<'_>) -> Result<WorldConfig> {
    if request
        .control
        .is_some_and(|c| c.service != request.check.service)
    {
        return Err(Error::Execution(
            "controller must also be the check service".into(),
        ));
    }
    if request.prepared.guests.is_empty() || request.prepared.guests.len() > 254 {
        return Err(Error::Integrity("expected 1..254 prepared machines".into()));
    }
    let memory = request.prepared.profile.semantic_parameters.memory_mb;
    let workload_machine = request
        .prepared
        .placement
        .get(&request.check.service)
        .ok_or_else(|| Error::Integrity("missing workload placement".into()))?;
    // This guest ABI launches one application in each machine. The public
    // execution contract also permits other backends to co-locate services.
    if request.prepared.placement.len() != request.prepared.guests.len()
        || request.prepared.placement.values().collect::<BTreeSet<_>>()
            != request.prepared.guests.keys().collect::<BTreeSet<_>>()
    {
        return Err(Error::Execution(
            "oci-init-v1 requires one service per machine".into(),
        ));
    }
    let control = request
        .control
        .map(|c| -> Result<ControlConfig> {
            let machine = request
                .prepared
                .placement
                .get(&c.service)
                .ok_or_else(|| Error::Execution("controller placement missing".into()))?;
            Ok(ControlConfig {
                machine: machine.clone(),
                latency_ns: control_latency(request)?,
                placement: request.prepared.placement.clone(),
            })
        })
        .transpose()?;
    let machines = request
        .prepared
        .guests
        .iter()
        .map(|(name, digest)| machine(request, name, digest))
        .collect::<Result<Vec<_>>>()?;
    let network = network(request)?;
    let mut world = WorldConfig {
        kernel: artifact(request, &request.prepared.kernel)?,
        mem_mb: memory.into(),
        // MAX is the shared scheduler's idle/no-event sentinel. Keep the
        // public open-ended limit, but use the last real instant internally.
        duration_ns: request.limits.simulated_time_ns.min(u64::MAX - 1),
        end_on_exit: Some(workload_machine.clone()),
        timer_jitter_ns: request.schedule.timer_jitter_ns,
        stop_tolerance_ns: request.schedule.stop_tolerance_ns,
        cpu_speed_hz: request.cpu.speed_hz,
        progress_ns: request.cpu.progress_ns(),
        progress_jitter_ns: request.cpu.progress_jitter_ns(),
        exact_timer_ppm: None,
        network: Some(network),
        machines,
        control,
        control_channel: Some(ControlChannel::VirtioConsoleV1),
        launch_skew_ns: None,
        disk_loss: None,
        checkpoint: None,
        boot: None,
        faults: Vec::new(),
        input_program: request.program.to_vec(),
        scenario: Vec::new(),
        kill_restart: None,
    };
    let lookahead = request
        .network
        .latency_ns
        .saturating_sub(request.network.jitter_ns);
    world.launch_skew_ns = crate::engine::arm_launch_skew(&request.prepared.profile, lookahead);
    if request.prepared.profile.semantic_parameters.torn_writes {
        world.disk_loss = Some(DiskFaultModel {
            sector_bytes: 512,
            survival_probability: 0.5,
        });
    }
    world.checkpoint = checkpoint_metadata(request, &world)?;
    Ok(world)
}

/// The absolute path of a verified artifact.
fn artifact(request: &ExecutionRequest<'_>, digest: &ContentDigest) -> Result<std::path::PathBuf> {
    let path = request
        .artifacts
        .get(digest)
        .ok_or_else(|| Error::Integrity(format!("missing verified artifact {}", digest.hex())))?;
    Ok(std::path::absolute(path)?)
}

/// A machine booting `digest`; it gets a disk when a service placed on it
/// mounts a volume.
fn machine(
    request: &ExecutionRequest<'_>,
    name: &String,
    digest: &ContentDigest,
) -> Result<MachineConfig> {
    let volumes = request.prepared.placement.iter().any(|(service, machine)| {
        machine == name
            && request
                .prepared
                .images
                .get(service)
                .is_some_and(|image| !image.launch.disk_volumes.is_empty())
    });
    Ok(MachineConfig {
        name: name.clone(),
        initrd: artifact(request, digest)?,
        disk: volumes.then_some(DiskConfig {
            size_mib: Some(VOLUME_DISK_MIB),
            image: None,
        }),
        cmdline: None,
    })
}

/// The network policy, with link overrides resolved from services to machines.
fn network(request: &ExecutionRequest<'_>) -> Result<NetworkConfig> {
    let machine = |service: &str| {
        request
            .prepared
            .placement
            .get(service)
            .cloned()
            .ok_or_else(|| Error::Integrity(format!("missing link placement for {service}")))
    };
    let links = request
        .network
        .links
        .iter()
        .map(|link| {
            Ok(LinkConfig {
                from: machine(&link.from)?,
                to: machine(&link.to)?,
                latency_ns: link.latency_ns,
                jitter_ns: link.jitter_ns,
                loss_ppm: link.loss_ppm,
                bandwidth_bps: link.bandwidth_bps,
                queue_bound_ns: link.queue_bound_ns,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(NetworkConfig {
        latency_ns: request.network.latency_ns,
        jitter_ns: request.network.jitter_ns,
        bandwidth_bps: request.network.bandwidth_bps,
        queue_bound_ns: request.network.queue_bound_ns,
        links,
    })
}

/// The recorded checkpoint of `world`, bound to its parent identity. The
/// engine computes the same identity from the same typed document.
pub(super) fn checkpoint_metadata(
    request: &ExecutionRequest<'_>,
    world: &WorldConfig,
) -> Result<Option<CheckpointConfig>> {
    let Some((at, seed)) = recorded_checkpoint(&request.prepared.profile)? else {
        return Ok(None);
    };
    if at >= request.limits.simulated_time_ns {
        return Err(Error::Execution(
            "checkpoint time must be before the simulated time limit".into(),
        ));
    }
    let parent = checkpoint::parent_identity(
        world,
        request.seed,
        request.prepared.profile.engine.hex(),
        recorded_deferred_tx(&request.prepared.profile)?,
        recorded_page_timer(&request.prepared.profile),
        |path| {
            request
                .artifacts
                .iter()
                .find_map(|(digest, artifact)| {
                    std::path::absolute(artifact)
                        .ok()
                        .filter(|p| p == path)
                        .map(|_| digest.hex().to_owned())
                })
                .ok_or_else(|| format!("checkpoint artifact {} is not verified", path.display()))
        },
    )
    .map_err(Error::Integrity)?;
    Ok(Some(CheckpointConfig {
        at_ns: at,
        branch_seed: seed,
        branch_identity: Some(checkpoint::branch_identity(&parent, at, seed)),
        parent_identity: Some(parent),
    }))
}

/// Validate a client's schedule before preparing or starting a simulation.
/// Service and machine names coincide in the current one-service-per-machine ABI.
pub fn validate_input_program(
    configuration: &crate::config::Simulation,
    program: &[seedbox_engine_protocol::input_program::TimedInput],
) -> Result<()> {
    seedbox_engine_protocol::input_program::validate_program(
        program,
        configuration.limits.simulated_time_ns,
    )
    .map_err(Error::Setup)?;
    if program.is_empty() {
        return Ok(());
    }
    let names: Vec<_> = configuration.services.keys().cloned().collect();
    let network = NetworkConfig {
        latency_ns: configuration.network.latency_ns,
        jitter_ns: configuration.network.jitter_ns,
        bandwidth_bps: configuration.network.bandwidth_bps,
        queue_bound_ns: configuration.network.queue_bound_ns,
        links: configuration
            .network
            .links
            .iter()
            .map(|link| LinkConfig {
                from: link.from.clone(),
                to: link.to.clone(),
                latency_ns: link.latency_ns,
                jitter_ns: link.jitter_ns,
                loss_ppm: link.loss_ppm,
                bandwidth_bps: link.bandwidth_bps,
                queue_bound_ns: link.queue_bound_ns,
            })
            .collect(),
    };
    let links = seedbox_engine_core::links::Links::from_network(0, &names, &network)
        .map_err(Error::Setup)?;
    let faults = seedbox_engine_core::faults::Faults::with_plans(Vec::new(), names, links)
        .map_err(Error::Setup)?;
    for input in program {
        faults
            .validate_action_shape(&input.action)
            .map_err(|error| Error::Setup(format!("program {}: {error}", input.key)))?;
    }
    Ok(())
}

#[cfg(test)]
mod program_tests {
    use super::*;
    use seedbox_control::Action;
    use seedbox_engine_protocol::input_program::TimedInput;
    #[test]
    fn program_validates_targets_handles_order_keys_and_deadlines_without_ids() {
        let mut config = crate::config::parse(
            "services:\n  db:\n    image: example:v1\n",
            std::path::Path::new("/fixture/compose.yaml"),
            &Default::default(),
        )
        .unwrap();
        let slow = |key: &str, at_ns| TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: key.into(),
            at_ns,
            action: Action::Throttle {
                handle: "slow".into(),
                machine: "db".into(),
                speed_hz: 1_000_000_000,
            },
        };
        let heal = TimedInput {
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
            key: "heal".into(),
            at_ns: 10,
            action: Action::Heal {
                handle: "slow".into(),
            },
        };
        let entries = [slow("first", 10), heal.clone(), slow("again", 20)];
        validate_input_program(&config, &entries).unwrap();
        // Handle availability is decided against the world when due. Editing
        // an earlier action can turn a later heal into a recorded rejection.
        validate_input_program(&config, std::slice::from_ref(&heal)).unwrap();
        validate_input_program(&config, &[slow("first", 1), slow("again", 2)]).unwrap();
        assert!(validate_input_program(&config, &[slow("same", 1), slow("same", 2)]).is_err());
        assert!(validate_input_program(&config, &[slow("later", 2), slow("earlier", 1)]).is_err());
        assert!(
            validate_input_program(&config, &[slow("late", config.limits.simulated_time_ns)])
                .is_err()
        );
        config.limits.simulated_time_ns = u64::MAX;
        assert!(validate_input_program(&config, &[slow("overflow", u64::MAX - 1)]).is_err());
        let mut unknown = slow("unknown", 1);
        unknown.action = Action::Stop {
            machine: "missing".into(),
        };
        assert!(validate_input_program(&config, &[unknown]).is_err());
        config.control = Some(crate::config::Control {
            service: "db".into(),
        });
        assert!(validate_input_program(&config, &[heal]).is_ok());
    }
}
