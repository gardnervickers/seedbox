//! Retain and materialize the explicit prepared Driver input closure. Host paths
//! are launch authority only; the exported definition keeps hashes, not paths.
use crate::{
    execution::{ExecutionRequest, PreparedInputs},
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_engine_protocol::driver_world::{self as wire, Artifact, Prepared};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
};
pub(super) const MANIFEST: &str = "driver-world/manifest";
pub(super) fn validate_contract(profile: &crate::execution::ExecutionProfile) -> Result<()> {
    if profile.semantic_parameters.launch() != crate::engine::ExecutableKind::DriverWorld {
        return Err(bad(
            "Driver execution requires its recorded driver-world-v1 contract",
        ));
    }
    profile.driver_backend().map_err(bad)?;
    for (field, enabled) in [
        ("page_timer", profile.semantic_parameters.page_timer),
        ("deferred_tx", profile.semantic_parameters.deferred_tx),
    ] {
        if enabled {
            return Err(Error::Setup(format!("runtime.execution.semantic_parameters.{field}: not applicable to driver-world-v1; set {field} to false for a new Driver simulation, or replay the original retained legacy simulation")));
        }
    }
    Ok(())
}
const OBJECT: &str = "driver-world/object/";
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Closure {
    manifest: ContentDigest,
    objects: Vec<ContentDigest>,
}
fn bad(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}

/// Retain every bootstrap, filesystem and initial-disk artifact named by a
/// prepared Driver world, then its typed manifest. No image engine, builder or
/// guest is invoked. The manifest replaces source host paths with logical
/// content references and joins PreparedInputs' ordinary materialization set.
// TODO: Re-evaluate: preparation and execution are separate lifetimes; retaining
// only the current boot loses stateless future-factory authority on branch/spawn.
pub fn retain_prepared(
    inputs: &mut PreparedInputs,
    prepared: &Prepared,
    store: &ContentStore,
) -> Result<()> {
    validate_contract(&inputs.profile)?;
    let mut prepared = prepared.clone();
    let mut objects = BTreeSet::new();
    visit(&mut prepared, |artifact| {
        artifact.validate().map_err(bad)?;
        let mut source = super::checkpoint::verified(artifact)?;
        let digest = store.put_reader(&mut source)?;
        if digest.hex() != artifact.sha256 {
            return Err(bad("Driver input changed while retaining its content"));
        }
        objects.insert(digest);
        artifact.artifact = format!("asset:{}", artifact.sha256);
        Ok(())
    })?;
    let manifest = store.put(&serde_json::to_vec(&prepared)?)?;
    put_closure(
        inputs,
        Closure {
            manifest,
            objects: objects.into_iter().collect(),
        },
    )
}
fn put_closure(inputs: &mut PreparedInputs, closure: Closure) -> Result<()> {
    closure.validate()?;
    inputs
        .auxiliary
        .retain(|key, _| key != MANIFEST && !key.starts_with(OBJECT));
    inputs.auxiliary.insert(MANIFEST.into(), closure.manifest);
    for (index, digest) in closure.objects.into_iter().enumerate() {
        inputs
            .auxiliary
            .insert(format!("{OBJECT}{index:08}"), digest);
    }
    Ok(())
}
impl Closure {
    fn validate(&self) -> Result<()> {
        if self.objects.len() > 65_536 || self.objects.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err(bad(
                "Driver retained object list must be bounded, unique and ordered",
            ));
        }
        Ok(())
    }
    pub(crate) fn from_inputs(inputs: &PreparedInputs) -> Result<Option<Self>> {
        let Some(manifest) = inputs.auxiliary.get(MANIFEST) else {
            if inputs.profile.semantic_parameters.launch()
                == crate::engine::ExecutableKind::DriverWorld
            {
                return Err(bad("prepared Driver world manifest is missing"));
            }
            return Ok(None);
        };
        let mut objects: Vec<_> = inputs
            .auxiliary
            .iter()
            .filter(|(key, _)| key.starts_with(OBJECT))
            .map(|(_, digest)| digest.clone())
            .collect();
        objects.sort();
        let value = Self {
            manifest: manifest.clone(),
            objects,
        };
        value.validate()?;
        Ok(Some(value))
    }
    pub(crate) fn into_auxiliary(self) -> Result<BTreeMap<String, ContentDigest>> {
        self.validate()?;
        let mut entries = BTreeMap::from([(MANIFEST.into(), self.manifest)]);
        for (index, digest) in self.objects.into_iter().enumerate() {
            entries.insert(format!("{OBJECT}{index:08}"), digest);
        }
        Ok(entries)
    }
}

