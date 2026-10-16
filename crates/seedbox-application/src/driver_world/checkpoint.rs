//! Transactional service checkpoints. The only publication point is the final
//! manifest rename, after every file and containing directory has been synced.
mod metadata;
pub(super) fn validate_metadata(bytes: &[u8]) -> Result<()> {
    metadata::validate(bytes).map_err(Error::from)
}
use crate::{Error, Result};
use seedbox_driver::driver::{Checkpoint, Driver, Event, SnapshotReference};
use seedbox_engine_protocol::driver_world::{self as wire, Artifact, Inventory, State};
use seedbox_machine::machine::Machine;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, BufReader, BufWriter, Read, Seek, Write},
    path::{Path, PathBuf},
};

fn integrity(message: impl Into<String>) -> Error {
    Error::Integrity(message.into())
}

/// Hold exact already-drained service state alongside the Driver. Prepared
/// interventions must commit first; an admitted pre-effect hold is Driver state.
// TODO: Re-evaluate: these two service gates are not backend snapshot state;
// publishing a capture there would sever the durable prepare/commit handshake.
pub(super) struct Capture<'a> {
    pub inventory: &'a Inventory,
    pub state: &'a mut State,
    pub prepared: bool,
}

/// A refused capture preserves previously valid Driver/service usability. A
/// publication failure has lost a drained suffix and MUST terminate the worker.
/// Neither outcome recovers a Driver already poisoned by Machine::run.
#[derive(Debug)]
pub(super) enum CaptureFailure {
    Refused(Error),
    Publication(Error),
}
impl std::fmt::Display for CaptureFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
        Self::Refused(error)=>write!(f,"checkpoint refused: {error}"),
        Self::Publication(error)=>write!(f,"checkpoint publication failed after draining Driver events; stop execution: {error}"),
    }
    }
}
impl std::error::Error for CaptureFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match self {
            Self::Refused(error) | Self::Publication(error) => error,
        })
    }
}
pub(super) type CaptureResult<T> = std::result::Result<T, CaptureFailure>;

