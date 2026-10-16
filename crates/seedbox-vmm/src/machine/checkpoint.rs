//! Native checkpoint files keep RAM and disk bytes outside coordinator JSON.
use super::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    fs::OpenOptions,
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"SBXXSNP1";
const MAX_METADATA: u64 = 64 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    path: PathBuf,
    sha256: String,
}

pub enum MachineSnapshot {
    Memory(Box<MemorySnapshot>),
    File(Artifact),
}

impl Serialize for MachineSnapshot {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::File(artifact) => artifact.serialize(serializer),
            Self::Memory(saved) => saved
                .artifact
                .get()
                .ok_or_else(|| {
                    serde::ser::Error::custom("machine snapshot has not been persisted")
                })?
                .serialize(serializer),
        }
    }
}
impl<'de> Deserialize<'de> for MachineSnapshot {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Artifact::deserialize(deserializer).map(Self::File)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata<T> {
    state: T,
    block: Option<crate::virtio_blk::Snapshot>,
}

impl MachineSnapshot {
    /// Bytes this snapshot retains: its RAM copy, or its persisted file.
    pub fn ram_bytes(&self) -> u64 {
        match self {
            Self::Memory(saved) => saved.ram.size as u64,
            Self::File(artifact) => std::fs::metadata(&artifact.path).map_or(0, |m| m.len()),
        }
    }
    /// Publish only a complete file. Coordinator metadata is written afterward.
    pub fn persist(&self, path: &Path) -> Result<(), String> {
        let Self::Memory(saved) = self else {
            return Ok(());
        };
        if saved.artifact.get().is_some() {
            return Ok(());
        }
        let path = std::path::absolute(path).map_err(io_error)?;
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_error)?;
        let result = write(saved, BufWriter::new(file));
        match result {
            Ok(sha256) => saved
                .artifact
                .set(Artifact { path, sha256 })
                .map_err(|_| "machine snapshot was already persisted".into()),
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                Err(error)
            }
        }
    }
}

fn io_error(error: std::io::Error) -> String {
    format!("machine checkpoint: {error}")
}

struct Hashing<T> {
    inner: T,
    hash: Sha256,
}
impl<T> Hashing<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
        }
    }
}
impl<T: Write> Write for Hashing<T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hash.update(&bytes[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
impl<T: Read> Read for Hashing<T> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(bytes)?;
        self.hash.update(&bytes[..read]);
        Ok(read)
    }
}

fn write(saved: &MemorySnapshot, output: impl Write) -> Result<String, String> {
    let metadata = serde_json::to_vec(&Metadata {
        state: &saved.state,
        block: saved.blk.as_ref().map(VirtioBlk::snapshot).transpose()?,
    })
    .map_err(|e| format!("machine checkpoint metadata: {e}"))?;
    if metadata.len() as u64 > MAX_METADATA {
        return Err("machine checkpoint metadata exceeds 64 MiB".into());
    }
    let mut output = Hashing::new(output);
    output.write_all(MAGIC).map_err(io_error)?;
    output
        .write_all(&(metadata.len() as u64).to_le_bytes())
        .map_err(io_error)?;
    output.write_all(&metadata).map_err(io_error)?;
    output
        .write_all(&(saved.ram.size as u64).to_le_bytes())
        .map_err(io_error)?;
    // SAFETY: the owned snapshot mapping remains live and immutable throughout this write.
    output
        .write_all(unsafe { std::slice::from_raw_parts(saved.ram.ptr.0, saved.ram.size) })
        .map_err(io_error)?;
    output.write_all(saved.page.as_slice()).map_err(io_error)?;
    if let Some(block) = &saved.blk {
        block.disk().write_checkpoint(&mut output)?;
    }
    output.flush().map_err(io_error)?;
    Ok(hex::encode(output.hash.finalize()))
}

fn bytes<const N: usize>(input: &mut impl Read) -> Result<[u8; N], String> {
    let mut bytes = [0; N];
    input.read_exact(&mut bytes).map_err(io_error)?;
    Ok(bytes)
}

pub(super) fn read(machine: &Machine, artifact: &Artifact) -> Result<MemorySnapshot, String> {
    let file = std::fs::File::open(&artifact.path).map_err(io_error)?;
    let mut input = Hashing::new(BufReader::new(file));
    if &bytes::<8>(&mut input)? != MAGIC {
        return Err("unsupported x86 checkpoint format".into());
    }
    let length = u64::from_le_bytes(bytes(&mut input)?);
    if length > MAX_METADATA {
        return Err("machine checkpoint metadata exceeds 64 MiB".into());
    }
    let mut encoded = vec![0; length as usize];
    input.read_exact(&mut encoded).map_err(io_error)?;
    let metadata: Metadata<SnapshotState> = serde_json::from_slice(&encoded)
        .map_err(|e| format!("machine checkpoint metadata: {e}"))?;
    let size = u64::from_le_bytes(bytes(&mut input)?);
    if size != machine.mem_size as u64
        || metadata.state.world.name != machine.world.name
        || metadata.block.is_some() != machine.blk.is_some()
    {
        return Err("machine checkpoint memory, name or disk configuration mismatch".into());
    }
    let ram = Mapping {
        ptr: SendPtr(map(machine.mem_size)?),
        size: machine.mem_size,
    };
    // SAFETY: this fresh mapping is exclusively owned, writable, and exactly size bytes long.
    input
        .read_exact(unsafe { std::slice::from_raw_parts_mut(ram.ptr.0, ram.size) })
        .map_err(io_error)?;
    let mut page = Box::new([0; 4096]);
    input.read_exact(page.as_mut_slice()).map_err(io_error)?;
    let blk = match (metadata.block, machine.blk.as_ref()) {
        (Some(saved), Some(base)) => Some(VirtioBlk::from_snapshot(
            saved,
            Disk::read_checkpoint(base.disk().clone(), &mut input)?,
        )?),
        (None, None) => None,
        _ => return Err("machine checkpoint disk configuration mismatch".into()),
    };
    let mut trailing = [0];
    if input.read(&mut trailing).map_err(io_error)? != 0 {
        return Err("machine checkpoint has trailing data".into());
    }
    if hex::encode(input.hash.finalize()) != artifact.sha256 {
        return Err("machine checkpoint checksum mismatch".into());
    }
    Ok(MemorySnapshot {
        ram,
        page,
        blk,
        state: metadata.state,
        artifact: Default::default(),
    })
}
