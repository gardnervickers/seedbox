//! Session-scoped resource ownership. Each accepted submission is one entry
//! containing both the resource and its original request; nothing is read from
//! or written to disk.

mod seeds;
use seedbox_api::{
    ControlReceipt, ControlRequest, ControlState, InputSource, Metadata, Phase, Simulation,
    SimulationId, SubmitRequest,
};
use seedbox_application::store::ContentDigest;
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("server is shutting down")]
    Unavailable,
    #[error("catalogue {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error(
        "catalogue state is unavailable; restart the server before accepting further mutations"
    )]
    StorageFailed,
    #[error("worker control failed: {0}")]
    WorkerControl(String),
    #[error("invalid catalogue record: {0}")]
    Invalid(String),
    #[error("request ID was already used with different contents")]
    RequestConflict,
    #[error("another control request is awaiting acknowledgement")]
    ControlBusy,
    #[error("controls require an active, unstopped simulation with a compatible worker")]
    ControlUnavailable,
    #[error("simulation not found: {0}")]
    NotFound(SimulationId),
    #[error("simulation revision changed")]
    RevisionConflict,
    #[error("host randomness unavailable: {0}")]
    Random(String),
    #[error("checkpoint state for {checkpoint} was reclaimed when simulation {deleted} was deleted; export a checkpoint file to keep it")]
    Reclaimed {
        checkpoint: String,
        deleted: SimulationId,
    },
}

/// Owned content a deletion reclaimed, by the checkpoint it held state for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reclaimed {
    /// `checkpoint NAME of simulation ID` when an export named the content.
    pub(crate) checkpoint: Option<String>,
    pub(crate) deleted: SimulationId,
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone)]
struct Entry {
    range: Option<seeds::Range>,
    request: SubmitRequest,
    resource: Simulation,
    controls: BTreeMap<String, ControlReceipt>,
    deleting: bool,
}

pub struct Catalogue {
    entries: BTreeMap<SimulationId, Entry>,
    requests: BTreeMap<String, SimulationId>,
    pending: BTreeMap<SimulationId, String>,
    /// Owned content deletions removed this session, so a submission that
    /// names it fails with what happened instead of partway through preparation.
    reclaimed: BTreeMap<ContentDigest, Reclaimed>,
}

impl Catalogue {
    pub fn in_memory() -> Self {
        Self {
            entries: BTreeMap::new(),
            requests: BTreeMap::new(),
            pending: BTreeMap::new(),
            reclaimed: BTreeMap::new(),
        }
    }

    pub fn get(&self, id: &SimulationId) -> Result<&Simulation> {
        self.entries
            .get(id)
            .map(|e| &e.resource)
            .ok_or_else(|| Error::NotFound(id.clone()))
    }

    pub fn list(&self) -> impl Iterator<Item = &Simulation> {
        self.entries.values().map(|e| &e.resource)
    }