/// Return the archive content identity declared by this retained Driver world.
/// This reads only bounded retained metadata and verifies closure membership and
/// length. It never searches startup/default assets, builds inputs or executes
/// a worker. The caller materializes precisely this content in its owned session.
// TODO: Re-evaluate: the CLI must acquire runtime ownership before launch, but
// must not duplicate private manifest parsing or infer authority from provenance.
pub fn runtime_digest(inputs: &PreparedInputs, store: &ContentStore) -> Result<ContentDigest> {
    validate_contract(&inputs.profile)?;
    let closure = Closure::from_inputs(inputs)?
        .ok_or_else(|| bad("Driver preparation manifest is missing"))?;
    if store.size(&closure.manifest)? > wire::MAX_ENVELOPE_BYTES {
        return Err(bad(
            "Driver preparation manifest exceeds metadata envelope limit",
        ));
    }
    let bytes = store.read(&closure.manifest)?;
    super::checkpoint::validate_metadata(&bytes)?;
    let prepared: Prepared = serde_json::from_slice(&bytes)?;
    prepared.inventory.validate().map_err(bad)?;
    let runtime = match &prepared.inventory {
        wire::Inventory::Pv { runtime, .. } | wire::Inventory::Tcg { runtime, .. } => runtime,
    };
    let digest = ContentDigest::try_from(runtime.sha256.clone())?;
    if closure.objects.binary_search(&digest).is_err() || store.size(&digest)? != runtime.length {
        return Err(bad(
            "Driver runtime archive differs from its retained input closure",
        ));
    }
    Ok(digest)
}

