//! Owned single-machine worker. HVF permits one VM per process; the world
//! coordinator serializes these workers through a bounded, numbered protocol.
use super::{Event, Evidence};
use crate::{
    backend::Backend,
    boot::BootSpec,
    disk::{Disk, DiskFaultModel},
    machine::{Machine, MachineStop},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

const MAX_MESSAGE: usize = 16 << 20;
pub const CONTRACT: &str = "arm64-machine-v3";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub seed: u64,
    pub memory_bytes: u64,
    pub command_line: String,
    pub mac: [u8; 6],
    pub disk_image: Option<PathBuf>,
    pub disk_sectors: Option<u64>,
    pub disk_fault_model: DiskFaultModel,
    #[serde(default)]
    pub timer_policy: Option<super::TimerPolicy>,
    #[serde(default)]
    pub progress: Option<super::Progress>,
}
impl Boot {
    fn machine<B: Backend>(&self, disk: Option<Disk>) -> Result<Machine<B>> {
        let kernel = std::fs::read(&self.kernel)
            .map_err(|e| format!("kernel {}: {e}", self.kernel.display()))?;
        let initrd = std::fs::read(&self.initrd)
            .map_err(|e| format!("initrd {}: {e}", self.initrd.display()))?;
        let machine = Machine::for_application(
            BootSpec::for_application(
                &kernel,
                &initrd,
                self.seed,
                self.memory_bytes,
                &self.command_line,
            )?,
            self.mac,
            disk,
        )?;
        let machine = match &self.progress {
            Some(progress) => machine.with_progress(progress)?,
            None => machine,
        };
        match &self.timer_policy {
            Some(policy) => machine.with_timer_policy(policy.clone()),
            None => Ok(machine),
        }
    }
    fn disk(&self) -> Result<Option<Disk>> {
        if self.disk_image.is_none() && self.disk_sectors.is_none() {
            return Ok(None);
        }
        let image = self
            .disk_image
            .as_ref()
            .map(|path| std::fs::read(path).map_err(|e| format!("disk {}: {e}", path.display())))
            .transpose()?;
        Disk::new(image, self.disk_sectors)
            .map(|disk| Some(disk.with_fault_model(self.disk_fault_model)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub generation: u64,
    pub incarnation: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferDescriptor {
    pub transaction: u64,
    pub predecessor: Identity,
    pub successor: Identity,
    pub artifact: PathBuf,
    pub at: u64,
}
impl TransferDescriptor {
    pub fn validate(&self) -> Result<()> {
        if self.predecessor.name != self.successor.name
            || self.predecessor.generation == self.successor.generation
            || self.predecessor.incarnation.checked_add(1) != Some(self.successor.incarnation)
            || self.at == u64::MAX
            || !self.artifact.is_absolute()
        {
            return Err("invalid succession transaction".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedTransfer {
    pub descriptor: TransferDescriptor,
    pub disk_digest: Option<String>,
    pub artifact_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedObservation {
    pub time_ns: u64,
    pub pending_irq: bool,
    pub next_deadline: u64,
    pub owed_io: Option<u64>,
    pub abi: u64,
    pub exits: u64,
    pub powered_off: bool,
    pub exit: Option<(u8, bool)>,
    pub barrier: bool,
    pub has_control: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub artifact: PathBuf,
    pub identity: Identity,
    pub observation: StoppedObservation,
    pub console_offset: usize,
    pub sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointHeader {
    identity: Identity,
    observation: StoppedObservation,
    console_offset: usize,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum Operation {
    Boot(Boot),
    Advance {
        hard_limit: u64,
        early_slack: u64,
        observe_console: bool,
        /// Frames to deliver, in order, if the slice reaches `hard_limit`.
        #[serde(default, with = "wire::frames")]
        then: Vec<Vec<u8>>,
        /// Slices to run next, in order, while each earlier one reaches its
        /// limit: the coordinator's next requests in that case.
        #[serde(default)]
        ahead: Vec<Slice>,
    },
    /// Frames due at this stopped point, delivered in order; each has the
    /// effects of a separate delivery. Never empty.
    Frames(#[serde(with = "wire::frames")] Vec<Vec<u8>>),
    Control(#[serde(with = "wire::bytes")] Vec<u8>),
    Release {
        at: u64,
        entropy: [u32; 8],
    },
    ResumeAt(u64),
    Park,
    SeedTimer {
        seed: u64,
        domain: String,
    },
    SeedProgressJitter {
        seed: u64,
    },
    Throttle {
        factor_permille: u64,
    },
    DiskErrors(Option<seedbox_engine_core::virtio_block::InjectedErrors>),
    DiskLatency(Option<seedbox_engine_core::virtio_block::InjectedLatency>),
    Preempt(Option<(u32, u64)>),
    HoldBlock(Option<seedbox_engine_core::virtio_block::Hold>),
    ReseedGuest {
        entropy: [u32; 8],
    },
    Evidence {
        full_state: bool,
        #[serde(default)]
        ram_pages: Option<PathBuf>,
    },
    CrashExport {
        run_seed: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_identity: Option<String>,
        full_state: bool,
        transfer: TransferDescriptor,
    },
    InstallSuccession(ExportedTransfer),
    Snapshot {
        artifact: PathBuf,
    },
    Restore(Checkpoint),
    Quit,
}
/// One slice of an `Advance` chain, exactly as a separate request states it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Slice {
    pub hard_limit: u64,
    pub early_slack: u64,
    pub observe_console: bool,
    #[serde(with = "wire::frames")]
    pub then: Vec<Vec<u8>>,
}

/// What one chained slice answers, with the stopped facts after it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SliceAnswer {
    pub stop: MachineStop,
    pub time_ns: u64,
    #[serde(with = "wire::events")]
    pub events: Vec<Event>,
    #[serde(with = "wire::bytes")]
    pub console: Vec<u8>,
    pub log_chunks: Vec<super::log_capture::Chunk>,
    pub accepted: Option<Vec<bool>>,
    pub observation: StoppedObservation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub sequence: u64,
    pub expected: Identity,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Answer {
    Ready,
    Snapshot(Checkpoint),
    Restored(Checkpoint),
    Advanced {
        stop: MachineStop,
        time_ns: u64,
        #[serde(with = "wire::events")]
        events: Vec<Event>,
        #[serde(with = "wire::bytes")]
        console: Vec<u8>,
        log_chunks: Vec<super::log_capture::Chunk>,
        /// One result per `then` frame, present exactly when they were delivered.
        accepted: Option<Vec<bool>>,
        /// The chained slices that ran after this one, which reached its limit.
        #[serde(default)]
        ahead: Vec<SliceAnswer>,
        /// The stopped facts after this slice, present exactly when `ahead`
        /// is not empty; the response's observation follows the last slice.
        first: Option<StoppedObservation>,
    },
    /// One result per delivered frame, in request order.
    Accepted(Vec<bool>),
    DiskErrors(Option<seedbox_engine_core::virtio_block::ErrorCounts>),
    DiskLatency(Option<seedbox_engine_core::virtio_block::LatencyCounts>),
    Evidence(Evidence),
    Crashed {
        evidence: Evidence,
        lost_sectors: u64,
        transfer: ExportedTransfer,
    },
    Installed {
        transfer: ExportedTransfer,
    },
    Error(String),
    Bye,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub sequence: u64,
    pub before: Option<Identity>,
    pub after: Option<Identity>,
    pub observation: Option<StoppedObservation>,
    pub answer: Answer,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferHeader {
    descriptor: TransferDescriptor,
    disk_digest: Option<String>,
}

/// Payload bytes (frames, console and management output) cross the worker
/// channel as one binary attachment after the JSON message, which names each
/// by offset and length; elsewhere, as in files, they are hexadecimal strings
/// rather than JSON number arrays. The protocol is private to one executable;
/// witnesses hash `Event`'s own serialization, which this does not change.
mod wire {
    use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
    use std::cell::RefCell;

    thread_local! {
        /// The attachment of the channel message being written or read.
        static ATTACHMENT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    /// Serialize with payloads moved to an attachment, which is returned.
    pub fn attach<T>(f: impl FnOnce() -> T) -> (T, Vec<u8>) {
        ATTACHMENT.with(|a| *a.borrow_mut() = Some(Vec::new()));
        let value = f();
        let attachment = ATTACHMENT
            .with(|a| a.borrow_mut().take())
            .unwrap_or_default();
        (value, attachment)
    }

    /// Deserialize with payloads taken from `attachment`.
    pub fn detach<T>(attachment: Vec<u8>, f: impl FnOnce() -> T) -> T {
        ATTACHMENT.with(|a| *a.borrow_mut() = Some(attachment));
        let value = f();
        ATTACHMENT.with(|a| a.borrow_mut().take());
        value
    }

    const DIGITS: &[u8; 16] = b"0123456789abcdef";

    pub fn encode(bytes: &[u8]) -> String {
        let mut out = Vec::with_capacity(bytes.len() * 2);
        for byte in bytes {
            out.push(DIGITS[(byte >> 4) as usize]);
            out.push(DIGITS[(byte & 15) as usize]);
        }
        String::from_utf8(out).expect("hexadecimal digits are ASCII")
    }

    /// Each ASCII byte's digit value, or 16 when it is not a lowercase digit.
    const VALUES: [u8; 256] = {
        let mut table = [16u8; 256];
        let mut i = 0;
        while i < 16 {
            table[DIGITS[i] as usize] = i as u8;
            i += 1;
        }
        table
    };

    pub fn decode(text: &str) -> Result<Vec<u8>, String> {
        let text = text.as_bytes();
        if !text.len().is_multiple_of(2) {
            return Err("odd-length hexadecimal bytes".into());
        }
        let mut out = Vec::with_capacity(text.len() / 2);
        let mut invalid = 0u8;
        for pair in text.as_chunks::<2>().0 {
            let (high, low) = (VALUES[pair[0] as usize], VALUES[pair[1] as usize]);
            invalid |= (high | low) & 16;
            out.push(high << 4 | (low & 15));
        }
        if invalid != 0 {
            return Err("invalid hexadecimal digit".into());
        }
        Ok(out)
    }

    pub struct Hex<'a>(pub &'a [u8]);
    impl Serialize for Hex<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let span = ATTACHMENT.with(|a| {
                a.borrow_mut().as_mut().map(|attachment| {
                    let offset = attachment.len() as u64;
                    attachment.extend_from_slice(self.0);
                    (offset, self.0.len() as u64)
                })
            });
            match span {
                Some((o, n)) => Span { o, n }.serialize(serializer),
                None => serializer.serialize_str(&encode(self.0)),
            }
        }
    }
    /// Where a payload lies in the message's attachment.
    #[derive(Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Span {
        o: u64,
        n: u64,
    }

    /// Accepts a hexadecimal string, an attachment span, or serde's own
    /// array of byte values.
    pub struct Owned(pub Vec<u8>);
    impl<'de> Deserialize<'de> for Owned {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct Visitor;
            impl<'de> de::Visitor<'de> for Visitor {
                type Value = Owned;
                fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                    f.write_str("hexadecimal bytes or an attachment span")
                }
                fn visit_str<E: de::Error>(self, value: &str) -> Result<Owned, E> {
                    decode(value).map(Owned).map_err(E::custom)
                }
                fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Owned, A::Error> {
                    let mut bytes = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                    while let Some(byte) = seq.next_element::<u8>()? {
                        bytes.push(byte);
                    }
                    Ok(Owned(bytes))
                }
                fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<Owned, A::Error> {
                    let Span { o, n } =
                        Span::deserialize(de::value::MapAccessDeserializer::new(map))?;
                    ATTACHMENT.with(|a| {
                        let attachment = a.borrow();
                        let attachment = attachment.as_ref().ok_or_else(|| {
                            de::Error::custom("attachment span without attachment")
                        })?;
                        o.checked_add(n)
                            .and_then(|end| {
                                attachment.get(usize::try_from(o).ok()?..usize::try_from(end).ok()?)
                            })
                            .map(|bytes| Owned(bytes.to_vec()))
                            .ok_or_else(|| de::Error::custom("attachment span out of bounds"))
                    })
                }
            }
            deserializer.deserialize_any(Visitor)
        }
    }

    pub mod bytes {
        use super::*;
        pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
            Hex(value).serialize(serializer)
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<u8>, D::Error> {
            Owned::deserialize(deserializer).map(|bytes| bytes.0)
        }
    }

    pub mod frames {
        use super::*;
        pub fn serialize<S: Serializer>(
            value: &[Vec<u8>],
            serializer: S,
        ) -> Result<S::Ok, S::Error> {
            serializer.collect_seq(value.iter().map(|frame| Hex(frame)))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<Vec<u8>>, D::Error> {
            Vec::<Owned>::deserialize(deserializer)
                .map(|frames| frames.into_iter().map(|frame| frame.0).collect())
        }
    }

    /// `Event` with its byte payloads as hexadecimal strings.
    pub mod events {
        use super::*;
        use crate::application::Event;
        #[derive(Serialize)]
        enum Out<'a> {
            Frame(Hex<'a>),
            Control(Hex<'a>),
            Ready,
            Effect { id: u32, success: bool },
            Exit { code: u8, setup_error: bool },
            Barrier,
            Held { trigger: u32 },
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        enum In {
            Frame(Owned),
            Control(Owned),
            Ready,
            Effect { id: u32, success: bool },
            Exit { code: u8, setup_error: bool },
            Barrier,
            Held { trigger: u32 },
        }
        pub fn serialize<S: Serializer>(value: &[Event], serializer: S) -> Result<S::Ok, S::Error> {
            serializer.collect_seq(value.iter().map(|event| match event {
                Event::Frame(bytes) => Out::Frame(Hex(bytes)),
                Event::Control(bytes) => Out::Control(Hex(bytes)),
                Event::Ready => Out::Ready,
                Event::Effect { id, success } => Out::Effect {
                    id: *id,
                    success: *success,
                },
                Event::Exit { code, setup_error } => Out::Exit {
                    code: *code,
                    setup_error: *setup_error,
                },
                Event::Barrier => Out::Barrier,
                Event::Held { trigger } => Out::Held { trigger: *trigger },
            }))
        }
        pub fn deserialize<'de, D: Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Vec<Event>, D::Error> {
            Ok(Vec::<In>::deserialize(deserializer)?
                .into_iter()
                .map(|event| match event {
                    In::Frame(bytes) => Event::Frame(bytes.0),
                    In::Control(bytes) => Event::Control(bytes.0),
                    In::Ready => Event::Ready,
                    In::Effect { id, success } => Event::Effect { id, success },
                    In::Exit { code, setup_error } => Event::Exit { code, setup_error },
                    In::Barrier => Event::Barrier,
                    In::Held { trigger } => Event::Held { trigger },
                })
                .collect())
        }
    }
}

pub fn read_message<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0u8; 4];
    loop {
        match input.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("worker header: {e}")),
            _ => unreachable!(),
        }
    }
    input
        .read_exact(&mut length[1..])
        .map_err(|e| format!("worker header: {e}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("worker body: {e}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("worker message: {e}"))
}
pub fn write_message<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    // One write per message: the length prefix and body leave together, so
    // the reader is woken once rather than for a bare header.
    let mut bytes = vec![0; 4];
    serde_json::to_writer(&mut bytes, value).map_err(|e| format!("worker message: {e}"))?;
    let length = bytes.len() - 4;
    if length == 0 || length > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    bytes[..4].copy_from_slice(&(length as u32).to_le_bytes());
    output
        .write_all(&bytes)
        .and_then(|_| output.flush())
        .map_err(|e| format!("worker output: {e}"))
}

/// Write one worker channel message: the JSON body with its payload bytes as
/// one attachment, in a single write.
pub fn send<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    let mut bytes = vec![0; 8];
    let (written, attachment) = wire::attach(|| serde_json::to_writer(&mut bytes, value));
    written.map_err(|e| format!("worker message: {e}"))?;
    let body = bytes.len() - 8;
    if body == 0 || body + attachment.len() > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    bytes[..4].copy_from_slice(&(body as u32).to_le_bytes());
    bytes[4..8].copy_from_slice(&(attachment.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&attachment);
    output
        .write_all(&bytes)
        .and_then(|_| output.flush())
        .map_err(|e| format!("worker output: {e}"))
}

/// Read one worker channel message written by `send`; `None` at a clean end.
pub fn receive<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut header = [0u8; 8];
    loop {
        match input.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("worker header: {e}")),
            _ => unreachable!(),
        }
    }
    input
        .read_exact(&mut header[1..])
        .map_err(|e| format!("worker header: {e}"))?;
    let body = u32::from_le_bytes(header[..4].try_into().expect("four bytes")) as usize;
    let attached = u32::from_le_bytes(header[4..].try_into().expect("four bytes")) as usize;
    if body == 0 || body.saturating_add(attached) > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    let mut bytes = vec![0; body];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("worker body: {e}"))?;
    let mut attachment = vec![0; attached];
    input
        .read_exact(&mut attachment)
        .map_err(|e| format!("worker attachment: {e}"))?;
    wire::detach(attachment, || serde_json::from_slice(&bytes))
        .map(Some)
        .map_err(|e| format!("worker message: {e}"))
}

fn artifact_hash(file: &mut std::fs::File) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("transfer seek: {e}"))?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 << 10];
    loop {
        let count = file
            .read(&mut bytes)
            .map_err(|e| format!("transfer hash: {e}"))?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("transfer seek: {e}"))?;
    Ok(hex::encode(hash.finalize()))
}