    /// Freeze the ownership tree before cancellation; submission uses this same
    /// catalogue lock and cannot create new descendants after the mark.
    pub fn begin_delete(&mut self, root: &SimulationId) -> Result<Vec<SimulationId>> {
        let mut ids = vec![root.clone()];
        for id in &ids {
            if let Some(entry) = self.entries.get(id) {
                let mut entry = entry.clone();
                entry.deleting = true;
                if entry
                    .resource
                    .metadata
                    .deletion_requested_at_unix_ms
                    .is_none()
                {
                    entry.resource.metadata.deletion_requested_at_unix_ms = Some(
                        SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|e| Error::Invalid(e.to_string()))?
                            .as_millis()
                            .try_into()
                            .map_err(|_| Error::Invalid("host time overflow".into()))?,
                    );
                    entry.resource.metadata.revision = entry
                        .resource
                        .metadata
                        .revision
                        .checked_add(1)
                        .ok_or_else(|| Error::Invalid("resource revision exhausted".into()))?;
                }

                if let Some(range) = &mut entry.range {
                    range.stop_requested = true;
                }
                self.entries.insert(id.clone(), entry);
            }
        }
        ids.reverse();
        Ok(ids)
    }

    /// Delete only terminal, unreferenced resources. Request identity lasts for
    /// the resource lifetime; explicit deletion permits a later resubmission.
    pub fn delete(&mut self, id: &SimulationId) -> Result<()> {
        let Some(entry) = self.entries.get(id) else {
            return Ok(());
        };
        if entry.resource.status.phase != Phase::Completed {
            return Err(Error::Invalid(
                "stop the simulation and wait for completion before deleting it".into(),
            ));
        }
        if entry.range.is_some() && !self.seed_range(id)?.complete() {
            return Err(Error::Invalid(
                "stop the seed range and wait for completion before deleting it".into(),
            ));
        }
        let request = entry.request.request_id.clone();
        self.remove_seed_member(&request)?;
        self.entries.remove(id);
        self.requests.remove(&request);
        self.pending.remove(id);
        Ok(())
    }

    /// What each resource names content in: its submitted and resolved
    /// specs.
    pub(crate) fn content(&self) -> Result<Vec<(SimulationId, serde_json::Value)>> {
        self.entries
            .iter()
            .map(|(id, entry)| {
                let value = serde_json::to_value((&entry.request.spec, &entry.resource.spec));
                Ok((
                    id.clone(),
                    value.map_err(|e| Error::Invalid(e.to_string()))?,
                ))
            })
            .collect()
    }

    /// Remember content a deletion reclaimed.
    pub(crate) fn reclaimed(
        &mut self,
        removed: impl IntoIterator<Item = (ContentDigest, Reclaimed)>,
    ) {
        self.reclaimed.extend(removed);
    }

    /// Content an upload or export put back is no longer reclaimed.
    pub(crate) fn restored(&mut self, digests: &BTreeSet<ContentDigest>) {
        for digest in digests {
            self.reclaimed.remove(digest);
        }
    }

    /// Returns (resource, newly_created). Retrying after an uncertain response
    /// returns the existing ID.
    pub fn submit(&mut self, request: SubmitRequest) -> Result<(Simulation, bool)> {
        validate_submission_request_id(&request.request_id)?;
        self.submit_inner(request, None)
    }

    fn submit_inner(
        &mut self,
        request: SubmitRequest,
        range: Option<seeds::Range>,
    ) -> Result<(Simulation, bool)> {
        let mut request = request;
        if request.metadata.name.is_empty() {
            request.metadata.name = request.spec.configuration.name.clone();
        }
        // Keep the stored and served representations identical across public schema projection.
        request = serde_json::from_value(
            serde_json::to_value(&request).map_err(|e| Error::Invalid(e.to_string()))?,
        )
        .map_err(|e| Error::Invalid(e.to_string()))?;
        validate_request_id(&request.request_id)?;
        if let Some(id) = self.requests.get(&request.request_id) {
            let entry = &self.entries[id];
            if entry.request != request
                || entry.range.as_ref().map(|r| r.count) != range.as_ref().map(|r| r.count)
            {
                return Err(Error::RequestConflict);
            }
            return Ok((entry.resource.clone(), false));
        }
        let id = loop {
            let id = random_id()?;
            if !self.entries.contains_key(&id) {
                break id;
            }
        };
        let spec = seeded_spec(&request.spec, range.as_ref())?;
        let referenced_backend = self.referenced_backend(&spec)?;
        let parent = self.validate_source(&spec)?;
        let recorded_backend = match &spec.inputs {
            InputSource::Resolved { definition } => definition
                .runtime
                .driver_backend()
                .map_err(Error::Invalid)?,
            _ => referenced_backend,
        };
        if recorded_backend
            .zip(request.options.backend)
            .is_some_and(|(recorded, requested)| recorded != requested)
        {
            return Err(Error::Invalid(
                "requested backend differs from retained backend".into(),
            ));
        }
        let backend = recorded_backend
            .or(request.options.backend)
            .unwrap_or_default();
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .ok_or_else(|| Error::Invalid("host clock cannot be represented".into()))?;
        let mut resource = Simulation::queued(
            Metadata {
                name: if request.metadata.name.is_empty() {
                    spec.configuration.name.clone()
                } else {
                    request.metadata.name.clone()
                },
                labels: request.metadata.labels.clone(),
                annotations: request.metadata.annotations.clone(),
                id: id.clone(),
                revision: 1,
                created_at_unix_ms,
                deletion_requested_at_unix_ms: None,
                parent,
                checkpoint: None,
            },
            spec,
        );
        resource.options = request.options.clone();
        resource.options.backend = Some(backend);
        let entry = Entry {
            request,
            resource: resource.clone(),
            controls: BTreeMap::new(),
            deleting: false,
            range,
        };
        self.requests
            .insert(entry.request.request_id.clone(), id.clone());
        self.entries.insert(id, entry);
        Ok((resource, true))
    }

    /// The backend recorded by the submission's source simulation, if it has one.
    fn referenced_backend(
        &self,
        spec: &seedbox_api::SimulationSpec,
    ) -> Result<Option<seedbox_spec::backend::CpuBackend>> {
        let Some(source) = source_reference(spec) else {
            return Ok(None);
        };
        let entry = self
            .entries
            .get(source)
            .ok_or_else(|| Error::NotFound(source.clone()))?;
        if entry.deleting {
            return Err(Error::Invalid("source simulation is being deleted".into()));
        }
        Ok(entry.resource.options.backend)
    }

    /// Check the submission against its input source; returns the resource's parent.
    fn validate_source(&self, spec: &seedbox_api::SimulationSpec) -> Result<Option<SimulationId>> {
        match &spec.inputs {
            InputSource::Resolved { definition } => {
                definition
                    .start
                    .validate_overrides(&definition.overrides)
                    .map_err(Error::Invalid)?;
                self.validate_unreclaimed(definition)?;
                if definition.machines.contains_key(definition.oracle_name()) {
                    return Err(Error::Invalid(
                        "oracle must have its own dedicated declaration".into(),
                    ));
                }
                Ok(None)
            }
            InputSource::Project => {
                seedbox_application::config::validate_project(&spec.configuration)
                    .map_err(|error| Error::Invalid(error.to_string()))?;
                Ok(None)
            }
            InputSource::Prepared { simulation } => {
                let original = self.get(simulation)?;
                if original.status.phase != Phase::Completed {
                    return Err(Error::Invalid(
                        "rerun requires a completed simulation".into(),
                    ));
                }
                if original.spec.configuration != spec.configuration {
                    return Err(Error::Invalid(
                        "retained input configuration differs from its source".into(),
                    ));
                }
                Ok(None)
            }
        }
    }

    /// Refuse a definition that names reclaimed content before admission.
    fn validate_unreclaimed(
        &self,
        definition: &seedbox_spec::definition::Definition,
    ) -> Result<()> {
        if self.reclaimed.is_empty() {
            return Ok(());
        }
        let mut digests = BTreeSet::new();
        crate::execution::content::digests_in(
            &serde_json::to_value(definition).map_err(|e| Error::Invalid(e.to_string()))?,
            &mut digests,
        );
        let mut reclaimed = digests
            .iter()
            .filter_map(|digest| self.reclaimed.get(digest));
        // Prefer a record that names the exported checkpoint.
        match reclaimed
            .clone()
            .find(|reclaimed| reclaimed.checkpoint.is_some())
            .or_else(|| reclaimed.next())
        {
            Some(reclaimed) => Err(Error::Reclaimed {
                checkpoint: reclaimed
                    .checkpoint
                    .clone()
                    .unwrap_or_else(|| "this definition".into()),
                deleted: reclaimed.deleted.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Compare-and-swap prevents a late worker callback from overwriting newer
    /// control state.
    pub fn update(&mut self, mut resource: Simulation) -> Result<Simulation> {
        let original = self
            .entries
            .get(&resource.metadata.id)
            .ok_or_else(|| Error::NotFound(resource.metadata.id.clone()))?;
        if original.resource.metadata.revision != resource.metadata.revision {
            return Err(Error::RevisionConflict);
        }
        if original.resource.metadata != resource.metadata {
            return Err(Error::Invalid("resource metadata is immutable".into()));
        }
        if original.resource == resource {
            return Ok(resource);
        }
        let before = &original.resource.status;
        let after = &resource.status;
        let transition_allowed = before.phase == after.phase
            || matches!(
                (before.phase, after.phase),
                (Phase::Queued, Phase::Preparing | Phase::Completed)
                    | (Phase::Preparing, Phase::Running | Phase::Completed)
                    | (Phase::Running, Phase::Finalizing | Phase::Completed)
                    | (Phase::Finalizing, Phase::Completed)
            );
        if !transition_allowed
            || after.simulated_time_ns < before.simulated_time_ns
            || (before.stop_requested && !after.stop_requested)
        {
            return Err(Error::Invalid(
                "simulation lifecycle cannot move backwards".into(),
            ));
        }
        if original.resource.spec != resource.spec
            && !matches!(
                original.resource.status.phase,
                Phase::Queued | Phase::Preparing
            )
        {
            return Err(Error::Invalid(
                "simulation inputs are immutable after preparation".into(),
            ));
        }
        if original.resource.status.phase == Phase::Completed && original.resource != resource {
            return Err(Error::Invalid("completed simulations are immutable".into()));
        }
        if original.resource.status.oracle_exit_code.is_some()
            && original.resource.status.oracle_exit_code != resource.status.oracle_exit_code
        {
            return Err(Error::Invalid(
                "an observed oracle exit is immutable".into(),
            ));
        }
        resource.metadata.revision = resource
            .metadata
            .revision
            .checked_add(1)
            .ok_or_else(|| Error::Invalid("resource revision exhausted".into()))?;
        let mut entry = Entry {
            request: original.request.clone(),
            resource: resource.clone(),
            controls: original.controls.clone(),
            deleting: original.deleting,
            range: original.range.clone(),
        };
        if resource.status.phase == Phase::Completed {
            for control in entry.controls.values_mut() {
                if control.state == ControlState::Accepted {
                    control.state = ControlState::Interrupted;
                    control.revision = 2;
                }
            }
        }
        if resource.status.phase == Phase::Completed {
            self.pending.remove(&resource.metadata.id);
        }
        self.entries.insert(resource.metadata.id.clone(), entry);
        Ok(resource)
    }

    /// Include pending and failed captures so discovery explains their current state.
    pub fn checkpoints(&self, id: &SimulationId) -> Result<Vec<ControlReceipt>> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        Ok(entry
            .controls
            .values()
            .filter(|receipt| receipt.request.command == seedbox_api::ControlCommand::Checkpoint)
            .cloned()
            .collect())
    }

    pub fn captured_checkpoint(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<seedbox_engine_protocol::host_control::CheckpointStatus> {
        use seedbox_engine_protocol::host_control::CheckpointState;
        let receipt = self.control_receipt(id, request_id)?;
        match receipt.map(|receipt| (&receipt.request.command, &receipt.state)) {
            Some((
                seedbox_api::ControlCommand::Checkpoint,
                ControlState::Acknowledged {
                    error: None,
                    checkpoint: Some(checkpoint),
                    ..
                },
            )) if checkpoint.state == CheckpointState::Captured
                && checkpoint.request_id == request_id =>
            {
                Ok(checkpoint.clone())
            }
            _ => Err(Error::Invalid(format!(
                "checkpoint {request_id} has not been captured"
            ))),
        }
    }

    pub fn control_receipt(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<Option<&ControlReceipt>> {
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        Ok(entry.controls.get(request_id))
    }

    pub fn pending_controls(&self) -> Vec<(SimulationId, ControlRequest)> {
        self.pending
            .iter()
            .filter_map(|(id, request_id)| {
                self.entries
                    .get(id)?
                    .controls
                    .get(request_id)
                    .map(|control| (id.clone(), control.request.clone()))
            })
            .collect()
    }

    pub fn accept_control(
        &mut self,
        id: &SimulationId,
        request: ControlRequest,
    ) -> Result<(ControlReceipt, bool)> {
        validate_request_id(&request.request_id)?;
        if serde_json::to_vec(&request)
            .map_err(|e| Error::Invalid(e.to_string()))?
            .len()
            > 4096
        {
            return Err(Error::Invalid("control request exceeds 4096 bytes".into()));
        }
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        check_injected(&request.command)?;
        if let Some(existing) = entry.controls.get(&request.request_id) {
            return if existing.request == request {
                Ok((existing.clone(), false))
            } else {
                Err(Error::RequestConflict)
            };
        }
        if entry.deleting
            || matches!(
                entry.resource.status.termination,
                Some(seedbox_api::ExecutionTermination::Error { .. })
            )
            || entry.resource.status.phase != Phase::Running
            || entry.resource.status.stop_requested
        {
            return Err(Error::ControlUnavailable);
        }
        if entry
            .controls
            .values()
            .any(|control| control.state == ControlState::Accepted)
        {
            return Err(Error::ControlBusy);
        }
        if entry.controls.len() >= 4096 {
            return Err(Error::Invalid(
                "simulation control request limit reached (4096)".into(),
            ));
        }
        let control = ControlReceipt {
            request,
            revision: 1,
            state: ControlState::Accepted,
        };
        let mut entry = entry.clone();
        entry
            .controls
            .insert(control.request.request_id.clone(), control.clone());
        self.pending
            .insert(id.clone(), control.request.request_id.clone());
        self.entries.insert(id.clone(), entry);
        Ok((control, true))
    }

    pub fn acknowledge_control(
        &mut self,
        id: &SimulationId,
        request_id: &str,
        state: ControlState,
    ) -> Result<ControlReceipt> {
        if matches!(state, ControlState::Accepted | ControlState::Interrupted) {
            return Err(Error::Invalid("invalid control acknowledgement".into()));
        }
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        let original = entry
            .controls
            .get(request_id)
            .ok_or_else(|| Error::Invalid("control request not found".into()))?;
        // Completion may have won the race. Preserve its conservative interrupted
        // receipt instead of rewriting history after the worker disappeared.
        if original.state != ControlState::Accepted {
            return Ok(original.clone());
        }
        let control = ControlReceipt {
            request: original.request.clone(),
            revision: 2,
            state,
        };
        let mut entry = entry.clone();
        entry
            .controls
            .insert(request_id.to_owned(), control.clone());
        self.pending.remove(id);
        self.entries.insert(id.clone(), entry);
        Ok(control)
    }
}

/// Resolve the submission's seed and project it through the public schema.
fn seeded_spec(
    spec: &seedbox_api::SimulationSpec,
    range: Option<&seeds::Range>,
) -> Result<seedbox_api::SimulationSpec> {
    let mut spec = spec.clone();
    resolve_program(&spec)?;
    // Explicit submission seed wins over the project's seed, then randomness.
    spec.seed = Some(match spec.seed.or(spec.configuration.seed) {
        Some(seed) => seed,
        None => {
            let mut bytes = [0; 8];
            random(&mut bytes)?;
            u64::from_le_bytes(bytes)
        }
    });
    spec = serde_json::from_value(
        serde_json::to_value(&spec).map_err(|e| Error::Invalid(e.to_string()))?,
    )
    .map_err(|e| Error::Invalid(e.to_string()))?;
    if let Some(range) = range {
        spec.seed
            .and_then(|seed| seed.checked_add(u64::from(range.count - 1)))
            .ok_or_else(|| Error::Invalid("seed range overflows u64".into()))?;
    }
    Ok(spec)
}

/// The existing simulation whose inputs a submission reuses.
fn source_reference(spec: &seedbox_api::SimulationSpec) -> Option<&SimulationId> {
    match &spec.inputs {
        InputSource::Project | InputSource::Resolved { .. } => None,
        InputSource::Prepared { simulation } => Some(simulation),
    }
}

fn random(bytes: &mut [u8]) -> Result<()> {
    getrandom::fill(bytes).map_err(|e| Error::Random(e.to_string()))
}

fn random_id() -> Result<SimulationId> {
    let alphabet = SimulationId::ALPHABET.as_bytes();
    let limit = 256 - 256 % alphabet.len();
    let mut id = String::with_capacity(SimulationId::LENGTH);
    while id.len() < SimulationId::LENGTH {
        let mut bytes = [0; 16];
        random(&mut bytes)?;
        for byte in bytes {
            // Rejection sampling avoids modulo bias for the alphabet length.
            if usize::from(byte) < limit {
                id.push(char::from(alphabet[usize::from(byte) % alphabet.len()]));
                if id.len() == SimulationId::LENGTH {
                    break;
                }
            }
        }
    }
    id.parse()
        .map_err(|e: seedbox_api::InvalidSimulationId| Error::Invalid(e.to_string()))
}

fn validate_request_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(Error::Invalid(
            "request ID must be 1-128 ASCII letters, digits, hyphens or underscores".into(),
        ));
    }
    Ok(())
}

fn validate_submission_request_id(id: &str) -> Result<()> {
    validate_request_id(id)?;
    if id.starts_with("seeds-") {
        return Err(Error::Invalid(
            "request ID prefix seeds- is reserved for seed range members".into(),
        ));
    }
    Ok(())
}

pub(crate) fn resolve_program(
    spec: &seedbox_api::SimulationSpec,
) -> Result<Vec<seedbox_engine_protocol::input_program::TimedInput>> {
    let program = spec
        .program
        .iter()
        .map(|input| {
            Ok(seedbox_engine_protocol::input_program::TimedInput {
                key: input.id.clone(),
                at_ns: input.at_ns,
                timeout_ns: input.timeout_ns,
                action: serde_json::from_value(input.action.clone())
                    .map_err(|error| Error::Invalid(format!("program {}: {error}", input.id)))?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    // Shape and arguments are reported before targets are compiled.
    seedbox_engine_protocol::input_program::validate_program(
        &program,
        spec.configuration.limits.simulated_time_ns,
    )
    .map_err(Error::Invalid)?;
    for input in &program {
        input
            .action
            .validate()
            .map_err(|error| Error::Invalid(format!("program {}: {error}", input.key)))?;
    }
    seedbox_application::fault_inputs::validate_input_program(&spec.configuration, &program)
        .map_err(|error| Error::Invalid(error.to_string()))?;
    Ok(program)
}

/// Refuse an injected fault with invalid arguments before it is journaled.
fn check_injected(command: &seedbox_api::ControlCommand) -> Result<()> {
    if let seedbox_api::ControlCommand::InjectFault { action } = command {
        action.validate().map_err(Error::Invalid)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