pub(crate) fn request(
    request: &ExecutionRequest<'_>,
    host_control: &Path,
    backend: Option<super::DriverBackend>,
    runtime: Option<&super::binding::Binding>,
) -> Result<wire::Request> {
    validate_contract(&request.prepared.profile)?;
    let manifest = request
        .prepared
        .auxiliary
        .get(MANIFEST)
        .ok_or_else(|| bad("Driver preparation manifest is missing"))?;
    let path = request
        .artifacts
        .get(manifest)
        .ok_or_else(|| bad("Driver preparation manifest was not materialized"))?;
    let bytes = bounded(path)?;
    let mut prepared: Prepared = serde_json::from_slice(&bytes)?;
    let (actual_backend, archive) = match &prepared.inventory {
        wire::Inventory::Pv { runtime, .. } => (super::DriverBackend::Pv, runtime),
        wire::Inventory::Tcg { runtime, .. } => (super::DriverBackend::Tcg, runtime),
    };
    if backend.is_some_and(|backend| backend != actual_backend) {
        return Err(bad("Driver executor backend differs from retained factory inventory; select that inventory's backend"));
    }
    if let Some(runtime) = runtime {
        runtime.wire.validate_archive(archive).map_err(bad)?;
    }
    let boots = match &mut prepared.inventory {
        wire::Inventory::Pv { boot_archives, .. } | wire::Inventory::Tcg { boot_archives, .. } => {
            boot_archives
        }
    };
    let mut imported = BTreeSet::new();
    // Resolved inline process/memory edits are authoritative, not the launch
    // fields in the original preparation manifest. Binary filesystem identity
    // likewise comes from the current resolved machine record.
    for machine in std::iter::once(&mut prepared.oracle).chain(&mut prepared.machines) {
        let image = request.prepared.images.get(&machine.name).ok_or_else(|| {
            bad(format!(
                "resolved Driver launch is missing for {}",
                machine.name
            ))
        })?;
        let process = wire::Process {
            argv: image.launch.argv.clone(),
            environment: image.launch.environment.clone(),
            working_directory: image.launch.working_dir.clone(),
            user: image.launch.user.clone(),
        };
        let process_changed = machine.process != process;
        machine.process = process;
        machine.memory_bytes =
            u64::from(request.prepared.profile.semantic_parameters.memory_mb) * 1024 * 1024;
        let digest = request
            .prepared
            .guests
            .get(&machine.name)
            .ok_or_else(|| bad("resolved Driver filesystem is missing"))?;
        let path = request
            .artifacts
            .get(digest)
            .ok_or_else(|| bad("resolved Driver filesystem was not materialized"))?;
        let current = Artifact {
            sha256: digest.hex().into(),
            length: fs::metadata(path)?.len(),
            artifact: format!("asset:{}", digest.hex()),
        };
        let mut signature = Vec::new();
        fs::File::open(path)?.take(6).read_to_end(&mut signature)?;
        if super::boot::complete(&signature) {
            let paired = boots
                .get(&machine.name)
                .is_some_and(|boot| boot.sha256 == current.sha256 && boot.length == current.length);
            if paired && process_changed {
                // A matching retained boot proves the original application pair.
                // Rebuild only for an actual inline launch edit, never for an
                // independently replaced complete archive.
                boots.remove(&machine.name);
            } else {
                boots.insert(machine.name.clone(), current);
                if !paired {
                    imported.insert(machine.name.clone());
                }
            }
        } else {
            // Older private Driver manifests used a tar filesystem as their
            // guest reference. Keep that contract rather than treating it as
            // Linux boot bytes or requiring a newly introduced paired field.
            boots.remove(&machine.name);
            machine.filesystem = current;
        }
    }
    prepared.network.latency_ns = request.network.latency_ns;
    prepared.network.jitter_ns = request.network.jitter_ns;
    prepared.network.bandwidth_bps = request.network.bandwidth_bps;
    prepared.network.queue_bound_ns = request.network.queue_bound_ns;
    prepared.network.links = request
        .network
        .links
        .iter()
        .map(|link| wire::Link {
            from: link.from.clone(),
            to: link.to.clone(),
            latency_ns: link.latency_ns,
            jitter_ns: link.jitter_ns,
            loss_ppm: link.loss_ppm,
            bandwidth_bps: link.bandwidth_bps,
            queue_bound_ns: link.queue_bound_ns,
        })
        .collect();
    let controller = request.control.map(|control| {
        request
            .prepared
            .placement
            .get(&control.service)
            .cloned()
            .unwrap_or_else(|| control.service.clone())
    });
    if controller
        .as_ref()
        .is_some_and(|name| name != &prepared.oracle.name)
    {
        return Err(bad(
            "Driver controller must name the retained oracle, not another retained machine",
        ));
    }
    {
        let policy = match &mut prepared.inventory {
            wire::Inventory::Pv { policy, .. } | wire::Inventory::Tcg { policy, .. } => policy,
        };
        let resolved = seedbox_machine::definition::BackendPolicy {
            seed: request.seed,
            timer_jitter_ns: request.schedule.timer_jitter_ns,
            stop_tolerance_ns: request.schedule.stop_tolerance_ns,
            torn_writes: request.prepared.profile.semantic_parameters.torn_writes,
            full_state: request.prepared.profile.witness_mode
                == crate::execution::WitnessMode::FullState,
            seeded_pv_speed: policy.as_ref().is_some_and(|policy| policy.seeded_pv_speed),
            boot_end_evidence: false,
            controller,
        };
        resolved.validate().map_err(bad)?;
        *policy = Some(super::policy::encode(&resolved).map_err(bad)?);
    }
    let epoch = request
        .prepared
        .wall_clock_epoch
        .ok_or_else(|| bad("Driver realtime epoch was not resolved"))?;
    match &mut prepared.inventory {
        wire::Inventory::Pv { epoch_seconds, .. } | wire::Inventory::Tcg { epoch_seconds, .. } => {
            *epoch_seconds = epoch
        }
    }
    visit(&mut prepared, |artifact| {
        let digest = ContentDigest::try_from(artifact.sha256.clone())?;
        let path = runtime
            .and_then(|runtime| runtime.files.get(&digest))
            .or_else(|| request.artifacts.get(&digest))
            .ok_or_else(|| {
                bad(format!(
                    "Driver runtime input {} was not retained/materialized",
                    artifact.sha256
                ))
            })?;
        artifact.artifact = path
            .to_str()
            .ok_or_else(|| bad("Driver materialized path is not UTF-8"))?
            .into();
        super::checkpoint::verified(artifact)?;
        Ok(())
    })?;
    // Imported views are attempt-owned files retained under the same output
    // lifetime as the request; guest paths are never extracted onto the host.
    let boots = match &prepared.inventory {
        wire::Inventory::Pv { boot_archives, .. } | wire::Inventory::Tcg { boot_archives, .. } => {
            boot_archives
        }
    };
    for machine in std::iter::once(&mut prepared.oracle).chain(&mut prepared.machines) {
        if imported.contains(&machine.name) {
            let boot = boots
                .get(&machine.name)
                .ok_or_else(|| bad("complete boot authority missing during import"))?;
            let mut file = super::checkpoint::verified(boot)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            machine.filesystem = super::boot::retain_view(&bytes, request.output)?;
        }
    }
    let workers = request
        .performance_parameters
        .get("SEEDBOX_MACHINE_WORKERS")
        .map_or(Ok(1), |value| {
            value
                .parse::<usize>()
                .map_err(|_| bad("invalid Driver machine worker count"))
        })?;
    if !(1..=64).contains(&workers) {
        return Err(bad("Driver machine worker count must be in 1..64"));
    }
    Ok(wire::Request {
        prepared,
        runtime_binding: runtime.map(|runtime| runtime.wire.clone()),
        seed: request.seed,
        program: request.program.to_vec(),
        simulated_time_ns: request.limits.simulated_time_ns,
        output: request
            .output
            .join(crate::engine_adapter::MACHINE_OUTPUT_DIR),
        machine_workers: workers,
        start_paused: request.start_paused,
        full_state: request.prepared.profile.witness_mode
            == crate::execution::WitnessMode::FullState,
        host_control: host_control.into(),
        observations: request.observations.map(Path::to_path_buf),
        replay_interventions: request.replay_interventions.cloned(),
        resume_checkpoint: request.resume_checkpoint.map(Path::to_path_buf),
    })
}
pub(super) fn bounded(path: &Path) -> Result<Vec<u8>> {
    let file = fs::File::open(path)?;
    if file.metadata()?.len() > wire::MAX_ENVELOPE_BYTES {
        return Err(bad("Driver request metadata exceeds limit"));
    }
    let mut bytes = Vec::new();
    file.take(wire::MAX_ENVELOPE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > wire::MAX_ENVELOPE_BYTES {
        return Err(bad("Driver request metadata exceeds limit"));
    }
    super::checkpoint::validate_metadata(&bytes)?;
    Ok(bytes)
}
pub(super) fn visit_inventory(
    inventory: &mut wire::Inventory,
    apply: impl FnMut(&mut Artifact) -> Result<()>,
) -> Result<()> {
    super::inventory::visit(inventory, apply)
}
fn visit(
    prepared: &mut Prepared,
    mut apply: impl FnMut(&mut Artifact) -> Result<()>,
) -> Result<()> {
    visit_inventory(&mut prepared.inventory, &mut apply)?;
    for machine in std::iter::once(&mut prepared.oracle)
        .chain(&mut prepared.machines)
        .chain(&mut prepared.spawnable)
    {
        apply(&mut machine.filesystem)?;
        if let Some(wire::Disk::Image { contents }) = &mut machine.disk {
            apply(contents)?;
        }
    }
    Ok(())
}
