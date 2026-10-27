//! Portable checkpoint definitions and their verified binary closure.
//!
//! Archives contain one `simulation.json`, followed by exactly the referenced
//! `objects/<sha256>` regular files. Archive paths are never extracted; any
//! descriptive host paths in retained diagnostics are never followed on import.
use crate::{
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_spec::definition::{Asset, Definition, Start};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Read, Seek, Write},
};

const MAX_OBJECTS: usize = 8192;
const MANIFEST: &str = "simulation.json";
const MAX_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;
pub const DEFAULT_MAX_BYTES: u64 = seedbox_api::BUNDLE_MAX_BYTES;

fn collect(definition: &Definition, assets: &mut BTreeSet<ContentDigest>) -> Result<()> {
    crate::checkpoint::validate(&definition.start)?;
    definition
        .start
        .validate_overrides(&definition.overrides)
        .map_err(Error::Integrity)?;
    let digest = |asset: &Asset| -> Result<ContentDigest> {
        asset
            .0
            .strip_prefix("sha256:")
            .ok_or_else(|| Error::Integrity("invalid archive asset reference".into()))?
            .to_owned()
            .try_into()
    };
    // The resolved definition owns this closure even if execution never produced
    // a record. Checkpoint definitions can retain a different factory inventory.
    if let Some(closure) = definition.runtime.execution.get("driver_world") {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct DriverClosure {
            executable: ContentDigest,
            manifest: ContentDigest,
            objects: Vec<ContentDigest>,
        }
        let closure: DriverClosure = serde_json::from_value(closure.clone())?;
        assets.insert(closure.executable);
        assets.insert(closure.manifest);
        assets.extend(closure.objects);
    }
    for machine in definition
        .machines
        .values()
        .chain(std::iter::once(&definition.oracle))
    {
        assets.insert(digest(&machine.boot_archive)?);
    }
    if let Start::Checkpoint {
        manifest,
        assets: binaries,
    } = &definition.start
    {
        fn references(value: &serde_json::Value, assets: &BTreeMap<String, Asset>) -> Result<()> {
            match value {
                serde_json::Value::Object(values) => {
                    if let Some(hash) = values.get("sha256").and_then(|v| v.as_str()) {
                        for key in ["artifact", "path"] {
                            if let Some(path) = values.get(key) {
                                let name = path
                                    .as_str()
                                    .and_then(|p| p.strip_prefix("asset:"))
                                    .ok_or_else(|| {
                                        Error::Integrity(
                                            "checkpoint binary must use a captured asset reference"
                                                .into(),
                                        )
                                    })?;
                                let asset = assets.get(name).ok_or_else(|| {
                                    Error::Integrity("checkpoint binary asset missing".into())
                                })?;
                                if asset.0.strip_prefix("sha256:") != Some(hash) {
                                    return Err(Error::Integrity(
                                        "checkpoint binary checksum and asset disagree".into(),
                                    ));
                                }
                            }
                        }
                    }
                    for value in values.values() {
                        references(value, assets)?;
                    }
                }
                serde_json::Value::Array(values) => {
                    for value in values {
                        references(value, assets)?;
                    }
                }
                _ => {}
            }
            Ok(())
        }
        references(manifest, binaries)?;
        for asset in binaries.values() {
            assets.insert(digest(asset)?);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Total wire bytes, including tar headers and padding.
    pub max_bytes: u64,
    pub cancellation: crate::execution::CancellationToken,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            cancellation: Default::default(),
        }
    }
}
impl Options {
    fn check(&self) -> Result<()> {
        if self.max_bytes == 0 {
            return Err(Error::Integrity(
                "archive byte limit must be positive".into(),
            ));
        }
        if self.cancellation.is_cancelled() {
            return Err(Error::Integrity("archive transfer cancelled".into()));
        }
        Ok(())
    }
}
struct Transfer<T> {
    inner: T,
    remaining: u64,
    cancellation: crate::execution::CancellationToken,
}
impl<T> Transfer<T> {
    fn check(&self) -> std::io::Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(std::io::Error::other("archive transfer cancelled"));
        }
        Ok(())
    }
}
impl<T: Read> Read for Transfer<T> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        self.check()?;
        let count = self.inner.read(bytes)?;
        self.remaining = self
            .remaining
            .checked_sub(count as u64)
            .ok_or_else(|| std::io::Error::other("archive exceeds wire byte limit"))?;
        Ok(count)
    }
}
impl<T: Write> Write for Transfer<T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.check()?;
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other("archive exceeds wire byte limit"));
        }
        let count = self.inner.write(bytes)?;
        self.remaining -= count as u64;
        Ok(count)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.check()?;
        self.inner.flush()
    }
}