/// Writes a file and then deallocates its runs of whole aligned zero blocks.
/// Reads return the same bytes; only the space a mostly-zero checkpoint
/// takes changes. APFS allocates gaps written past the end, so holes are
/// punched explicitly.
struct Sparse<'a> {
    file: &'a mut std::fs::File,
    position: u64,
    /// The current block's bytes, from its start in the file.
    block: Vec<u8>,
    /// Runs of zero blocks, as (offset, length).
    zeros: Vec<(u64, u64)>,
    /// Written blocks not yet passed to the file.
    out: Vec<u8>,
}
impl<'a> Sparse<'a> {
    const BLOCK: usize = 4096;
    /// Shorter runs are not worth a system call.
    const MIN_HOLE: u64 = 64 * 1024;
    const BUFFER: usize = 1 << 20;
    fn new(file: &'a mut std::fs::File) -> Self {
        Self {
            file,
            position: 0,
            block: Vec::with_capacity(Self::BLOCK),
            zeros: Vec::new(),
            out: Vec::with_capacity(Self::BUFFER),
        }
    }
    fn emit(&mut self) -> std::io::Result<()> {
        use std::io::Write as _;
        let start = self.position - self.block.len() as u64;
        if self.block.len() == Self::BLOCK && self.block.iter().all(|&b| b == 0) {
            match self.zeros.last_mut() {
                Some((offset, length)) if *offset + *length == start => {
                    *length += Self::BLOCK as u64
                }
                _ => self.zeros.push((start, Self::BLOCK as u64)),
            }
        }
        self.out.extend_from_slice(&self.block);
        self.block.clear();
        if self.out.len() >= Self::BUFFER {
            self.file.write_all(&self.out)?;
            self.out.clear();
        }
        Ok(())
    }
    fn finish(mut self) -> Result<()> {
        use std::io::Write as _;
        self.emit().map_err(|e| e.to_string())?;
        self.file.write_all(&self.out).map_err(|e| e.to_string())?;
        self.file.flush().map_err(|e| e.to_string())?;
        for &(offset, length) in self.zeros.iter().filter(|z| z.1 >= Self::MIN_HOLE) {
            punch(self.file, offset, length);
        }
        Ok(())
    }
}
/// Deallocate a zero range. Failure only costs space.
#[cfg(target_os = "macos")]
fn punch(file: &std::fs::File, offset: u64, length: u64) {
    use std::os::fd::AsRawFd;
    let hole = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: length as libc::off_t,
    };
    // SAFETY: a valid descriptor and a fully initialized argument.
    unsafe { libc::fcntl(file.as_raw_fd(), libc::F_PUNCHHOLE, &hole) };
}
#[cfg(not(target_os = "macos"))]
fn punch(_: &std::fs::File, _: u64, _: u64) {}
impl std::io::Write for Sparse<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = (Self::BLOCK - self.block.len()).min(rest.len());
            self.block.extend_from_slice(&rest[..take]);
            self.position += take as u64;
            rest = &rest[take..];
            if self.block.len() == Self::BLOCK {
                self.emit()?;
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Payload bytes after which a worker runs no further chained slices, well
/// inside the protocol's message limit.
const CHAIN_ANSWER_BYTES: usize = 4 << 20;

fn answer_bytes(answer: &SliceAnswer) -> usize {
    let events: usize = answer
        .events
        .iter()
        .map(|event| match event {
            Event::Frame(bytes) | Event::Control(bytes) => bytes.len(),
            _ => 0,
        })
        .sum();
    answer.console.len() + events
}

/// Run one slice, then, only once it has reached its limit, make exactly the
/// separate deliveries the coordinator would make next. Returns whether it
/// reached the limit.
fn run_slice<B: Backend>(
    vm: &mut Machine<B>,
    console_offset: &mut usize,
    slice: &Slice,
) -> Result<(SliceAnswer, bool)> {
    let stop = vm.run_slice(slice.hard_limit, slice.early_slack, slice.observe_console)?;
    let console = vm.console()[*console_offset..].to_vec();
    *console_offset = vm.console().len();
    let time_ns = vm.time_ns();
    let events = vm.take_events();
    let log_chunks = vm.take_log_chunks();
    let reached = matches!(stop, MachineStop::Horizon { time_ns: t } if t == slice.hard_limit);
    let accepted = if reached && !slice.then.is_empty() {
        Some(
            slice
                .then
                .iter()
                .map(|bytes| vm.receive_frame(bytes))
                .collect::<Result<Vec<_>>>()?,
        )
    } else {
        None
    };
    Ok((
        SliceAnswer {
            stop,
            time_ns,
            events,
            console,
            log_chunks,
            accepted,
            observation: vm.stopped_observation(),
        },
        reached,
    ))
}

/// All requests complete stopped. Identity and sequence are checked before any
/// physical action; errors terminate this worker rather than replaying mutation.
/// How a machine worker serves; none of it changes results.
#[derive(Clone, Copy, Debug, Default)]
pub struct ServeOptions {
    /// Restore checkpoints lazily: guest RAM maps the checkpoint file's RAM
    /// section, loaded as the guest touches it, and the file is not hashed
    /// as a whole first (the metadata and disk sections are still verified).
    pub lazy_restore: bool,
}

pub fn serve<B: Backend>(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    serve_with::<B>(input, output, ServeOptions::default())
}

pub fn serve_with<B: Backend>(
    input: &mut impl Read,
    output: &mut impl Write,
    options: ServeOptions,
) -> Result<()> {
    let mut sequence = 0u64;
    let mut identity: Option<Identity> = None;
    let mut machine: Option<Machine<B>> = None;
    let mut console_offset = 0usize;
    let mut install_available = false;
    let mut boot_inputs: Option<Boot> = None;
    while let Some(request) = receive::<Request>(input)? {
        if request.sequence != sequence {
            return Err("worker request sequence mismatch".into());
        }
        let before = identity.clone();
        match &identity {
            Some(current) if current != &request.expected => {
                return Err("worker request identity mismatch".into())
            }
            None if !matches!(request.operation, Operation::Boot(_)) => {
                return Err("unbound worker requires boot".into())
            }
            _ => (),
        }
        if request.expected.name.is_empty() {
            return Err("empty worker machine identity".into());
        }
        let terminal = matches!(
            request.operation,
            Operation::Quit | Operation::CrashExport { .. }
        );
        let result = (|| -> Result<Answer> {
            if let Operation::Boot(boot) = &request.operation {
                if identity.is_some() || request.expected.incarnation != 0 {
                    return Err("worker already bound or invalid initial incarnation".into());
                }
                Machine::<B>::label_stall_reports(&request.expected.name);
                machine = Some(boot.machine(boot.disk()?)?);
                boot_inputs = Some(boot.clone());
                identity = Some(request.expected.clone());
                install_available = true;
                return Ok(Answer::Ready);
            }
            if matches!(request.operation, Operation::Quit) {
                machine = None;
                return Ok(Answer::Bye);
            }
            if let Operation::Restore(saved) = &request.operation {
                if saved.identity.name != request.expected.name || !saved.artifact.is_absolute() {
                    return Err("checkpoint identity/path mismatch".into());
                }
                let mut file = std::fs::File::open(&saved.artifact)
                    .map_err(|e| format!("open checkpoint: {e}"))?;
                // A lazy restore reads RAM only as the guest touches it, so it
                // does not hash the whole file first.
                if !options.lazy_restore && artifact_hash(&mut file)? != saved.sha256 {
                    return Err("checkpoint artifact hash mismatch".into());
                }
                let header = read_message::<CheckpointHeader>(&mut file)?
                    .ok_or("missing checkpoint header")?;
                if header.identity != saved.identity
                    || header.observation != saved.observation
                    || header.console_offset != saved.console_offset
                {
                    return Err("checkpoint header binding mismatch".into());
                }
                let mut boot = boot_inputs
                    .clone()
                    .ok_or("missing checkpoint boot inputs")?;
                // A later incarnation booted with its own reboot entropy and no
                // launch barrier: rebuild the replacement from the boot the
                // checkpoint was captured under. Its kernel, initrd and layout
                // must still match these inputs.
                use std::io::{Seek, SeekFrom};
                let stream = file
                    .stream_position()
                    .map_err(|e| format!("checkpoint position: {e}"))?;
                let captured = Machine::<B>::checkpoint_boot_identity(&mut file)?;
                file.seek(SeekFrom::Start(stream))
                    .map_err(|e| format!("checkpoint seek: {e}"))?;
                boot.seed = captured.seed;
                boot.command_line = captured.command_line;
                // HVF allows one VM per process and requires architectural restore
                // into a never-entered replacement. Failure terminates this worker.
                drop(machine.take());
                let replacement = boot.machine::<B>(boot.disk()?)?;
                let restored = if options.lazy_restore {
                    replacement.restore_checkpoint_lazy(&mut file)?
                } else {
                    replacement.restore_checkpoint(&mut file)?
                };
                if restored.stopped_observation() != saved.observation
                    || saved.console_offset > restored.console().len()
                {
                    return Err("checkpoint restored observation mismatch".into());
                }
                machine = Some(restored);
                identity = Some(Identity {
                    incarnation: saved.identity.incarnation,
                    ..request.expected.clone()
                });
                console_offset = saved.console_offset;
                install_available = false;
                return Ok(Answer::Restored(saved.clone()));
            }
            let vm = machine.as_mut().ok_or("worker has no live machine")?;
            match request.operation {
                Operation::Advance {
                    hard_limit,
                    early_slack,
                    observe_console,
                    then,
                    ahead,
                } => {
                    install_available = false;
                    let first = Slice {
                        hard_limit,
                        early_slack,
                        observe_console,
                        then,
                    };
                    let (answer, mut reached) = run_slice(vm, &mut console_offset, &first)?;
                    let mut chained = Vec::new();
                    // Stopping early is always allowed: the coordinator then
                    // requests the remaining slices itself.
                    let mut answered = answer_bytes(&answer);
                    for slice in &ahead {
                        if !reached || answered > CHAIN_ANSWER_BYTES {
                            break;
                        }
                        let (next, next_reached) = run_slice(vm, &mut console_offset, slice)?;
                        answered += answer_bytes(&next);
                        chained.push(next);
                        reached = next_reached;
                    }
                    let SliceAnswer {
                        stop,
                        time_ns,
                        events,
                        console,
                        log_chunks,
                        accepted,
                        observation,
                    } = answer;
                    Ok(Answer::Advanced {
                        stop,
                        time_ns,
                        events,
                        console,
                        log_chunks,
                        accepted,
                        first: (!chained.is_empty()).then_some(observation),
                        ahead: chained,
                    })
                }
                Operation::Frames(frames) => {
                    install_available = false;
                    if frames.is_empty() {
                        return Err("empty frame delivery".into());
                    }
                    let accepted = frames
                        .iter()
                        .map(|bytes| vm.receive_frame(bytes))
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Answer::Accepted(accepted))
                }
                Operation::Control(bytes) => {
                    install_available = false;
                    vm.receive_control(&bytes)?;
                    Ok(Answer::Ready)
                }
                Operation::Release { at, entropy } => {
                    install_available = false;
                    if !vm.stopped_observation().barrier {
                        return Err("ARM time-fork release/reseed is unsupported".into());
                    }
                    vm.resume_at(at)?;
                    vm.release_barrier(entropy)?;
                    Ok(Answer::Ready)
                }
                Operation::ResumeAt(at) => {
                    install_available = false;
                    vm.resume_at(at)?;
                    Ok(Answer::Ready)
                }
                Operation::Park => {
                    install_available = false;
                    vm.park()?;
                    Ok(Answer::Ready)
                }
                Operation::SeedTimer { seed, domain } => {
                    vm.seed_timer(seed, &domain)?;
                    Ok(Answer::Ready)
                }
                Operation::SeedProgressJitter { seed } => {
                    vm.seed_progress_jitter(seed)?;
                    Ok(Answer::Ready)
                }
                Operation::Throttle { factor_permille } => {
                    vm.throttle(factor_permille)?;
                    Ok(Answer::Ready)
                }
                Operation::DiskErrors(errors) => Ok(Answer::DiskErrors(vm.disk_errors(errors)?)),
                Operation::DiskLatency(latency) => {
                    Ok(Answer::DiskLatency(vm.disk_latency(latency)?))
                }
                Operation::Preempt(setting) => {
                    vm.preempt(setting)?;
                    Ok(Answer::Ready)
                }
                Operation::HoldBlock(hold) => {
                    vm.hold_block(hold)?;
                    Ok(Answer::Ready)
                }
                Operation::ReseedGuest { entropy } => {
                    vm.reseed_guest(entropy)?;
                    Ok(Answer::Ready)
                }
                Operation::Evidence {
                    full_state,
                    ram_pages,
                } => {
                    let evidence = vm.application_evidence(full_state)?;
                    if let Some(path) = ram_pages {
                        let mut output = std::fs::File::create(path).map_err(|e| e.to_string())?;
                        vm.write_application_page_hashes(&mut output)?;
                    }
                    Ok(Answer::Evidence(evidence))
                }
                Operation::CrashExport {
                    run_seed,
                    branch_identity,
                    full_state,
                    transfer,
                } => {
                    transfer.validate()?;
                    if transfer.predecessor != request.expected || transfer.at < vm.time_ns() {
                        return Err("crash transaction predecessor/time mismatch".into());
                    }
                    let evidence = vm.application_evidence(full_state)?;
                    let (disk, lost_sectors) = vm
                        .crashed_disk(
                            run_seed,
                            branch_identity.as_deref(),
                            &request.expected.name,
                            request.expected.incarnation,
                        )
                        .map(|(d, n)| (Some(d), n))
                        .unwrap_or((None, 0));
                    let disk_digest = disk.as_ref().map(Disk::digest);
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&transfer.artifact)
                        .map_err(|e| {
                            format!("crash transfer {}: {e}", transfer.artifact.display())
                        })?;
                    write_message(
                        &mut file,
                        &TransferHeader {
                            descriptor: transfer.clone(),
                            disk_digest: disk_digest.clone(),
                        },
                    )?;
                    if let Some(disk) = disk {
                        disk.write_crash_transfer(&mut file)?;
                    }
                    file.flush()
                        .map_err(|e| format!("crash transfer flush: {e}"))?;
                    let artifact_sha256 = artifact_hash(&mut file)?;
                    machine = None;
                    Ok(Answer::Crashed {
                        evidence,
                        lost_sectors,
                        transfer: ExportedTransfer {
                            descriptor: transfer,
                            disk_digest,
                            artifact_sha256,
                        },
                    })
                }
                Operation::InstallSuccession(transfer) => {
                    transfer.descriptor.validate()?;
                    let next = &transfer.descriptor.successor;
                    if !install_available
                        || request.expected.incarnation != 0
                        || next.name != request.expected.name
                        || next.generation != request.expected.generation
                    {
                        return Err("successor is not a fresh uninstalled worker".into());
                    }
                    let mut file = std::fs::File::open(&transfer.descriptor.artifact)
                        .map_err(|e| format!("successor transfer: {e}"))?;
                    if artifact_hash(&mut file)? != transfer.artifact_sha256 {
                        return Err("successor artifact hash mismatch".into());
                    }
                    let header = read_message::<TransferHeader>(&mut file)?
                        .ok_or("missing successor transfer header")?;
                    if header.descriptor != transfer.descriptor
                        || header.disk_digest != transfer.disk_digest
                    {
                        return Err("successor transaction binding mismatch".into());
                    }
                    vm.install_crash_disk(&mut file, transfer.disk_digest.as_deref())?;
                    vm.resume_at(transfer.descriptor.at)?;
                    install_available = false;
                    identity = Some(next.clone());
                    Ok(Answer::Installed { transfer })
                }
                Operation::Snapshot { artifact } => {
                    use std::os::unix::fs::OpenOptionsExt;
                    if !artifact.is_absolute() {
                        return Err("checkpoint path must be absolute".into());
                    }
                    let snapshot = vm.snapshot()?;
                    let observation = vm.stopped_observation();
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&artifact)
                        .map_err(|e| format!("create checkpoint {}: {e}", artifact.display()))?;
                    // Guest RAM is mostly zero pages: leave them as holes.
                    let mut sparse = Sparse::new(&mut file);
                    write_message(
                        &mut sparse,
                        &CheckpointHeader {
                            identity: request.expected.clone(),
                            observation: observation.clone(),
                            console_offset,
                        },
                    )?;
                    let start = sparse.position;
                    snapshot.write_checkpoint(&mut sparse, start)?;
                    sparse.finish()?;
                    let sha256 = artifact_hash(&mut file)?;
                    Ok(Answer::Snapshot(Checkpoint {
                        artifact,
                        identity: request.expected.clone(),
                        observation,
                        console_offset,
                        sha256,
                    }))
                }
                _ => Err("invalid worker operation".into()),
            }
        })();
        let failed = result.is_err();
        let answer = result.unwrap_or_else(Answer::Error);
        let observation = if failed {
            None
        } else {
            machine.as_ref().map(Machine::stopped_observation)
        };
        send(
            output,
            &Response {
                sequence,
                before,
                after: identity.clone(),
                observation,
                answer,
            },
        )?;
        if terminal || failed {
            return Ok(());
        }
        sequence = sequence.checked_add(1).ok_or("worker sequence overflow")?;
    }
    Ok(())
}