/// Production implementation shares this internal generic seam with host mocks.
/// The mapper owns each native snapshot, streams it, and drops it before the
/// Driver captures the next machine. It never buffers all native snapshots.
pub(super) fn capture<M>(
    driver: &mut Driver<M>,
    capture: Capture<'_>,
    output: &Path,
    request_id: &str,
    mut publish: impl FnMut(&mut State, Event) -> Result<()>,
    mut write_snapshot: impl FnMut(M::Snapshot, &mut BufWriter<File>) -> Result<()>,
) -> CaptureResult<PathBuf>
where
    M: Machine + Send + 'static,
    M::Context: 'static,
    M::Error: Send + std::fmt::Display + 'static,
{
    let mut publication_failed = false;
    let result: Result<PathBuf> = (|| {
        let playback = driver
            .with_barrier(|barrier| Ok(barrier.playback()))
            .map_err(|e| integrity(e.to_string()))?;
        if capture.prepared {
            return Err(integrity(
                "Driver checkpoint requires no prepared intervention",
            ));
        }
        if request_id.is_empty()
            || request_id.len() > 128
            || !request_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            return Err(integrity("invalid Driver checkpoint request identity"));
        }
        capture.inventory.validate().map_err(integrity)?;
        capture.state.world.validate().map_err(integrity)?;
        capture
            .state
            .world
            .input_program
            .validate_at(playback.position.at.as_nanos())
            .map_err(integrity)?;
        // Cheap refusal and immutable factory-authority checks precede draining.
        // The manifest is the ONLY publication marker. Each attempt owns a unique
        // bodies directory, so abandoned attempts cannot block this request ID or
        // be replaced by an exists-then-rename race. Session reclamation owns orphans.
        let output = fs::canonicalize(output)
            .map_err(|e| integrity(format!("checkpoint output {}: {e}", output.display())))?;
        let manifest = output.join(format!("checkpoint-{request_id}.json"));
        if manifest.exists() {
            return Err(integrity("Driver checkpoint identity already published"));
        }
        let staging = tempfile::Builder::new()
            .prefix(&format!("checkpoint-{request_id}-bodies-"))
            .tempdir_in(&output)?;
        let final_dir = staging.path().to_path_buf();
        let inventory = copy_inventory(capture.inventory, staging.path(), &final_dir)?;
        // Publish exactly once before native capture. A failed publisher has lost
        // the drained suffix: distinguish it from a recoverable capture refusal.
        for event in driver.drain_events() {
            if let Err(error) = publish(capture.state, event) {
                publication_failed = true;
                return Err(error);
            }
        }
        let position = playback.position;
        capture.state.world.position = seedbox_engine_protocol::host_control::BoundaryPosition {
            boundary_index: position.boundary_index,
            completed_rounds: position.completed_quanta,
            simulated_time_ns: position.at.as_nanos(),
        };
        let mut artifacts = Vec::new();
        let captured = driver
            .checkpoint_map(|snapshot| {
                let name = format!("machine-{}.bin", artifacts.len());
                let staged = staging.path().join(&name);
                let artifact = write_artifact(&staged, &final_dir.join(&name), |file| {
                    write_snapshot(snapshot, file)
                })?;
                let reference = snapshot_reference(&artifact)?;
                artifacts.push(artifact);
                Ok::<_, Error>(reference)
            })
            .map_err(|error| integrity(format!("Driver checkpoint: {error}")))?;
        let metadata = write_artifact(
            &staging.path().join("driver.bin"),
            &final_dir.join("driver.bin"),
            |file| {
                captured
                    .write_to(file)
                    .map_err(|e| integrity(e.to_string()))
            },
        )?;
        let envelope = wire::Checkpoint {
            contract: wire::CONTRACT.into(),
            version: wire::ENVELOPE_VERSION,
            inventory,
            driver: metadata,
            machines: artifacts,
            state: capture.state.clone(),
        };
        envelope.validate().map_err(integrity)?;
        let mut temporary_manifest = tempfile::NamedTempFile::new_in(&output)?;
        serde_json::to_writer_pretty(temporary_manifest.as_file_mut(), &envelope)?;
        if temporary_manifest.as_file().metadata()?.len() > wire::MAX_ENVELOPE_BYTES {
            return Err(integrity(
                "Driver checkpoint service envelope exceeds limit",
            ));
        }
        temporary_manifest.as_file_mut().rewind()?;
        metadata::validate_reader(temporary_manifest.as_file_mut()).map_err(|e| {
            integrity(format!(
                "Driver checkpoint metadata cannot be restored: {e}"
            ))
        })?;
        temporary_manifest.as_file().sync_all()?;
        File::open(staging.path())?.sync_all()?;
        // The guard remains armed through durable manifest publication. Bodies
        // were allocated exclusively under this unique owned attempt directory.
        let mut publication = PublicationGuard {
            bodies: final_dir,
            manifest: manifest.clone(),
            manifest_owned: false,
            committed: false,
        };
        File::open(&output)?.sync_all()?;
        temporary_manifest
            .persist_noclobber(&manifest)
            .map_err(|e| Error::from(e.error))?;
        publication.manifest_owned = true;
        File::open(&output)?.sync_all()?;
        publication.committed = true;
        let _ = staging.keep();
        Ok(manifest)
    })();
    result.map_err(|error| {
        if publication_failed {
            CaptureFailure::Publication(error)
        } else {
            CaptureFailure::Refused(error)
        }
    })
}
struct PublicationGuard {
    bodies: PathBuf,
    manifest: PathBuf,
    manifest_owned: bool,
    committed: bool,
}
impl Drop for PublicationGuard {
    fn drop(&mut self) {
        if !self.committed {
            if self.manifest_owned {
                let _ = fs::remove_file(&self.manifest);
            }
            let _ = fs::remove_dir_all(&self.bodies);
        }
    }
}
fn write_artifact(
    path: &Path,
    published: &Path,
    write: impl FnOnce(&mut BufWriter<File>) -> Result<()>,
) -> Result<Artifact> {
    let mut file = BufWriter::new(File::options().write(true).create_new(true).open(path)?);
    write(&mut file)?;
    file.flush()?;
    file.get_ref().sync_all()?;
    drop(file);
    describe(path, published)
}
fn describe(path: &Path, published: &Path) -> Result<Artifact> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let mut hash = Sha256::new();
    let count = io::copy(
        &mut (&mut file).take(length.saturating_add(1)),
        &mut HashWriter(&mut hash),
    )?;
    if count != length {
        return Err(integrity("Driver artifact changed while hashing"));
    }
    Ok(Artifact {
        sha256: hex::encode(hash.finalize()),
        length,
        artifact: published
            .to_str()
            .ok_or_else(|| integrity("Driver artifact path is not UTF-8"))?
            .into(),
    })
}
struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn snapshot_reference(artifact: &Artifact) -> Result<SnapshotReference> {
    let mut digest = [0; 32];
    hex::decode_to_slice(&artifact.sha256, &mut digest).map_err(|e| integrity(e.to_string()))?;
    Ok(SnapshotReference::new(digest, artifact.length))
}

