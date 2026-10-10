//! Durable resource ownership. Each accepted submission is one atomically
//! replaced record containing both the resource and its original request. The
//! request index can therefore be rebuilt without a separate journal.

mod seeds;
use seedbox_api::{
    ControlReceipt, ControlRequest, ControlState, InputSource, Metadata, Phase, Simulation,
    SimulationId, SubmitRequest,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const RECORD_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("server is shutting down")]
    Unavailable,
    #[error("catalogue {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("catalogue is already owned by another server: {0}")]
    AlreadyOwned(PathBuf),
    #[error("catalogue storage failed; restart the server before accepting further mutations")]
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
}

type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    #[serde(default)]
    range: Option<seeds::Range>,
    request: SubmitRequest,
    resource: Simulation,
    #[serde(default)]
    controls: BTreeMap<String, ControlReceipt>,
    #[serde(default)]
    deleted_checkpoints: BTreeSet<String>,
    #[serde(default)]
    deleting: bool,
}

pub struct Catalogue {
    directory: PathBuf,
    // Dropping the catalogue releases ownership. Never unlink the lock file:
    // that would let another process lock a different inode at the same path.
    _lock: File,
    entries: BTreeMap<SimulationId, Entry>,
    requests: BTreeMap<String, SimulationId>,
    storage_failed: bool,
    pending: BTreeMap<SimulationId, String>,
}

