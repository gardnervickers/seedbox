//! World JSON generated from verified execution inputs.

use super::*;

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
pub fn world(request: &ExecutionRequest<'_>) -> Result<Value> {
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
    let memory: u32 = request
        .prepared
        .profile
        .semantic_parameters
        .get("memory_mb")
        .ok_or_else(|| Error::Integrity("profile missing memory_mb".into()))?
        .parse()
        .map_err(|_| Error::Integrity("invalid memory_mb".into()))?;
    let artifact = |digest: &ContentDigest| {
        let path = request.artifacts.get(digest).ok_or_else(|| {
            Error::Integrity(format!("missing verified artifact {}", digest.hex()))
        })?;
        Ok::<_, Error>(std::path::absolute(path)?)
    };
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
    let control = request.control.map(|c| -> Result<Value> {
        let machine = request.prepared.placement.get(&c.service).ok_or_else(|| Error::Execution("controller placement missing".into()))?;
        let latency_ns = control_latency(request)?;
        Ok(json!({"machine":machine,"latency_ns":latency_ns,"placement":request.prepared.placement}))
    }).transpose()?;
    let machines = request
        .prepared
        .guests
        .iter()
        .map(|(name, digest)| {
            let mut machine = json!({"name":name,"initrd":artifact(digest)?});
            // A machine gets a disk when a service placed on it mounts a volume.
            let volumes = request.prepared.placement.iter().any(|(service, machine)| {
                machine == name
                    && request
                        .prepared
                        .images
                        .get(service)
                        .is_some_and(|image| !image.launch.disk_volumes.is_empty())
            });
            if volumes {
                machine["disk"] = json!({"size_mib": VOLUME_DISK_MIB});
            }
            Ok(machine)
        })
        .collect::<Result<Vec<_>>>()?;
    let links = request.network.links.iter().map(|link| {
        let machine = |service: &str| request.prepared.placement.get(service).ok_or_else(||Error::Integrity(format!("missing link placement for {service}")));
        let mut policy = json!({"from":machine(&link.from)?,"to":machine(&link.to)?,"latency_ns":link.latency_ns,"jitter_ns":link.jitter_ns,"loss_ppm":link.loss_ppm});
        if let Some(rate) = link.bandwidth_bps { policy["bandwidth_bps"] = json!(rate); }
        if let Some(bound) = link.queue_bound_ns { policy["queue_bound_ns"] = json!(bound); }
        Ok(policy)
    }).collect::<Result<Vec<_>>>()?;
    let mut network =
        json!({"latency_ns":request.network.latency_ns,"jitter_ns":request.network.jitter_ns});
    if let Some(rate) = request.network.bandwidth_bps {
        network["bandwidth_bps"] = json!(rate);
    }
    if let Some(bound) = request.network.queue_bound_ns {
        network["queue_bound_ns"] = json!(bound);
    }
    // Absent unless used, so a world without link overrides is unchanged.
    if !links.is_empty() {
        network["links"] = json!(links);
    }
    let mut world = json!({
        "kernel":artifact(&request.prepared.kernel)?, "mem_mb":memory,
        "duration_ns":request.limits.simulated_time_ns,"end_on_exit":workload_machine,
        "timer_jitter_ns":request.schedule.timer_jitter_ns,"stop_tolerance_ns":request.schedule.stop_tolerance_ns,
        "network":network,
        "machines":machines,"control":control,
        "control_channel":"virtio-console-v1",
    });
    let lookahead = request
        .network
        .latency_ns
        .saturating_sub(request.network.jitter_ns);
    if let Some(skew) = crate::engine::arm_launch_skew(&request.prepared.profile, lookahead)? {
        world["launch_skew_ns"] = json!(skew);
    }
    if recorded_torn_writes(&request.prepared.profile)? {
        world[DISK_LOSS] = match request.prepared.profile.semantic_parameters.get(DISK_LOSS) {
            Some(model) => json!(model), // The retained historical worker owns its disk semantics.
            None => json!({"sector_bytes": 512, "survival_probability": 0.5}),
        };
    }
    if let Some(checkpoint) = checkpoint_metadata(request, &world)? {
        world["checkpoint"] = checkpoint;
    }
    Ok(world)
}

pub(super) fn checkpoint_metadata(
    request: &ExecutionRequest<'_>,
    world: &Value,
) -> Result<Option<Value>> {
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
        &checkpoint::identity_timebase(
            recorded_timebase(&request.prepared.profile)?.name(),
            recorded_deferred_tx(&request.prepared.profile)?,
        ),
        recorded_page_timer(&request.prepared.profile),
        |path| {
            request
                .artifacts
                .iter()
                .find_map(|(digest, artifact)| {
                    std::path::absolute(artifact)
                        .ok()
                        .filter(|p| p == Path::new(path))
                        .map(|_| digest.hex().to_owned())
                })
                .ok_or_else(|| format!("checkpoint artifact {path} is not verified"))
        },
    )
    .map_err(Error::Integrity)?;
    Ok(Some(
        json!({"at_ns":at,"branch_seed":seed,"parent_identity":parent,
        "branch_identity":checkpoint::branch_identity(&parent,at,seed)}),
    ))
}