/// Verify original bytes and length before handing any content to a decoder.
/// Retain this exact open descriptor, rewound; do not reopen by an unverified
/// path after the integrity check. This is not an authenticity/provenance gate.
pub(super) fn verified(artifact: &Artifact) -> Result<BufReader<File>> {
    artifact.validate().map_err(integrity)?;
    let mut file = File::open(&artifact.artifact)?;
    let length = file.metadata()?.len();
    if length != artifact.length {
        return Err(integrity("Driver artifact encoded length differs"));
    }
    let mut hash = Sha256::new();
    let count = io::copy(
        &mut (&mut file).take(length.saturating_add(1)),
        &mut HashWriter(&mut hash),
    )?;
    if count != artifact.length || hex::encode(hash.finalize()) != artifact.sha256 {
        return Err(integrity("Driver artifact SHA-256 differs"));
    }
    file.rewind()?;
    Ok(BufReader::new(file))
}
fn copy_inventory(inventory: &Inventory, staging: &Path, published: &Path) -> Result<Inventory> {
    let mut index = 0;
    let mut copy = |artifact: &Artifact| -> Result<Artifact> {
        let name = format!("factory-{index}.bin");
        index += 1;
        let mut input = verified(artifact)?;
        write_artifact(&staging.join(&name), &published.join(name), |output| {
            let count = io::copy(
                &mut (&mut input).take(artifact.length.saturating_add(1)),
                output,
            )?;
            if count != artifact.length {
                return Err(integrity("factory artifact changed during copy"));
            }
            Ok(())
        })
        .and_then(|copied| {
            if copied.sha256 != artifact.sha256 || copied.length != artifact.length {
                return Err(integrity("factory artifact changed during copy"));
            }
            Ok(copied)
        })
    };
    let mut retained = inventory.clone();
    super::inventory::visit(&mut retained, |artifact| {
        *artifact = copy(artifact)?;
        Ok(())
    })?;
    Ok(retained)
}