fn export_payload(
    manifest: &[u8],
    assets: BTreeSet<ContentDigest>,
    store: &ContentStore,
    output: impl Write,
    options: &Options,
) -> Result<()> {
    options.check()?;
    let max_bytes = options.max_bytes;
    if manifest.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(Error::Integrity("archive manifest exceeds 16 MiB".into()));
    }
    let mut total = manifest.len() as u64;
    for asset in &assets {
        total = total
            .checked_add(store.size(asset)?)
            .ok_or_else(|| Error::Integrity("archive size overflow".into()))?;
    }
    if total > max_bytes {
        return Err(Error::Integrity(format!(
            "archive payload exceeds {max_bytes} bytes"
        )));
    }
    let mut archive = tar::Builder::new(Transfer {
        inner: output,
        remaining: max_bytes,
        cancellation: options.cancellation.clone(),
    });
    append(&mut archive, MANIFEST, manifest.len() as u64, manifest)?;
    // Spooling one object avoids loading guest RAM or a kernel into memory. The
    // store verifies the source while copying; object entries use only digests.
    for asset in assets {
        let mut file = tempfile::tempfile()?;
        store.copy_to(
            &asset,
            &mut Transfer {
                inner: &mut file,
                remaining: max_bytes,
                cancellation: options.cancellation.clone(),
            },
        )?;
        let size = file.metadata()?.len();
        file.rewind()?;
        append(
            &mut archive,
            &format!("objects/{}", asset.hex()),
            size,
            file,
        )?;
    }
    archive.finish()?;
    Ok(())
}

fn append<W: Write>(
    archive: &mut tar::Builder<W>,
    path: &str,
    size: u64,
    input: impl Read,
) -> Result<()> {
    let mut header = tar::Header::new_ustar();
    header.set_size(size);
    header.set_mode(0o600);
    header.set_entry_type(tar::EntryType::Regular);
    header.set_cksum();
    archive.append_data(&mut header, path, input)?;
    Ok(())
}

