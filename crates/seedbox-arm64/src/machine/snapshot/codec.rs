//! Framed, checksummed stopped-machine state. Callers publish the completed
//! stream atomically; an incomplete stream can never restore a machine.
use super::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const MAGIC: &[u8; 8] = b"SBXARM02";
const MAX_METADATA: u64 = 64 << 20;
/// RAM data starts at a multiple of this file offset (the host page size), so
/// a lazy restore can map it; a zero padding section before it gets there.
pub(crate) const RAM_ALIGN: u64 = crate::PAGE_SIZE as u64;

/// How a restore took the RAM section.
enum Ram {
    /// Read and verified.
    Bytes(Vec<u8>),
    /// Left in the file at this offset, for the caller to map; unverified.
    Mapped(u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Devices {
    net: application::virtio::Snapshot,
    console: application::virtio::Snapshot,
    block: Option<application::virtio::Snapshot>,
    events: Vec<application::Event>,
    exit: Option<(u8, bool)>,
    barrier: bool,
    entropy: std::collections::VecDeque<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    state: Snapshot,
    devices: Option<Devices>,
}

struct HashIo<T> {
    io: T,
    hash: Sha256,
}
impl<T: Write> Write for HashIo<T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.io.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.io.flush()
    }
}
impl<T: Read> Read for HashIo<T> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.io.read(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}
fn io_error(error: std::io::Error) -> String {
    format!("ARM checkpoint I/O: {error}")
}
fn bytes<const N: usize>(reader: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0; N];
    reader.read_exact(&mut bytes).map_err(io_error)?;
    Ok(bytes)
}
fn write_section(
    writer: &mut impl Write,
    length: u64,
    write: impl FnOnce(&mut HashIo<&mut dyn Write>) -> Result<()>,
) -> Result<()> {
    writer.write_all(&length.to_le_bytes()).map_err(io_error)?;
    let mut hashed = HashIo {
        io: writer as &mut dyn Write,
        hash: Sha256::new(),
    };
    write(&mut hashed)?;
    let digest = hashed.hash.finalize();
    hashed.io.write_all(&digest).map_err(io_error)
}
fn read_section<T>(
    reader: &mut impl Read,
    max: u64,
    read: impl FnOnce(&mut HashIo<std::io::Take<&mut dyn Read>>, u64) -> Result<T>,
) -> Result<T> {
    let length = u64::from_le_bytes(bytes(reader)?);
    if length > max {
        return Err("ARM checkpoint section exceeds its size limit".into());
    }
    let mut hashed = HashIo {
        io: (reader as &mut dyn Read).take(length),
        hash: Sha256::new(),
    };
    let value = read(&mut hashed, length)?;
    if hashed.io.limit() != 0 {
        return Err("ARM checkpoint section was not consumed".into());
    }
    let digest: [u8; 32] = hashed.hash.finalize().into();
    if digest != bytes::<32>(hashed.io.get_mut())? {
        return Err("ARM checkpoint section checksum mismatch".into());
    }
    Ok(value)
}
fn read_vec(reader: &mut impl Read, length: u64) -> Result<Vec<u8>> {
    let length = usize::try_from(length).map_err(|_| "ARM checkpoint length overflow")?;
    let mut data = Vec::new();
    data.try_reserve_exact(length)
        .map_err(|e| format!("ARM checkpoint allocation: {e}"))?;
    data.resize(length, 0);
    reader.read_exact(&mut data).map_err(io_error)?;
    Ok(data)
}