impl Catalogue {
    pub fn open(directory: &Path) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        at(directory, builder.create(directory))?;
        let metadata = at(directory, fs::symlink_metadata(directory))?;
        // SAFETY: geteuid has no pointer arguments or preconditions.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(Error::Invalid(format!(
                "{} must be a private directory owned by the server user",
                directory.display()
            )));
        }
        let lock_path = directory.join("catalogue.lock");
        let lock = at(
            &lock_path,
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&lock_path),
        )?;
        // SAFETY: the file descriptor remains live for the catalogue lifetime.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let source = io::Error::last_os_error();
            if source.kind() == io::ErrorKind::WouldBlock {
                return Err(Error::AlreadyOwned(directory.to_owned()));
            }
            return Err(Error::Io {
                path: lock_path,
                source,
            });
        }
        let mut catalogue = Self {
            directory: directory.to_owned(),
            _lock: lock,
            entries: BTreeMap::new(),
            requests: BTreeMap::new(),
            storage_failed: false,
            pending: BTreeMap::new(),
        };
        for item in at(directory, fs::read_dir(directory))? {
            let path = at(directory, item)?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let id: SimulationId = path
                .file_stem()
                .and_then(|s| s.to_str())
                .ok_or_else(|| Error::Invalid("invalid record filename".into()))?
                .parse()
                .map_err(|e: seedbox_api::InvalidSimulationId| Error::Invalid(e.to_string()))?;
            let file = at(
                &path,
                OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(&path),
            )?;
            let mut bytes = Vec::new();
            at(&path, file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes))?;
            if bytes.len() as u64 > RECORD_LIMIT {
                return Err(Error::Invalid(format!(
                    "{} exceeds record limit",
                    path.display()
                )));
            }
            let entry: Entry = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Invalid(format!("{}: {e}", path.display())))?;
            if let Some(range) = &entry.range {
                range.validate(entry.resource.spec.seed)?;
            }
            let mut pending = 0;
            for (request_id, control) in &entry.controls {
                validate_request_id(request_id)?;
                let accepted = control.state == ControlState::Accepted;
                pending += usize::from(accepted);
                if *request_id != control.request.request_id
                    || control.revision != if accepted { 1 } else { 2 }
                    || (accepted
                        && !matches!(
                            entry.resource.status.phase,
                            Phase::Running | Phase::Finalizing
                        ))
                {
                    return Err(Error::Invalid("inconsistent control receipt".into()));
                }
            }
            if pending > 1 || entry.controls.len() > 4096 {
                return Err(Error::Invalid("control bounds exceeded".into()));
            }
            if entry.resource.metadata.id != id || entry.resource.metadata.revision == 0 {
                return Err(Error::Invalid(format!(
                    "{} has inconsistent identity",
                    path.display()
                )));
            }
            if catalogue
                .requests
                .insert(entry.request.request_id.clone(), id.clone())
                .is_some()
            {
                return Err(Error::Invalid("duplicate submission request ID".into()));
            }
            if let Some(control) = entry
                .controls
                .values()
                .find(|control| control.state == ControlState::Accepted)
            {
                catalogue
                    .pending
                    .insert(id.clone(), control.request.request_id.clone());
            }
            catalogue.entries.insert(id, entry);
        }
        Ok(catalogue)
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

    pub fn pending_deletions(&self) -> Vec<SimulationId> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.deleting)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Freeze the ownership tree before cancellation; submission uses this same
    /// catalogue lock and cannot create new descendants after the mark.
    pub fn begin_delete(&mut self, root: &SimulationId) -> Result<Vec<SimulationId>> {
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
        let mut ids = vec![root.clone()];
        let mut cursor = 0;
        while cursor < ids.len() {
            let parent = ids[cursor].clone();
            for (id, entry) in &self.entries {
                if entry.resource.metadata.parent.as_ref() == Some(&parent) && !ids.contains(id) {
                    ids.push(id.clone());
                }
            }
            cursor += 1;
        }
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
                self.persist(&entry)?;
                self.entries.insert(id.clone(), entry);
            }
        }
        ids.reverse();
        Ok(ids)
    }

    /// Delete only terminal, unreferenced resources. Request identity lasts for
    /// the resource lifetime; explicit deletion permits a later resubmission.
    pub fn delete(&mut self, id: &SimulationId) -> Result<()> {
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
        let Some(entry) = self.entries.get(id) else {
            return Ok(());
        };
        if entry.resource.status.phase != Phase::Completed {
            return Err(Error::Invalid(
                "stop the simulation and wait for completion before deleting it".into(),
            ));
        }
        if self
            .entries
            .values()
            .any(|entry| entry.resource.metadata.parent.as_ref() == Some(id))
        {
            return Err(Error::Invalid(
                "simulation is referenced by another simulation; delete its dependents first"
                    .into(),
            ));
        }
        if entry.range.is_some() && !self.seed_range(id)?.complete() {
            return Err(Error::Invalid(
                "stop the seed range and wait for completion before deleting it".into(),
            ));
        }
        let request = entry.request.request_id.clone();
        self.remove_seed_member(&request)?;
        let path = self.directory.join(format!("{id}.json"));
        if let Err(error) = at(&path, fs::remove_file(&path)) {
            self.storage_failed = true;
            return Err(error);
        }
        self.entries.remove(id);
        self.requests.remove(&request);
        self.pending.remove(id);
        Ok(())
    }

    /// Returns (resource, newly_created). Retrying after an uncertain response
    /// returns the existing ID, including after server restart.
    pub fn submit(&mut self, request: SubmitRequest) -> Result<(Simulation, bool)> {
        self.submit_inner(request, None)
    }

    fn submit_inner(
        &mut self,
        request: SubmitRequest,
        range: Option<seeds::Range>,
    ) -> Result<(Simulation, bool)> {
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
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
        let mut spec = request.spec.clone();
        // Explicit submission seed wins over the project's seed, then randomness.
        spec.seed = Some(match spec.seed.or(spec.configuration.seed) {
            Some(seed) => seed,
            None => {
                let mut bytes = [0; 8];
                random(&mut bytes)?;
                u64::from_le_bytes(bytes)
            }
        });
        if let Some(range) = &range {
            spec.seed
                .and_then(|seed| seed.checked_add(u64::from(range.count - 1)))
                .ok_or_else(|| Error::Invalid("seed range overflows u64".into()))?;
        }
        let reference = match &spec.inputs {
            InputSource::Project => None,
            InputSource::Retained { simulation }
            | InputSource::Prepared { simulation }
            | InputSource::Checkpoint { simulation, .. } => Some(simulation),
        };
        if reference.is_some_and(|id| self.entries.get(id).is_some_and(|entry| entry.deleting)) {
            return Err(Error::Invalid("source simulation is being deleted".into()));
        }
        let parent = match &spec.inputs {
            InputSource::Project => {
                let config = &spec.configuration;
                if config.check.exit_code != 0 {
                    return Err(Error::Invalid(
                        "oracle success is always exit code zero".into(),
                    ));
                }

                if config.machine.page_timer
                    || config.machine.deferred_tx
                    || config.schedule.stop_tolerance_ns != 0
                {
                    return Err(Error::Invalid("new simulations use default page timer, deferred TX and stop tolerance settings".into()));
                }
                None
            }
            InputSource::Checkpoint {
                simulation,
                checkpoint,
            } => {
                validate_request_id(checkpoint)?;
                let original = self.get(simulation)?;
                if original.spec.configuration != spec.configuration
                    || original.spec.seed != spec.seed
                {
                    return Err(Error::Invalid(
                        "checkpoint requires its source configuration and seed".into(),
                    ));
                }
                self.captured_checkpoint(simulation, checkpoint)?;
                Some(simulation.clone())
            }
            InputSource::Retained { simulation } | InputSource::Prepared { simulation } => {
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
                Some(simulation.clone())
            }
        };
        let created_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok())
            .ok_or_else(|| Error::Invalid("host clock cannot be represented".into()))?;
        let resource = Simulation::queued(
            Metadata {
                id: id.clone(),
                revision: 1,
                created_at_unix_ms,
                deletion_requested_at_unix_ms: None,
                parent,
                checkpoint: match &spec.inputs {
                    InputSource::Checkpoint { checkpoint, .. } => Some(checkpoint.clone()),
                    _ => None,
                },
            },
            spec,
        );
        let entry = Entry {
            request,
            resource: resource.clone(),
            controls: BTreeMap::new(),
            deleted_checkpoints: BTreeSet::new(),
            deleting: false,
            range,
        };
        self.persist(&entry)?;
        self.requests
            .insert(entry.request.request_id.clone(), id.clone());
        self.entries.insert(id, entry);
        Ok((resource, true))
    }

    /// Compare-and-swap prevents a late worker callback from overwriting newer
    /// control state. Publication happens only after the returned revision is durable.
    pub fn update(&mut self, mut resource: Simulation) -> Result<Simulation> {
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
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
            deleted_checkpoints: original.deleted_checkpoints.clone(),
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
        self.persist(&entry)?;
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
            .filter(|receipt| {
                receipt.request.command == seedbox_api::ControlCommand::Checkpoint
                    && !entry
                        .deleted_checkpoints
                        .contains(&receipt.request.request_id)
            })
            .cloned()
            .collect())
    }

    /// Tombstone under the same lock as branch admission, retaining the original
    /// control receipt for idempotent request retries and audit.
    pub fn delete_checkpoint(&mut self, id: &SimulationId, name: &str) -> Result<()> {
        validate_request_id(name)?;
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        if entry.resource.status.phase != Phase::Completed {
            return Err(Error::Invalid(
                "checkpoint deletion requires a completed simulation".into(),
            ));
        }
        if entry.deleted_checkpoints.contains(name) {
            return Ok(());
        }
        self.captured_checkpoint(id, name)?;
        if self.entries.values().any(|candidate| {
            candidate.resource.metadata.parent.as_ref() == Some(id)
                && candidate.resource.metadata.checkpoint.as_deref() == Some(name)
        }) {
            return Err(Error::Invalid(format!(
                "checkpoint {name} is referenced by a branch"
            )));
        }
        let mut entry = entry.clone();
        entry.deleted_checkpoints.insert(name.to_owned());
        self.persist(&entry)?;
        self.entries.insert(id.clone(), entry);
        Ok(())
    }

    pub fn captured_checkpoint(
        &self,
        id: &SimulationId,
        request_id: &str,
    ) -> Result<seedbox_engine_protocol::host_control::CheckpointStatus> {
        use seedbox_engine_protocol::host_control::CheckpointState;
        let entry = self
            .entries
            .get(id)
            .ok_or_else(|| Error::NotFound(id.clone()))?;
        if entry.deleted_checkpoints.contains(request_id) {
            return Err(Error::Invalid(format!(
                "checkpoint {request_id} was deleted"
            )));
        }

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
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
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
        if let Some(existing) = entry.controls.get(&request.request_id) {
            return if existing.request == request {
                Ok((existing.clone(), false))
            } else {
                Err(Error::RequestConflict)
            };
        }
        if entry.deleting
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
        self.persist(&entry)?;
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
        if self.storage_failed {
            return Err(Error::StorageFailed);
        }
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
        self.persist(&entry)?;
        self.pending.remove(id);
        self.entries.insert(id.clone(), entry);
        Ok(control)
    }

    fn persist(&mut self, entry: &Entry) -> Result<()> {
        let bytes = serde_json::to_vec(entry).map_err(|e| Error::Invalid(e.to_string()))?;
        if bytes.len() as u64 > RECORD_LIMIT {
            return Err(Error::Invalid(
                "simulation exceeds record size limit".into(),
            ));
        }
        let path = self
            .directory
            .join(format!("{}.json", entry.resource.metadata.id));
        let result = (|| -> io::Result<()> {
            let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
            temporary.write_all(&bytes)?;
            temporary.persist(&path).map_err(|e| e.error)?;
            Ok(())
        })();
        if result.is_err() {
            // Stop accepting mutations after a storage failure; preserve the
            // last published in-memory state for inspection.
            self.storage_failed = true;
        }
        at(&path, result)
    }
}

fn at<T>(path: &Path, result: io::Result<T>) -> Result<T> {
    result.map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })
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