/// Decode service metadata and verify the whole referenced closure before any
/// backend decode or native restore. Service JSON has a finite byte envelope;
/// the large Driver/native bodies use their own explicit closed codec budgets.
pub(super) fn read(path: &Path) -> Result<wire::Checkpoint> {
    let file = File::open(path)?;
    if file.metadata()?.len() > wire::MAX_ENVELOPE_BYTES {
        return Err(integrity("Driver service envelope exceeds limit"));
    }
    let mut bytes = Vec::new();
    file.take(wire::MAX_ENVELOPE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > wire::MAX_ENVELOPE_BYTES {
        return Err(integrity("Driver service envelope exceeds limit"));
    }
    metadata::validate(&bytes)?;
    let checkpoint: wire::Checkpoint = serde_json::from_slice(&bytes)?;
    checkpoint.validate().map_err(integrity)?;
    // Verify the entire closure first, including unused future-bootstrap inputs.
    for artifact in checkpoint
        .inventory
        .artifacts()
        .into_iter()
        .chain([&checkpoint.driver])
        .chain(&checkpoint.machines)
    {
        verified(artifact)?;
    }
    Ok(checkpoint)
}

/// Resolve by typed digest+length, never a mutable index or host-path codec.
/// Every manifest entry must match a Driver body in insertion order. All
/// references and original bytes are checked before the first backend decoder;
/// identical machine snapshots may legitimately share content identity.
fn validate_machine_names<'a>(
    actual: impl Iterator<Item = Option<&'a str>>,
    expected: &[String],
) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    let mut names = Vec::new();
    for name in actual {
        let name =
            name.ok_or_else(|| integrity("captured Driver has a missing service machine name"))?;
        if !seen.insert(name) {
            return Err(integrity(
                "captured Driver has duplicate service machine names",
            ));
        }
        names.push(name);
    }
    if !names.into_iter().eq(expected.iter().map(String::as_str)) {
        return Err(integrity(
            "service machine names differ from the captured Driver control ledger",
        ));
    }
    Ok(())
}

pub(super) fn open(
    envelope: &wire::Checkpoint,
    limits: seedbox_driver::driver::DecodeLimits,
) -> Result<Checkpoint<Opened>> {
    envelope.validate().map_err(integrity)?;
    for artifact in envelope.inventory.artifacts() {
        verified(artifact)?;
    }
    let mut metadata = verified(&envelope.driver)?;
    let captured =
        Checkpoint::read_from(&mut metadata, limits).map_err(|e| integrity(e.to_string()))?;
    validate_machine_names(
        captured.machine_names().map(|(_, name)| name),
        &envelope.state.world.machine_names,
    )?;
    let mut index = 0;
    let opened = captured.try_map_snapshots(|reference| {
        let artifact = envelope
            .machines
            .get(index)
            .ok_or_else(|| integrity("missing Driver native snapshot reference"))?;
        index += 1;
        if reference != snapshot_reference(artifact)? {
            return Err(integrity(
                "Driver native snapshot reference differs from service inventory",
            ));
        }
        Ok::<_, Error>(Opened {
            file: verified(artifact)?.into_inner(),
            length: artifact.length,
        })
    })?;
    if index != envelope.machines.len() {
        return Err(integrity("unused Driver native snapshot inventory"));
    }
    Ok(opened)
}

/// Already verified immutable service artifact. Each reader has an independent
/// offset (pread), so repeated restores never share a seek cursor. Native RAM
/// is decoded only inside the Driver's per-machine restore callback.
pub(super) struct Opened {
    file: File,
    length: u64,
}
impl Opened {
    pub(super) fn reader(&self) -> impl Read + '_ {
        Section {
            file: &self.file,
            offset: 0,
            length: self.length,
        }
    }
}
struct Section<'a> {
    file: &'a File,
    offset: u64,
    length: u64,
}
impl Read for Section<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let count = usize::try_from((self.length - self.offset).min(bytes.len() as u64))
            .map_err(io::Error::other)?;
        let read = self.file.read_at(&mut bytes[..count], self.offset)?;
        self.offset += read as u64;
        Ok(read)
    }
}

// Only the original phase-1 round-trip tests deliberately materialize a typed
// aggregate. Production uses open + one per-machine restore callback instead.
#[cfg(test)]
fn resolve<S>(
    envelope: &wire::Checkpoint,
    limits: seedbox_driver::driver::DecodeLimits,
    mut decode: impl FnMut(&Inventory, &mut BufReader<File>) -> Result<S>,
) -> Result<Checkpoint<S>> {
    open(envelope, limits)?
        .try_map_snapshots(|file| decode(&envelope.inventory, &mut BufReader::new(file.file)))
}

#[cfg(test)]
mod tests;