/// Owned process with sticky protocol failure. No uncertain mutation is retried.
pub struct OwnedWorker {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<std::io::BufReader<ChildStdout>>,
    sequence: u64,
    identity: Identity,
    bound: bool,
    observation: Option<StoppedObservation>,
    poison: Option<String>,
    /// Chained slices the worker already ran, each with its answer and the
    /// stopped facts after it, served to the matching next requests.
    chained: std::collections::VecDeque<(Slice, Answer, StoppedObservation)>,
    /// A snapshot or restore request sent by `begin`, awaiting `finish`.
    begun: Option<Request>,
}
impl OwnedWorker {
    pub fn spawn(executable: &std::path::Path, identity: Identity) -> Result<Self> {
        Self::spawn_with(executable, identity, ServeOptions::default())
    }
    /// A worker that serves with `options` (`--lazy-restore`).
    pub fn spawn_with(
        executable: &std::path::Path,
        identity: Identity,
        options: ServeOptions,
    ) -> Result<Self> {
        let mut command = Command::new(executable);
        command.args(["__engine", CONTRACT]);
        if options.lazy_restore {
            command.arg("--lazy-restore");
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("machine worker {}: {e}", executable.display()))?;
        let Some((input, output)) = child.stdin.take().zip(child.stdout.take()) else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("machine worker pipes unavailable".into());
        };
        Ok(Self {
            child,
            input: Some(input),
            output: Some(std::io::BufReader::with_capacity(64 << 10, output)),
            sequence: 0,
            identity,
            bound: false,
            observation: None,
            poison: None,
            chained: std::collections::VecDeque::new(),
            begun: None,
        })
    }
    pub fn ensure_active(&self) -> Result<()> {
        match &self.poison {
            Some(error) => Err(format!("machine worker is terminal: {error}")),
            None => Ok(()),
        }
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn observation(&self) -> Option<&StoppedObservation> {
        self.observation.as_ref()
    }
    /// Chained slices already run and not yet requested.
    pub fn chained(&self) -> usize {
        self.chained.len()
    }
    pub fn invalidate(&mut self, error: &str) {
        self.poison.get_or_insert_with(|| error.into());
        if let Err(cleanup) = self.terminate() {
            self.poison = Some(format!("{error}; {cleanup}"));
        }
    }
    /// Poison first, close both pipes, then kill and join the owned process.
    /// Idempotent and never sends a protocol operation during uncertain cleanup.
    pub fn terminate(&mut self) -> Result<()> {
        self.poison
            .get_or_insert_with(|| "worker terminated".into());
        self.input = None;
        self.output = None;
        match self
            .child
            .try_wait()
            .map_err(|e| format!("worker status: {e}"))?
        {
            Some(_) => Ok(()),
            None => {
                if let Err(kill) = self.child.kill() {
                    if self
                        .child
                        .try_wait()
                        .map_err(|e| format!("worker status after kill: {e}"))?
                        .is_none()
                    {
                        return Err(format!("worker kill: {kill}"));
                    }
                }
                self.child.wait().map_err(|e| format!("worker reap: {e}"))?;
                Ok(())
            }
        }
    }
    pub fn call(&mut self, operation: Operation) -> Result<Answer> {
        if let Some(error) = &self.poison {
            return Err(format!("machine worker is terminal: {error}"));
        }
        match self.exchange(operation) {
            Ok(answer) => Ok(answer),
            Err(error) => {
                self.invalidate(&error);
                Err(self.poison.clone().unwrap_or(error))
            }
        }
    }
    /// Send a snapshot or restore request without waiting for its answer;
    /// `finish` takes it. Other operations use `call`.
    pub fn begin(&mut self, operation: Operation) -> Result<()> {
        if let Some(error) = &self.poison {
            return Err(format!("machine worker is terminal: {error}"));
        }
        let result = (|| -> Result<()> {
            if !matches!(
                operation,
                Operation::Snapshot { .. } | Operation::Restore(_)
            ) {
                return Err("only a snapshot or restore can be begun".into());
            }
            if !self.bound || !self.chained.is_empty() || self.begun.is_some() {
                return Err("worker cannot begin a request now".into());
            }
            let request = Request {
                sequence: self.sequence,
                expected: self.identity.clone(),
                operation,
            };
            send(self.input.as_mut().ok_or("worker input closed")?, &request)?;
            self.begun = Some(request);
            Ok(())
        })();
        if let Err(error) = &result {
            self.invalidate(error);
        }
        result
    }
    /// The answer to the request `begin` sent.
    pub fn finish(&mut self) -> Result<Answer> {
        if let Some(error) = &self.poison {
            return Err(format!("machine worker is terminal: {error}"));
        }
        let request = self.begun.take().ok_or("no begun worker request")?;
        match self.complete(request) {
            Ok(answer) => Ok(answer),
            Err(error) => {
                self.invalidate(&error);
                Err(self.poison.clone().unwrap_or(error))
            }
        }
    }
    /// Whether a begun request awaits `finish`.
    pub fn begun(&self) -> bool {
        self.begun.is_some()
    }
    fn exchange(&mut self, operation: Operation) -> Result<Answer> {
        if !self.chained.is_empty() {
            // Only the exact next chained slice may be requested.
            let Operation::Advance {
                hard_limit,
                early_slack,
                observe_console,
                then,
                ahead,
            } = operation
            else {
                return Err("worker operation while chained slices are pending".into());
            };
            let (slice, answer, observation) = self.chained.pop_front().expect("pending slice");
            let requested = Slice {
                hard_limit,
                early_slack,
                observe_console,
                then,
            };
            if !ahead.is_empty() || requested != slice {
                return Err("worker request differs from the chained slice".into());
            }
            self.observation = Some(observation);
            return Ok(answer);
        }
        if !self.bound && !matches!(operation, Operation::Boot(_)) {
            return Err("unbound worker requires boot".into());
        }
        if self.bound && matches!(operation, Operation::Boot(_)) {
            return Err("worker is already bound".into());
        }
        if let Operation::InstallSuccession(transfer) = &operation {
            transfer.descriptor.validate()?;
            if self.identity.incarnation != 0
                || transfer.descriptor.successor.name != self.identity.name
                || transfer.descriptor.successor.generation != self.identity.generation
            {
                return Err("worker succession identity mismatch".into());
            }
        }
        if let Operation::CrashExport { transfer, .. } = &operation {
            transfer.validate()?;
            if transfer.predecessor != self.identity {
                return Err("worker crash predecessor mismatch".into());
            }
        }
        if let Operation::Advance { hard_limit, .. } = &operation {
            if *hard_limit == u64::MAX
                || self
                    .observation
                    .as_ref()
                    .is_some_and(|o| o.time_ns > *hard_limit)
            {
                return Err("invalid worker slice hard limit".into());
            }
        }
        if self.begun.is_some() {
            return Err("worker request while another awaits its answer".into());
        }
        let request = Request {
            sequence: self.sequence,
            expected: self.identity.clone(),
            operation,
        };
        send(self.input.as_mut().ok_or("worker input closed")?, &request)?;
        self.complete(request)
    }
    /// Receive and validate the answer to `request`, already sent.
    fn complete(&mut self, request: Request) -> Result<Answer> {
        let mut response =
            receive::<Response>(self.output.as_mut().ok_or("worker output closed")?)?
                .ok_or("machine worker closed its output")?;
        let chained = self.split_chain(&request, &mut response)?;
        self.validate_response(&request, &response)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("worker sequence overflow")?;
        self.identity = response.after.clone().ok_or("missing worker identity")?;
        self.bound = true;
        self.observation = response.observation;
        self.chained = chained;
        if matches!(response.answer, Answer::Crashed { .. } | Answer::Bye) {
            self.terminate()
                .map_err(|e| format!("acknowledged terminal operation; {e}"))?;
        }
        Ok(response.answer)
    }
    /// Split a chained advance into the response to its first slice and,
    /// for each later slice, the answer a separate request would have had.
    /// Each is validated as that separate exchange, in order.
    fn split_chain(
        &mut self,
        request: &Request,
        response: &mut Response,
    ) -> Result<std::collections::VecDeque<(Slice, Answer, StoppedObservation)>> {
        let mut chained = std::collections::VecDeque::new();
        let Operation::Advance { ahead: slices, .. } = &request.operation else {
            return Ok(chained);
        };
        let Answer::Advanced { ahead, first, .. } = &mut response.answer else {
            return Ok(chained);
        };
        let answers = std::mem::take(ahead);
        let first = first.take();
        if answers.len() > slices.len() || answers.is_empty() != first.is_none() {
            return Err("worker chained slices do not match the request".into());
        }
        let Some(first) = first else {
            return Ok(chained);
        };
        let last = response.observation.replace(first);
        let previous = self.observation.clone();
        let result = (|| -> Result<()> {
            self.validate_response(request, response)?;
            self.observation = response.observation.clone();
            let count = answers.len();
            for (index, (slice, answer)) in slices.iter().zip(answers).enumerate() {
                let observation = answer.observation;
                if index + 1 == count && last.as_ref() != Some(&observation) {
                    return Err("worker chain observation disagrees with its last slice".into());
                }
                let single = Request {
                    sequence: request.sequence,
                    expected: request.expected.clone(),
                    operation: Operation::Advance {
                        hard_limit: slice.hard_limit,
                        early_slack: slice.early_slack,
                        observe_console: slice.observe_console,
                        then: slice.then.clone(),
                        ahead: Vec::new(),
                    },
                };
                let separate = Response {
                    sequence: response.sequence,
                    before: response.before.clone(),
                    after: response.after.clone(),
                    observation: Some(observation.clone()),
                    answer: Answer::Advanced {
                        stop: answer.stop,
                        time_ns: answer.time_ns,
                        events: answer.events,
                        console: answer.console,
                        log_chunks: answer.log_chunks,
                        accepted: answer.accepted,
                        ahead: Vec::new(),
                        first: None,
                    },
                };
                self.validate_response(&single, &separate)?;
                self.observation = Some(observation.clone());
                chained.push_back((slice.clone(), separate.answer, observation));
            }
            Ok(())
        })();
        self.observation = previous;
        result?;
        // A slice after one that stopped short is never run.
        let reached = |answer: &Answer, slice_limit: u64| matches!(answer, Answer::Advanced { stop: MachineStop::Horizon { time_ns }, .. } if *time_ns == slice_limit);
        let Operation::Advance { hard_limit, .. } = &request.operation else {
            unreachable!()
        };
        let mut limit = *hard_limit;
        let mut answer = &response.answer;
        for (slice, (_, next, _)) in slices.iter().zip(&chained) {
            if !reached(answer, limit) {
                return Err("worker ran a chained slice after one that stopped short".into());
            }
            limit = slice.hard_limit;
            answer = next;
        }
        Ok(chained)
    }

    fn validate_response(&self, request: &Request, response: &Response) -> Result<()> {
        let expected_before = self.bound.then_some(&self.identity);
        let restored_identity = match &request.operation {
            Operation::Restore(saved) => Some(Identity {
                incarnation: saved.identity.incarnation,
                ..self.identity.clone()
            }),
            _ => None,
        };
        let expected_after = match &request.operation {
            Operation::InstallSuccession(t) => &t.descriptor.successor,
            Operation::Restore(_) => restored_identity.as_ref().expect("restore identity"),
            _ => &self.identity,
        };
        // A failed operation leaves the worker's identity unchanged, so a
        // failed boot answers unbound: report the worker's own error.
        if response.sequence == request.sequence {
            if let Answer::Error(error) = &response.answer {
                return Err(error.clone());
            }
        }
        if response.sequence != request.sequence
            || response.before.as_ref() != expected_before
            || response.after.as_ref() != Some(expected_after)
        {
            let op = serde_json::to_value(&request.operation)
                .ok()
                .and_then(|v| match v {
                    serde_json::Value::String(name) => Some(name),
                    serde_json::Value::Object(map) => map.keys().next().cloned(),
                    _ => None,
                })
                .unwrap_or_default();
            return Err(format!(
                "worker response stamp mismatch for {op}: expected {expected_before:?} -> {expected_after:?}, got {:?} -> {:?}",
                response.before, response.after
            ));
        }
        let kind_matches = matches!(
            (&request.operation, &response.answer),
            (
                Operation::Boot(_)
                    | Operation::Control(_)
                    | Operation::Release { .. }
                    | Operation::ResumeAt(_)
                    | Operation::Park
                    | Operation::SeedTimer { .. }
                    | Operation::SeedProgressJitter { .. }
                    | Operation::Throttle { .. }
                    | Operation::HoldBlock(_)
                    | Operation::Preempt(_)
                    | Operation::ReseedGuest { .. },
                Answer::Ready
            ) | (Operation::Advance { .. }, Answer::Advanced { .. })
                | (Operation::Frames(_), Answer::Accepted(_))
                | (Operation::DiskErrors(_), Answer::DiskErrors(_))
                | (Operation::DiskLatency(_), Answer::DiskLatency(_))
                | (Operation::Evidence { .. }, Answer::Evidence(_))
                | (Operation::CrashExport { .. }, Answer::Crashed { .. })
                | (Operation::InstallSuccession(_), Answer::Installed { .. })
                | (Operation::Snapshot { .. }, Answer::Snapshot(_))
                | (Operation::Restore(_), Answer::Restored(_))
                | (Operation::Quit, Answer::Bye)
        );
        if !kind_matches {
            return Err("unexpected worker answer kind".into());
        }
        if let (Operation::Frames(frames), Answer::Accepted(accepted)) =
            (&request.operation, &response.answer)
        {
            if frames.is_empty() || accepted.len() != frames.len() {
                return Err("worker frame results do not match the delivered frames".into());
            }
        }
        let terminal = matches!(
            request.operation,
            Operation::CrashExport { .. } | Operation::Quit
        );
        if terminal != response.observation.is_none() {
            return Err("worker stopped observation missing or unexpected".into());
        }
        if let Some(observation) = &response.observation {
            if observation.time_ns == u64::MAX
                || (!matches!(request.operation, Operation::Restore(_))
                    && self.observation.as_ref().is_some_and(|old| {
                        observation.time_ns < old.time_ns || observation.exits < old.exits
                    }))
            {
                return Err("worker stopped observation moved backwards".into());
            }
            if observation.owed_io.is_some() {
                return Err("ARM worker unexpectedly owes delayed I/O".into());
            }
            if observation.powered_off as u8
                + observation.exit.is_some() as u8
                + observation.barrier as u8
                > 1
            {
                return Err("worker reported conflicting physical stop states".into());
            }
            match &request.operation {
                Operation::Snapshot { artifact } => {
                    let Answer::Snapshot(saved) = &response.answer else {
                        unreachable!()
                    };
                    if saved.artifact != *artifact
                        || saved.identity != request.expected
                        || &saved.observation != observation
                        || self.observation.as_ref() != Some(observation)
                        || saved.sha256.len() != 64
                        || !saved.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Err("checkpoint acknowledgement mismatch".into());
                    }
                }
                Operation::Restore(saved) => {
                    if saved.identity.name != request.expected.name
                        || &saved.observation != observation
                        || !matches!(&response.answer, Answer::Restored(ack) if ack == saved)
                    {
                        return Err("checkpoint restore acknowledgement mismatch".into());
                    }
                }
                Operation::Boot(_) => {
                    if observation.time_ns != 0
                        || observation.exits != 0
                        || observation.abi != 0
                        || observation.powered_off
                        || observation.exit.is_some()
                        || observation.barrier
                    {
                        return Err("invalid initial worker observation".into());
                    }
                }
                Operation::InstallSuccession(transfer) => {
                    if observation.time_ns != transfer.descriptor.at
                        || observation.exits != 0
                        || observation.abi != 0
                        || observation.powered_off
                        || observation.exit.is_some()
                        || observation.barrier
                    {
                        return Err("invalid fresh successor observation".into());
                    }
                    if !matches!(&response.answer, Answer::Installed { transfer:ack } if ack == transfer)
                    {
                        return Err("successor acknowledgement transfer mismatch".into());
                    }
                }
                Operation::Advance { .. } => {
                    let old = self
                        .observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?;
                    if observation.has_control != old.has_control
                        || !(observation.abi == old.abi
                            || (old.abi == 0 && observation.abi == crate::ABI))
                    {
                        return Err("worker slice changed control capability or invalid ABI".into());
                    }
                }
                operation => {
                    let old = self
                        .observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?;
                    let expected_time = match operation {
                        Operation::ResumeAt(at) | Operation::Release { at, .. } => *at,
                        _ => old.time_ns,
                    };
                    let expected_barrier = if matches!(operation, Operation::Release { .. }) {
                        false
                    } else {
                        old.barrier
                    };
                    let preserves_deadline = matches!(
                        operation,
                        Operation::Frames(_)
                            | Operation::Control(_)
                            | Operation::Park
                            | Operation::SeedTimer { .. }
                            | Operation::SeedProgressJitter { .. }
                            | Operation::Throttle { .. }
                            | Operation::DiskErrors(_)
                            | Operation::DiskLatency(_)
                            | Operation::Preempt(_)
                            | Operation::HoldBlock(_)
                            | Operation::ReseedGuest { .. }
                            | Operation::Evidence { .. }
                    );
                    let preserves_irq = matches!(
                        operation,
                        Operation::Park
                            | Operation::SeedTimer { .. }
                            | Operation::SeedProgressJitter { .. }
                            | Operation::Throttle { .. }
                            | Operation::DiskErrors(_)
                            | Operation::DiskLatency(_)
                            | Operation::Preempt(_)
                            | Operation::HoldBlock(_)
                            | Operation::ReseedGuest { .. }
                            | Operation::Evidence { .. }
                    );
                    if (preserves_deadline && observation.next_deadline != old.next_deadline)
                        || (preserves_irq && observation.pending_irq != old.pending_irq)
                    {
                        return Err(
                            "worker mutation changed unrelated interrupt observations".into()
                        );
                    }
                    if observation.time_ns != expected_time
                        || observation.exits != old.exits
                        || observation.abi != old.abi
                        || observation.powered_off != old.powered_off
                        || observation.exit != old.exit
                        || observation.barrier != expected_barrier
                        || observation.has_control != old.has_control
                    {
                        return Err("worker mutation changed unrelated stopped facts".into());
                    }
                }
            }
            if let (Operation::Evidence { full_state, .. }, Answer::Evidence(evidence)) =
                (&request.operation, &response.answer)
            {
                validate_evidence(evidence, observation, *full_state)?;
            }
            if let Operation::Advance {
                hard_limit,
                observe_console,
                then,
                ..
            } = &request.operation
            {
                let Answer::Advanced {
                    stop,
                    time_ns,
                    console,
                    events,
                    log_chunks,
                    accepted,
                    ..
                } = &response.answer
                else {
                    unreachable!()
                };
                let reached =
                    matches!(stop, MachineStop::Horizon { time_ns: t } if t == hard_limit);
                let delivered_ok = match accepted {
                    None => then.is_empty() || !reached,
                    Some(accepted) => reached && accepted.len() == then.len() && !then.is_empty(),
                };
                if !delivered_ok {
                    return Err("worker slice frame results do not match the slice stop".into());
                }
                if observation.time_ns != *time_ns || *time_ns > *hard_limit {
                    return Err(
                        "worker slice exceeds hard limit or disagrees with observation".into(),
                    );
                }
                let management = events.iter().try_fold(0usize, |length, event| {
                    length
                        .checked_add(match event {
                            Event::Control(bytes) => bytes.len(),
                            _ => 0,
                        })
                        .ok_or("worker management output length overflow")
                })?;
                super::log_capture::validate(
                    log_chunks,
                    console.len(),
                    management,
                    self.observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?
                        .time_ns,
                    *time_ns,
                    *observe_console,
                )?;
                let stop_valid = match stop {
                    MachineStop::Horizon { time_ns: t } => {
                        *t == *time_ns
                            && *t == *hard_limit
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Device { time_ns: t } => {
                        *t == *time_ns
                            && !events.is_empty()
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Console { time_ns: t } => {
                        *t == *time_ns
                            && *observe_console
                            && console.len() == 1
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Held {
                        time_ns: t,
                        trigger,
                    } => {
                        *t == *time_ns
                            && events.contains(&Event::Held { trigger: *trigger })
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::PoweredOff => observation.powered_off,
                    MachineStop::ApplicationExit { code, setup_error } => {
                        observation.exit == Some((*code, *setup_error))
                    }
                    MachineStop::Barrier => observation.barrier,
                };
                if !stop_valid {
                    return Err("worker stop and physical observation disagree".into());
                }
            }
        }
        if let (
            Operation::CrashExport {
                transfer: expected, ..
            },
            Answer::Crashed { transfer, .. },
        ) = (&request.operation, &response.answer)
        {
            expected.validate()?;
            if &transfer.descriptor != expected
                || expected.predecessor != self.identity
                || !valid_hash(&transfer.artifact_sha256)
                || transfer
                    .disk_digest
                    .as_ref()
                    .is_some_and(|hash| !valid_hash(hash))
            {
                return Err("worker crash transfer binding mismatch".into());
            }
        }
        if let (Operation::CrashExport { full_state, .. }, Answer::Crashed { evidence, .. }) =
            (&request.operation, &response.answer)
        {
            validate_evidence(
                evidence,
                self.observation
                    .as_ref()
                    .ok_or("missing crash observation")?,
                *full_state,
            )?;
        }
        if let Operation::InstallSuccession(transfer) = &request.operation {
            transfer.descriptor.validate()?;
            if self.identity.incarnation != 0
                || transfer.descriptor.successor.name != self.identity.name
                || transfer.descriptor.successor.generation != self.identity.generation
            {
                return Err("worker succession identity mismatch".into());
            }
        }
        Ok(())
    }
}
fn validate_evidence(
    evidence: &Evidence,
    observation: &StoppedObservation,
    full_state: bool,
) -> Result<()> {
    if evidence.time_ns != observation.time_ns
        || evidence.exits != observation.exits
        || evidence.format != 1
        || evidence.state_digest.is_some() != full_state
        || evidence.state_digest_parts.is_some() != full_state
        || !valid_hash(&evidence.witness)
        || !valid_hash(&evidence.trace_hash)
        || !valid_hash(&evidence.console_hash)
        || !valid_hash(&evidence.kernel_sha256)
        || !valid_hash(&evidence.initrd_sha256)
        || evidence
            .disk_digest
            .as_ref()
            .is_some_and(|hash| !valid_hash(hash))
        || evidence
            .state_digest
            .as_ref()
            .is_some_and(|hash| !valid_hash(hash))
        || evidence
            .state_digest_parts
            .as_ref()
            .is_some_and(|parts| parts.values().any(|hash| !valid_hash(hash)))
    {
        return Err("worker evidence disagrees with acknowledged physical state".into());
    }
    Ok(())
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
impl Drop for OwnedWorker {
    fn drop(&mut self) {
        self.invalidate("worker dropped");
    }
}

#[cfg(test)]
mod sparse_tests {
    use super::Sparse;
    use std::io::Write;

    #[test]
    fn sparse_writes_read_back_identically_and_leave_zero_blocks_unallocated() {
        use std::os::unix::fs::MetadataExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("checkpoint");
        let mut expected = vec![7u8; 100];
        expected.extend(vec![0u8; 64 * 4096]);
        expected.extend(vec![9u8; 5000]);
        expected.extend(vec![0u8; 3 * 4096]);
        let mut file = std::fs::File::create(&path).unwrap();
        let mut sparse = Sparse::new(&mut file);
        // Unaligned writes of every size cross block boundaries.
        for chunk in expected.chunks(3001) {
            sparse.write_all(chunk).unwrap();
        }
        sparse.finish().unwrap();
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        let allocated = std::fs::metadata(&path).unwrap().blocks() * 512;
        assert!(
            allocated < expected.len() as u64 / 4,
            "{allocated} bytes allocated"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(name: &str, generation: u64, incarnation: u32) -> Identity {
        Identity {
            name: name.into(),
            generation,
            incarnation,
        }
    }
    #[test]
    fn transfer_identity_requires_same_machine_distinct_generation_and_checked_successor() {
        let mut descriptor = TransferDescriptor {
            transaction: 1,
            predecessor: identity("a", 1, 0),
            successor: identity("a", 2, 1),
            artifact: PathBuf::from("/tmp/owned-transfer"),
            at: 10,
        };
        descriptor.validate().unwrap();
        descriptor.successor.name = "b".into();
        assert!(descriptor.validate().is_err());
        descriptor.successor.name = "a".into();
        descriptor.successor.generation = 1;
        assert!(descriptor.validate().is_err());
        descriptor.successor.generation = 2;
        descriptor.predecessor.incarnation = u32::MAX;
        descriptor.successor.incarnation = 0;
        assert!(descriptor.validate().is_err());
    }
    #[test]
    fn a_failed_boot_reports_the_worker_error_not_a_stamp_mismatch() {
        let worker = OwnedWorker {
            child: Command::new("true").spawn().unwrap(),
            input: None,
            output: None,
            sequence: 0,
            identity: identity("a", 1, 0),
            bound: false,
            observation: None,
            poison: None,
            chained: Default::default(),
            begun: None,
        };
        let request = Request {
            sequence: 0,
            expected: identity("a", 1, 0),
            operation: Operation::Quit,
        };
        let response = |sequence, answer| Response {
            sequence,
            before: None,
            after: None,
            observation: None,
            answer,
        };
        assert_eq!(
            worker
                .validate_response(&request, &response(0, Answer::Error("hv_vm_create".into())))
                .unwrap_err(),
            "hv_vm_create"
        );
        for (sequence, answer) in [(1, Answer::Error("stale".into())), (0, Answer::Bye)] {
            let error = worker
                .validate_response(&request, &response(sequence, answer))
                .unwrap_err();
            assert!(
                error.starts_with("worker response stamp mismatch for Quit"),
                "{error}"
            );
        }
    }
    #[test]
    fn bounded_frames_reject_truncation_before_deserialization() {
        for bytes in [
            vec![1],
            vec![5, 0, 0, 0, b'{'],
            vec![0; 4],
            ((MAX_MESSAGE + 1) as u32).to_le_bytes().to_vec(),
        ] {
            assert!(read_message::<Request>(&mut bytes.as_slice()).is_err());
        }
        assert!(read_message::<Request>(&mut [].as_slice())
            .unwrap()
            .is_none());
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &Request {
                sequence: 3,
                expected: identity("a", 2, 0),
                operation: Operation::Quit,
            },
        )
        .unwrap();
        let request = read_message::<Request>(&mut bytes.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(request.sequence, 3);
        assert_eq!(request.expected, identity("a", 2, 0));
    }
    #[test]
    fn channel_messages_carry_payloads_in_one_bounded_attachment() {
        let frame: Vec<u8> = (0..=255).cycle().take(1514).collect();
        let request = Request {
            sequence: 9,
            expected: identity("a", 1, 0),
            operation: Operation::Advance {
                hard_limit: 7,
                early_slack: 0,
                observe_console: true,
                then: vec![frame.clone(), Vec::new()],
                ahead: vec![Slice {
                    hard_limit: 8,
                    early_slack: 1,
                    observe_console: false,
                    then: vec![frame[..60].to_vec()],
                }],
            },
        };
        let mut bytes = Vec::new();
        send(&mut bytes, &request).unwrap();
        // One write: both lengths, the JSON body, then the raw payloads.
        let body = u32::from_le_bytes(bytes[..4].try_into().unwrap()) as usize;
        let attached = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(attached, 1514 + 60);
        assert_eq!(bytes.len(), 8 + body + attached);
        assert!(body < 400, "payload bytes must not be in the JSON body");
        let Some(Request {
            operation:
                Operation::Advance {
                    then,
                    ahead,
                    observe_console: true,
                    ..
                },
            ..
        }) = receive::<Request>(&mut bytes.as_slice()).unwrap()
        else {
            panic!("advance did not round-trip");
        };
        assert_eq!(then, vec![frame.clone(), Vec::new()]);
        assert_eq!(ahead[0].then, vec![frame[..60].to_vec()]);
        assert_eq!((ahead[0].hard_limit, ahead[0].early_slack), (8, 1));

        let events = vec![
            Event::Frame(frame.clone()),
            Event::Control(b"line\n".to_vec()),
            Event::Effect {
                id: 3,
                success: true,
            },
        ];
        let answer = Answer::Advanced {
            stop: MachineStop::Device { time_ns: 5 },
            time_ns: 5,
            events: events.clone(),
            console: b"ok".to_vec(),
            log_chunks: Vec::new(),
            accepted: None,
            ahead: Vec::new(),
            first: None,
        };
        let mut bytes = Vec::new();
        send(&mut bytes, &answer).unwrap();
        let Some(Answer::Advanced {
            events: received,
            console,
            ..
        }) = receive::<Answer>(&mut bytes.as_slice()).unwrap()
        else {
            panic!("answer did not round-trip");
        };
        assert_eq!((received, console), (events, b"ok".to_vec()));
        // Truncated payloads and a span beyond the attachment are rejected.
        let mut short = bytes.clone();
        short.pop();
        assert!(receive::<Answer>(&mut short.as_slice()).is_err());
        let mut lying = bytes;
        let attached = u32::from_le_bytes(lying[4..8].try_into().unwrap());
        lying[4..8].copy_from_slice(&(attached - 1).to_le_bytes());
        lying.pop();
        assert!(receive::<Answer>(&mut lying.as_slice())
            .unwrap_err()
            .contains("out of bounds"));
    }
    #[test]
    fn payloads_outside_the_channel_are_hexadecimal_and_byte_arrays_still_parse() {
        let operation = Operation::Frames(vec![vec![0, 0xab, 0xff]]);
        let json = serde_json::to_string(&operation).unwrap();
        assert_eq!(json, r#"{"Frames":["00abff"]}"#);
        let Operation::Frames(frames) = serde_json::from_str(&json).unwrap() else {
            panic!("frames did not round-trip");
        };
        assert_eq!(frames, vec![vec![0, 0xab, 0xff]]);
        let Operation::Frames(frames) = serde_json::from_str(r#"{"Frames":[[1,2,3]]}"#).unwrap()
        else {
            panic!("byte arrays must parse");
        };
        assert_eq!(frames, vec![vec![1, 2, 3]]);
        for bad in [
            r#"{"Frames":["0g"]}"#,
            r#"{"Frames":["abc"]}"#,
            r#"{"Frames":[{"o":0,"n":1}]}"#,
        ] {
            assert!(serde_json::from_str::<Operation>(bad).is_err(), "{bad}");
        }
    }
    #[test]
    fn structured_disk_fault_policy_survives_worker_boot_serialization() {
        for probability in [0.0, 0.5, 1.0] {
            let model = crate::disk::parse_disk_fault_model(
                &serde_json::json!({"sector_bytes":512,"survival_probability":probability}),
            )
            .unwrap();
            let boot = Boot {
                kernel: "kernel".into(),
                initrd: "initrd".into(),
                seed: 7,
                memory_bytes: 128 << 20,
                command_line: String::new(),
                mac: [2, 0, 0, 0, 0, 1],
                disk_image: None,
                disk_sectors: Some(2),
                disk_fault_model: model,
                timer_policy: None,
                progress: None,
            };
            let boot: Boot = serde_json::from_slice(&serde_json::to_vec(&boot).unwrap()).unwrap();
            let mut disk = boot.disk().unwrap().unwrap();
            disk.write(0, &[1; 512]).unwrap();
            disk.flush();
            disk.write(1, &[2; 512]).unwrap();
            let (first, _) = disk.crashed_seeded(1009, "a", 0);
            let (again, _) = disk.crashed_seeded(1009, "a", 0);
            assert_eq!(first.digest(), again.digest());
            assert_eq!(first.fault_model(), model);
        }
    }
}
