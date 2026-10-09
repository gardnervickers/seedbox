//! World JSON generated from verified execution inputs.

use super::*;
use seedbox_engine_core::disk::DiskFaultModel;
use seedbox_engine_core::world_config::{
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
        duration_ns: request.limits.simulated_time_ns,
        end_on_exit: Some(workload_machine.clone()),
        timer_jitter_ns: request.schedule.timer_jitter_ns,
        stop_tolerance_ns: request.schedule.stop_tolerance_ns,
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