fn import_payload<T>(
    input: impl Read,
    store: &ContentStore,
    options: &Options,
    decode: impl FnOnce(&[u8]) -> Result<(T, BTreeSet<ContentDigest>, BTreeSet<ContentDigest>)>,
) -> Result<T> {
    options.check()?;
    let max_bytes = options.max_bytes;
    let staging = tempfile::tempdir()?;
    let staged = ContentStore::new(staging.path());
    let mut archive = tar::Archive::new(Transfer {
        inner: input,
        remaining: max_bytes,
        cancellation: options.cancellation.clone(),
    });
    // Raw entries expose extension headers rather than letting tar interpret
    // them into aliases or long names. This format has no extension headers.
    let mut entries = archive.entries()?.raw(true);
    let mut manifest = entries
        .next()
        .ok_or_else(|| Error::Integrity("archive is empty".into()))??;
    if manifest.path_bytes().as_ref() != MANIFEST.as_bytes()
        || !manifest.header().entry_type().is_file()
    {
        return Err(Error::Integrity(
            "expected checkpoint archive (simulation.json first)".into(),
        ));
    }
    let size = manifest.size();
    if size > MAX_MANIFEST_BYTES || size > max_bytes {
        return Err(Error::Integrity(
            "archive manifest exceeds transfer limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    manifest.read_to_end(&mut bytes)?;
    if bytes.len() as u64 != size {
        return Err(Error::Integrity("truncated archive manifest".into()));
    }
    let (payload, expected, binaries) = decode(&bytes)?;
    let mut seen = BTreeSet::new();
    let mut total = size;
    for entry in entries {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            return Err(Error::Integrity(
                "archive entries must be regular files".into(),
            ));
        }
        let path = entry.path_bytes();
        let hex = path
            .strip_prefix(b"objects/")
            .ok_or_else(|| Error::Integrity("unexpected archive entry".into()))?;
        let digest: ContentDigest = std::str::from_utf8(hex)
            .map_err(|_| Error::Integrity("invalid archive object name".into()))?
            .to_owned()
            .try_into()?;
        if !expected.contains(&digest) || !seen.insert(digest.clone()) {
            return Err(Error::Integrity(
                "undeclared or duplicate archive object".into(),
            ));
        }
        total = total
            .checked_add(entry.size())
            .ok_or_else(|| Error::Integrity("archive size overflow".into()))?;
        if total > max_bytes {
            return Err(Error::Integrity(format!(
                "archive payload exceeds {max_bytes} bytes"
            )));
        }
        if staged.put_reader(&mut entry)? != digest {
            return Err(Error::Integrity("archive object checksum mismatch".into()));
        }
    }
    if seen != expected {
        return Err(Error::Integrity(
            "archive is missing referenced objects".into(),
        ));
    }
    // Reject concatenated archives and arbitrary trailers, rather than silently
    // accepting a valid prefix. tar consumes the first zero end block.
    let mut tail = Vec::new();
    archive.into_inner().take(1025).read_to_end(&mut tail)?;
    if tail.len() != 512 || tail.iter().any(|byte| *byte != 0) {
        return Err(Error::Integrity(
            "invalid archive end blocks or trailing data".into(),
        ));
    }
    for digest in expected {
        options.check()?;
        let mut file = tempfile::tempfile()?;
        staged.copy_to(
            &digest,
            &mut Transfer {
                inner: &mut file,
                remaining: max_bytes,
                cancellation: options.cancellation.clone(),
            },
        )?;
        file.rewind()?;
        let reader = Transfer {
            inner: file,
            remaining: max_bytes,
            cancellation: options.cancellation.clone(),
        };
        let published = if binaries.contains(&digest) {
            store.put_reader(reader)?
        } else {
            store.put_reader_unsynced(reader)?
        };
        if published != digest {
            return Err(Error::Integrity(
                "archive object changed during publication".into(),
            ));
        }
    }
    Ok(payload)
}

/// A resumable checkpoint contains only execution inputs and their binary closure.
/// It neither requires nor recreates the source catalogue resource.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointManifest {
    format: String,
    sha256: ContentDigest,
    definition: Definition,
}

fn checkpoint_assets(definition: &Definition) -> Result<BTreeSet<ContentDigest>> {
    if !matches!(definition.start, Start::Checkpoint { .. }) {
        return Err(Error::Integrity(
            "checkpoint archive requires captured state".into(),
        ));
    }
    let mut assets = BTreeSet::new();
    collect(definition, &mut assets)?;
    if assets.len() > MAX_OBJECTS {
        return Err(Error::Integrity(
            "checkpoint has too many binary objects".into(),
        ));
    }
    Ok(assets)
}

/// Save a checkpoint with all runtime, machine and captured-state binaries.
pub fn export_checkpoint(
    definition: &Definition,
    store: &ContentStore,
    output: impl Write,
    options: &Options,
) -> Result<()> {
    let assets = checkpoint_assets(definition)?;
    let manifest = serde_json::to_vec(&CheckpointManifest {
        format: "seedbox-checkpoint-v1".into(),
        sha256: ContentDigest::of(&serde_json::to_vec(definition)?),
        definition: definition.clone(),
    })?;
    export_payload(&manifest, assets, store, output, options)
}

/// Validate the entire archive before publishing any assets into `store`.
/// Duplicate, undeclared, missing, corrupt and non-regular entries are errors.
/// Files are streamed into an isolated staging store, never extracted by path.
/// A failed publication may leave verified cache objects, but no resource is
/// registered here.
///
/// Validate and restore checkpoint binaries into an empty content store.
/// Returns independent inputs for a new submission; never registers a resource.
pub fn import_checkpoint(
    input: impl Read,
    store: &ContentStore,
    options: &Options,
) -> Result<Definition> {
    import_payload(input, store, options, |bytes| {
        let manifest: CheckpointManifest = serde_json::from_slice(bytes)?;
        if manifest.format != "seedbox-checkpoint-v1"
            || ContentDigest::of(&serde_json::to_vec(&manifest.definition)?) != manifest.sha256
        {
            return Err(Error::Integrity(
                "unsupported checkpoint format or manifest checksum mismatch".into(),
            ));
        }
        let assets = checkpoint_assets(&manifest.definition)?;
        Ok((manifest.definition, assets.clone(), assets))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execution::{ExecutionProfile, SemanticParameters, WitnessMode};
    use seedbox_spec::definition::*;

    fn fixture(store: &ContentStore) -> Definition {
        let binary = store.put(b"guest-binary").unwrap();
        let asset = Asset(format!("sha256:{}", binary.hex()));
        let profile = ExecutionProfile {
            backend: "archive-test".into(),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: SemanticParameters::new(),
        };
        Definition {
            architecture: Architecture::X86_64,
            seed: 7,
            clock: Clock {
                epoch_seconds: 86400,
            },
            runtime: Runtime {
                execution: serde_json::to_value(profile).unwrap(),
            },
            machines: BTreeMap::new(),
            oracle: Machine {
                image: None,
                name: None,
                boot_archive: asset,
                memory_bytes: 16 * 1024 * 1024,
                process: Process {
                    argv: vec!["/test".into()],
                    environment: BTreeMap::new(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
                volumes: BTreeMap::new(),
                anonymous_volumes: Vec::new(),
            },
            network: seedbox_spec::Network {
                name: "network".into(),
                latency_ns: 1000,
                jitter_ns: 0,
                bandwidth_bps: None,
                queue_bound_ns: None,
                links: Vec::new(),
            },
            simulator: Simulator {
                quantum_ns: std::num::NonZeroU64::new(1_000_000).unwrap(),
                progress_budget: std::num::NonZeroU32::new(10_000).unwrap().into(),
                schedule: Default::default(),
                disk: Default::default(),
                oracle_control: false,
            },
            start: Start::Fresh,
            overrides: Vec::new(),
        }
    }
    /// A checkpoint definition whose captured state is `ram`.
    fn checkpoint(store: &ContentStore, ram: &[u8]) -> Definition {
        let mut definition = fixture(store);
        let state = store.put(ram).unwrap();
        definition.start = Start::Checkpoint {
            manifest: serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[]}}}, "ram":{"artifact":"asset:ram", "sha256":state.hex()}}),
            assets: BTreeMap::from([("ram".into(), Asset(format!("sha256:{}", state.hex())))]),
        };
        definition
    }
    fn encode(definition: &Definition, store: &ContentStore) -> Vec<u8> {
        let mut bytes = Vec::new();
        export_checkpoint(definition, store, &mut bytes, &Options::default()).unwrap();
        bytes
    }
    fn entries(bytes: &[u8]) -> Vec<(String, tar::EntryType, Vec<u8>)> {
        tar::Archive::new(bytes)
            .entries()
            .unwrap()
            .raw(true)
            .map(|entry| {
                let mut entry = entry.unwrap();
                let name = String::from_utf8(entry.path_bytes().into_owned()).unwrap();
                let kind = entry.header().entry_type();
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                (name, kind, bytes)
            })
            .collect()
    }
    fn pack(entries: &[(String, tar::EntryType, Vec<u8>)]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for (name, kind, bytes) in entries {
            let mut header = tar::Header::new_ustar();
            header.set_size(bytes.len() as u64);
            header.set_mode(0o600);
            header.set_entry_type(*kind);
            header.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
            header.set_cksum();
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        builder.into_inner().unwrap()
    }
    #[test]
    fn checkpoint_file_survives_source_deletion_and_rejects_missing_or_corrupt_state() {
        let source = tempfile::tempdir().unwrap();
        let store = ContentStore::new(source.path());
        let definition = checkpoint(&store, b"checkpoint RAM and coordinator state");
        let state = store.put(b"checkpoint RAM and coordinator state").unwrap();
        let bytes = encode(&definition, &store);
        drop(store);
        drop(source);
        let destination = tempfile::tempdir().unwrap();
        let store = ContentStore::new(destination.path());
        let restored = import_checkpoint(bytes.as_slice(), &store, &Options::default()).unwrap();
        assert_eq!(restored, definition);
        assert_eq!(
            store.read(&state).unwrap(),
            b"checkpoint RAM and coordinator state"
        );
        for digest in checkpoint_assets(&restored).unwrap() {
            store.verify(&digest).unwrap();
        }
        assert_eq!(encode(&restored, &store), bytes);
        let mut files = entries(&bytes);
        files[1].2[0] ^= 1;
        assert!(import_checkpoint(pack(&files).as_slice(), &store, &Options::default()).is_err());
        assert!(
            import_checkpoint(pack(&files[..1]).as_slice(), &store, &Options::default()).is_err()
        );
        let mut cold = restored;
        cold.start = Start::Fresh;
        assert!(export_checkpoint(&cold, &store, Vec::new(), &Options::default()).is_err());
    }

    #[test]
    fn rejects_corruption_duplicates_unsafe_types_paths_and_missing_objects() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::new(directory.path());
        let definition = checkpoint(&store, b"checkpoint-memory");
        let bytes = encode(&definition, &store);
        let original = entries(&bytes);
        let mut cases = Vec::new();
        let mut corrupt = original.clone();
        corrupt[1].2[0] ^= 1;
        cases.push(corrupt);
        let mut duplicate = original.clone();
        duplicate.push(duplicate[1].clone());
        cases.push(duplicate);
        let mut metadata = original.clone();
        let text = String::from_utf8(metadata[0].2.clone()).unwrap();
        assert!(text.contains("\"/test\""));
        metadata[0].2 = text.replace("\"/test\"", "\"/tesu\"").into_bytes();
        cases.push(metadata);
        cases.push(original[..1].to_vec());
        for name in [
            "../outside",
            "/tmp/outside",
            "objects/../outside",
            "simulation.json",
            "objects/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        ] {
            let mut unsafe_entry = original.clone();
            unsafe_entry[1].0 = name.into();
            cases.push(unsafe_entry);
        }
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Directory,
            tar::EntryType::XHeader,
            tar::EntryType::GNULongName,
        ] {
            let mut unsafe_entry = original.clone();
            unsafe_entry[1].1 = kind;
            cases.push(unsafe_entry);
        }
        for case in cases {
            let destination = tempfile::tempdir().unwrap();
            let store = ContentStore::new(destination.path());
            assert!(
                import_checkpoint(pack(&case).as_slice(), &store, &Options::default()).is_err()
            );
            assert!(
                !destination.path().join("objects").exists(),
                "invalid archive published content"
            );
        }
        let mut trailer = bytes.clone();
        trailer.push(1);
        assert!(import_checkpoint(trailer.as_slice(), &store, &Options::default()).is_err());
        assert!(
            import_checkpoint(&bytes[..bytes.len() - 513], &store, &Options::default()).is_err()
        );
    }

    #[test]
    fn enforces_wire_metadata_object_count_and_cancellation_bounds() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::new(directory.path());
        let mut definition = checkpoint(&store, b"checkpoint-memory");
        let bytes = encode(&definition, &store);
        let exact = Options {
            max_bytes: bytes.len() as u64,
            ..Default::default()
        };
        import_checkpoint(bytes.as_slice(), &store, &exact).unwrap();
        export_checkpoint(&definition, &store, Vec::new(), &exact).unwrap();
        let short = Options {
            max_bytes: bytes.len() as u64 - 1,
            ..Default::default()
        };
        assert!(import_checkpoint(bytes.as_slice(), &store, &short).is_err());
        assert!(export_checkpoint(&definition, &store, Vec::new(), &short).is_err());
        let cancelled = Options::default();
        cancelled.cancellation.cancel();
        assert!(import_checkpoint(bytes.as_slice(), &store, &cancelled).is_err());
        assert!(export_checkpoint(&definition, &store, Vec::new(), &cancelled).is_err());
        definition
            .oracle
            .process
            .environment
            .insert("OVERSIZED".into(), "a".repeat(MAX_MANIFEST_BYTES as usize));
        assert!(export_checkpoint(&definition, &store, Vec::new(), &Options::default()).is_err());
        definition.oracle.process.environment.clear();
        definition.start = Start::Checkpoint {
            manifest: serde_json::json!({"state":{"world":{"input_program":{"inputs":[],"next":0,"admissions":[],"rejections":[]}}}}),
            assets: (0..=MAX_OBJECTS)
                .map(|index| {
                    (
                        index.to_string(),
                        Asset(format!(
                            "sha256:{}",
                            ContentDigest::of(index.to_string().as_bytes()).hex()
                        )),
                    )
                })
                .collect(),
        };
        assert!(checkpoint_assets(&definition).is_err());
    }

    #[test]
    fn large_binary_closure_is_disk_spooled_with_bounded_reads_and_midstream_cancellation() {
        struct BoundedReader {
            file: std::fs::File,
            cancellation: Option<crate::execution::CancellationToken>,
            bytes: u64,
        }
        impl Read for BoundedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(
                    buffer.len() <= 64 * 1024,
                    "archive attempted an unbounded binary read"
                );
                let count = self.file.read(buffer)?;
                self.bytes += count as u64;
                if self.bytes > 128 * 1024 {
                    if let Some(token) = &self.cancellation {
                        token.cancel();
                    }
                }
                Ok(count)
            }
        }
        let source = tempfile::tempdir().unwrap();
        let store = ContentStore::new(source.path());
        let mut definition = checkpoint(&store, b"checkpoint-memory");
        let large = store
            .put_reader(std::io::repeat(42).take(16 * 1024 * 1024))
            .unwrap();
        definition.oracle.boot_archive = Asset(format!("sha256:{}", large.hex()));
        let mut spool = tempfile::tempfile().unwrap();
        export_checkpoint(&definition, &store, &mut spool, &Options::default()).unwrap();
        spool.rewind().unwrap();
        let destination = tempfile::tempdir().unwrap();
        let target = ContentStore::new(destination.path());
        import_checkpoint(
            BoundedReader {
                file: spool.try_clone().unwrap(),
                cancellation: None,
                bytes: 0,
            },
            &target,
            &Options::default(),
        )
        .unwrap();
        target.verify(&large).unwrap();
        spool.rewind().unwrap();
        let cancelled = Options::default();
        let destination = tempfile::tempdir().unwrap();
        let target = ContentStore::new(destination.path());
        assert!(import_checkpoint(
            BoundedReader {
                file: spool,
                cancellation: Some(cancelled.cancellation.clone()),
                bytes: 0
            },
            &target,
            &cancelled
        )
        .is_err());
        assert!(!destination.path().join("objects").exists());
    }
}