impl Snapshot {
    /// Write the checkpoint stream; `start` is its offset in the file, so
    /// the RAM section's data lands on a RAM_ALIGN boundary.
    pub fn write_checkpoint(&self, mut writer: impl Write, start: u64) -> Result<()> {
        let devices = self
            .application
            .as_ref()
            .map(|d| {
                Ok::<_, String>(Devices {
                    net: d.net.snapshot()?,
                    console: d.console.snapshot()?,
                    block: d
                        .block
                        .as_ref()
                        .map(application::virtio::Virtio::snapshot)
                        .transpose()?,
                    events: d.events.clone(),
                    exit: d.exit,
                    barrier: d.barrier,
                    entropy: d.entropy.clone(),
                })
            })
            .transpose()?;
        let metadata = serde_json::to_vec(&Metadata {
            state: self.clone(),
            devices,
        })
        .map_err(|e| format!("encode ARM checkpoint: {e}"))?;
        if metadata.len() as u64 > MAX_METADATA {
            return Err("ARM checkpoint metadata exceeds size limit".into());
        }
        writer.write_all(MAGIC).map_err(io_error)?;
        write_section(&mut writer, metadata.len() as u64, |out| {
            out.write_all(&metadata).map_err(io_error)
        })?;
        // Magic, metadata section, then the padding and RAM section headers
        // and the padding digest before the RAM data.
        let before = start + 8 + 8 + metadata.len() as u64 + 32 + 8 + 32 + 8;
        let padding = (RAM_ALIGN - before % RAM_ALIGN) % RAM_ALIGN;
        write_section(&mut writer, padding, |out| {
            std::io::copy(&mut std::io::repeat(0).take(padding), out)
                .map(drop)
                .map_err(io_error)
        })?;
        write_section(&mut writer, self.ram.len() as u64, |out| {
            out.write_all(&self.ram).map_err(io_error)
        })?;
        if let Some(disk) = self
            .application
            .as_ref()
            .and_then(|d| d.block.as_ref())
            .and_then(|d| d.disk())
        {
            // The disk codec has a fixed header/trailer and two sparse layers.
            // Count by streaming to a sink so its format remains owned by Disk.
            let mut counter = Counter(0);
            disk.write_checkpoint(&mut counter)?;
            write_section(&mut writer, counter.0, |out| disk.write_checkpoint(out))?;
        }
        writer.flush().map_err(io_error)
    }
}
struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("checkpoint size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<B: Backend> Machine<B> {
    /// The boot identity a checkpoint stream was captured under, read without
    /// a machine, so a restore can build its replacement from the same boot.
    pub fn checkpoint_boot_identity(mut reader: impl Read) -> Result<application::BootIdentity> {
        if &bytes::<8>(&mut reader)? != MAGIC {
            return Err("unsupported ARM checkpoint format".into());
        }
        let metadata: Metadata = read_section(&mut reader, MAX_METADATA, |input, length| {
            serde_json::from_slice(&read_vec(input, length)?)
                .map_err(|e| format!("decode ARM checkpoint: {e}"))
        })?;
        Ok(metadata.state.boot_identity)
    }
    /// Consume a fresh replacement. Verification finishes before touching the
    /// native VM; any error drops the replacement and leaves the source intact.
    pub fn restore_checkpoint(self, reader: impl Read) -> Result<Self> {
        let (machine, state, ram) = self.read_checkpoint(reader, |input, length| {
            let mut hashed = HashIo {
                io: (input as &mut dyn Read).take(length),
                hash: Sha256::new(),
            };
            let data = read_vec(&mut hashed, length)?;
            let digest: [u8; 32] = hashed.hash.finalize().into();
            if digest != bytes::<32>(input)? {
                return Err("ARM checkpoint section checksum mismatch".into());
            }
            Ok(Ram::Bytes(data))
        })?;
        let Ram::Bytes(bytes) = ram else {
            unreachable!("the reader returned bytes")
        };
        let mut state = state;
        state.ram = Arc::new(bytes);
        machine.restore_snapshot(&state)
    }

    /// As `restore_checkpoint`, but guest RAM becomes a private mapping of
    /// the checkpoint file's RAM section: pages load when the guest first
    /// touches them and guest writes stay private. The RAM section's
    /// checksum is not verified, since that would read every page; the
    /// metadata and disk sections are.
    pub fn restore_checkpoint_lazy(self, file: &mut std::fs::File) -> Result<Self> {
        use std::io::{Seek, SeekFrom};
        let (mut machine, state, ram) = self.read_checkpoint(&mut *file, |input, length| {
            let offset = input.stream_position().map_err(io_error)?;
            if offset % RAM_ALIGN != 0 {
                return Err("ARM checkpoint RAM is not page aligned".into());
            }
            input
                .seek(SeekFrom::Current((length + 32) as i64))
                .map_err(io_error)?;
            Ok(Ram::Mapped(offset))
        })?;
        let Ram::Mapped(offset) = ram else {
            unreachable!("the reader returned an offset")
        };
        machine.map_ram_from(file, offset)?;
        machine.restore_snapshot_without_ram(&state)
    }

    /// Parse a checkpoint stream into the replacement's state, taking the RAM
    /// section through `ram` (given the reader positioned at its data and its
    /// length, it must consume the data and the digest).
    fn read_checkpoint<R: Read>(
        self,
        mut reader: R,
        ram: impl FnOnce(&mut R, u64) -> Result<Ram>,
    ) -> Result<(Self, Snapshot, Ram)> {
        if &bytes::<8>(&mut reader)? != MAGIC {
            return Err("unsupported ARM checkpoint format".into());
        }
        let mut metadata: Metadata = read_section(&mut reader, MAX_METADATA, |input, length| {
            serde_json::from_slice(&read_vec(input, length)?)
                .map_err(|e| format!("decode ARM checkpoint: {e}"))
        })?;
        if !metadata
            .state
            .boot_identity
            .same_inputs(&self.boot_identity)
            || metadata.state.kernel_hash != self.kernel_hash
            || metadata.state.initrd_hash != self.initrd_hash
            || metadata.state.page.len() != self.page.len()
        {
            let field = if !metadata
                .state
                .boot_identity
                .same_inputs(&self.boot_identity)
            {
                format!(
                    "boot layout {:?} != {:?}",
                    metadata.state.boot_identity, self.boot_identity
                )
            } else if metadata.state.kernel_hash != self.kernel_hash {
                "kernel".into()
            } else if metadata.state.initrd_hash != self.initrd_hash {
                "initrd".into()
            } else {
                "PV page size".into()
            };
            return Err(format!("ARM checkpoint boot inputs mismatch: {field}"));
        }
        read_section(&mut reader, RAM_ALIGN, |input, length| {
            std::io::copy(&mut input.take(length), &mut std::io::sink())
                .map(drop)
                .map_err(io_error)
        })?;
        let length = u64::from_le_bytes(bytes(&mut reader)?);
        if length != self.ram.len() as u64 {
            return Err("ARM checkpoint RAM size mismatch".into());
        }
        let ram = ram(&mut reader, length)?;
        metadata.state.application = match (self.application.clone(), metadata.devices) {
            (None, None) => None,
            (Some(base), Some(saved)) => {
                let block = match (base.block, saved.block) {
                    (None, None) => None,
                    (Some(base), Some(saved)) => {
                        let disk = base
                            .disk()
                            .ok_or("checkpoint block device has no disk")?
                            .clone();
                        let max = disk
                            .capacity()
                            .checked_mul(1040)
                            .and_then(|v| v.checked_add(1024))
                            .ok_or("ARM checkpoint disk size overflow")?;
                        let disk = read_section(&mut reader, max, |input, _| {
                            crate::disk::Disk::read_checkpoint(disk, input)
                        })?;
                        Some(application::virtio::Virtio::block(disk).restore_snapshot(saved)?)
                    }
                    _ => return Err("ARM checkpoint block device mismatch".into()),
                };
                Some(application::Devices {
                    net: base.net.restore_snapshot(saved.net)?,
                    console: base.console.restore_snapshot(saved.console)?,
                    block,
                    events: saved.events,
                    exit: saved.exit,
                    barrier: saved.barrier,
                    entropy: saved.entropy,
                })
            }
            _ => return Err("ARM checkpoint application devices mismatch".into()),
        };
        if reader.read(&mut [0]).map_err(io_error)? != 0 {
            return Err("ARM checkpoint has trailing data".into());
        }
        Ok((self, metadata.state, ram))
    }
}
