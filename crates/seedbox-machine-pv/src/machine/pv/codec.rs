//! PV snapshot v3 retains native interaction chain plus named policy, boot ownership, ended evidence/page
//! records and pending reports outside the bounded private JSON header.
//! Layout: fixed host/format header, closed metadata, six raw lengths,
//! then kernel, initrd, page+mapped RAM, native overlays, client overlays, base.
//! All lengths precede raw bodies, allowing one exact retained-body reservation.
use super::*;
use seedbox_machine::snapshot::{
    reserve, CodecError, DecodeLimits, Reader, Record, Result, Writer,
};
use std::io::{Read, Write};
const MAGIC: &[u8; 8] = b"SBXPVS01";
const HEADER_BYTES: u64 = 24;
const VERSION: u32 = 3;
#[cfg(target_arch = "x86_64")]
const ARCH: u8 = 1;
#[cfg(target_arch = "aarch64")]
const ARCH: u8 = 2;
#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
const NATIVE: u8 = 1;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const NATIVE: u8 = 2;
#[cfg(all(target_arch = "aarch64", target_os = "linux"))]
const NATIVE: u8 = 0;
const PAGE: u64 = 16384;

struct Count {
    bytes: u64,
    exceeded: bool,
}
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > protocol::MAX_MESSAGE as u64 - self.bytes {
            self.exceeded = true;
            return Err(std::io::Error::other("PV metadata cap"));
        }
        self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
pub(super) fn json_len<T: Serialize>(value: &T) -> Result<usize> {
    let mut count = Count {
        bytes: 0,
        exceeded: false,
    };
    let result = serde_json::to_writer(&mut count, value);
    if count.exceeded {
        return Err(CodecError::Limit("private metadata bytes"));
    }
    result.map_err(|_| CodecError::Invalid("private metadata serialization"))?;
    usize::try_from(count.bytes).map_err(|_| CodecError::Limit("host metadata length"))
}
/// Caller reserves the counted metadata plus all following raw bodies first.
pub(super) fn json_write<T: Serialize>(value: &T, out: &mut Vec<u8>) -> Result<()> {
    let n = json_len(value)?;
    out.extend_from_slice(&(n as u32).to_le_bytes());
    serde_json::to_writer(out, value)
        .map_err(|_| CodecError::Invalid("private metadata serialization"))
}
fn invalid(what: &'static str) -> CodecError {
    CodecError::Invalid(what)
}
fn sum(values: &[u64]) -> Result<u64> {
    values.iter().try_fold(0u64, |n, v| {
        n.checked_add(*v).ok_or(invalid("length overflow"))
    })
}
fn host(n: u64) -> Result<usize> {
    usize::try_from(n).map_err(|_| CodecError::Limit("host body length"))
}
fn validate(c: &Capture, disk: Option<&DiskImage>) -> Result<()> {
    if c.layout_version != boot::LAYOUT_VERSION {
        return Err(invalid("RAM layout version"));
    }
    boot::RamLayout::native(c.memory_bytes).map_err(|_| invalid("RAM layout"))?;
    boot::arguments(&c.command_line).map_err(|_| invalid("kernel arguments"))?;
    if let Some(mac) = c.network {
        seedbox_machine::definition::MacAddress::new(mac).map_err(|_| invalid("network MAC"))?;
    }
    if c.disk != disk.map(DiskImage::sectors) {
        return Err(invalid("disk base capacity"));
    }
    if c.state == State::Invalid
        || (!c.live && c.pending_boots == 0 && !matches!(c.state, State::Exited | State::Crashed))
        || (c.live && !matches!(c.state, State::Ready | State::Blocked(_)))
        || matches!(c.state,State::Blocked(Some(d)) if d<=c.reached)
    {
        return Err(invalid("machine state or blocked deadline"));
    }
    // Successful run transfers accepted input, ordered writes and prepared
    // replies; constructors are the sole pending-boot capture boundary.
    // TODO: Re-evaluate if input delivery ever moves beyond the end of run.
    if !c.input.is_empty()
        || !c.writes.is_empty()
        || c.pending_boots > 1
        || c.pending_at.is_some()
        || (c.pending_boots == 1
            && (c.live || c.reached != 0 || c.seed != 0 || c.state != State::Ready || c.shutdown))
    {
        return Err(invalid("capture boundary ownership"));
    }
    Ok(())
}
fn validate_overlay(bytes: &[u8], sectors: Option<u64>) -> Result<()> {
    match sectors {
        Some(capacity) => seedbox_machine::machine::disk::codec::validate_overlays(bytes, capacity)
            .map_err(|_| invalid("disk overlays")),
        None if bytes.is_empty() => Ok(()),
        None => Err(invalid("diskless overlays")),
    }
}
fn boot_validate(c: &Capture, kernel: &[u8], initrd: &[u8]) -> Result<()> {
    let ram = boot::RamLayout::native(c.memory_bytes).map_err(|_| invalid("RAM layout"))?;
    boot::layout_for(&ram, kernel, initrd).map_err(|_| invalid("kernel/initrd layout"))?;
    Ok(())
}

/// Borrowed trusted-capture view. Only small private JSON metadata is parsed;
/// kernel, initrd, mapped RAM and overlays remain borrowed from the one capture.
struct Parts<'a> {
    capture: Capture,
    aux: &'a retained::Aux,
    native: Option<worker::snapshot::Header>,
    kernel: &'a [u8],
    initrd: &'a [u8],
    ram: &'a [u8],
    native_overlays: &'a [u8],
    client: &'a [u8],
    disk: Option<&'a DiskImage>,
}
impl<'a> Parts<'a> {
    fn new(snapshot: &'a Snapshot) -> Result<Self> {
        let mut input = &snapshot.bytes[..];
        let capture: Capture = protocol::receive(&mut input)
            .map_err(|_| invalid("private capture header"))?
            .ok_or(invalid("capture absent"))?;
        validate(&capture, snapshot.disk.as_ref())?;
        snapshot
            .aux
            .validate(&capture)
            .map_err(|_| invalid("retained policy/evidence"))?;
        capture.guest.validate_snapshot(capture.reached, true)?;
        let kernel = read_blob(&mut input).map_err(|_| invalid("private kernel body"))?;
        let initrd = read_blob(&mut input).map_err(|_| invalid("private initrd body"))?;
        let mut image = read_blob(&mut input).map_err(|_| invalid("private native body"))?;
        let client = if capture.disk.is_some() {
            read_blob(&mut input).map_err(|_| invalid("private client overlays"))?
        } else {
            &[]
        };
        if !input.is_empty() {
            return Err(invalid("private trailing bytes"));
        }
        boot_validate(&capture, kernel, initrd)?;
        let (native, ram, native_overlays) = if capture.live {
            if NATIVE == 0 {
                return Err(invalid("unsupported native snapshot format"));
            }
            let header = worker::snapshot::Header::from_private(&mut image)?;
            header.validate(
                capture.memory_bytes,
                capture.disk.is_some(),
                capture.network,
                capture.reached,
            )?;
            header.validate_witness(
                &super::execution::setup(
                    &snapshot.aux,
                    snapshot.aux.active.expect("validated image owner"),
                )
                .map_err(|_| invalid("native witness origin"))?,
            )?;
            header.validate_policy(
                snapshot
                    .aux
                    .policy
                    .as_ref()
                    .map(|p| p.setup(snapshot.aux.active.expect("validated image owner")))
                    .as_ref(),
            )?;
            let count = host(sum(&[PAGE, capture.memory_bytes])?)?;
            if image.len() < count {
                return Err(invalid("native RAM length"));
            }
            let (ram, overlays) = image.split_at(count);
            validate_overlay(overlays, capture.disk)?;
            if !client.is_empty() {
                return Err(invalid("live client overlay ownership"));
            }
            (Some(header), ram, overlays)
        } else {
            if !image.is_empty() {
                return Err(invalid("nonlive native body"));
            }
            validate_overlay(client, capture.disk)?;
            (None, &[][..], &[][..])
        };
        Ok(Self {
            capture,
            aux: &snapshot.aux,
            native,
            kernel,
            initrd,
            ram,
            native_overlays,
            client,
            disk: snapshot.disk.as_ref(),
        })
    }
    fn write<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        self.capture.encode(w)?;
        self.aux.encode(w)?;
        if let Some(header) = &self.native {
            header.write(w)?;
        }
        match self.disk {
            None => w.u8(0)?,
            Some(disk) => {
                w.u8(if disk.image().is_some() { 2 } else { 1 })?;
                w.u64(disk.capacity_bytes())?;
            }
        }
        let base = self
            .disk
            .and_then(DiskImage::image)
            .map_or(&[][..], |v| &v[..]);
        for b in [
            self.kernel,
            self.initrd,
            self.ram,
            self.native_overlays,
            self.client,
            base,
        ] {
            w.u64(b.len() as u64)?;
        }
        for b in [
            self.kernel,
            self.initrd,
            self.ram,
            self.native_overlays,
            self.client,
            base,
        ] {
            w.raw(b)?;
        }
        Ok(())
    }
}
impl Snapshot {
    /// Stream a complete, versioned, source-independent PV capture, including
    /// exact boot inputs, native state, immutable disk base and both disk layers.
    /// Borrows this capture without execution, RNG draws or publication. Raw
    /// bodies are written in bounded chunks without staging another RAM image.
    /// Ordinary I/O failure leaves caller-owned partial output; retrying from
    /// this unchanged capture is supported. No file/CAS transaction is implied.
    // TODO: Re-evaluate: concrete snapshots own backend closure; a generic
    // machine serializer would expose private native and bridge representation.
    pub fn write_to(&self, output: &mut impl Write) -> Result<()> {
        let parts = Parts::new(self)?;
        let mut sink = std::io::sink();
        let mut count = Writer::new(&mut sink);
        parts.write(&mut count)?;
        let mut w = Writer::new(output);
        w.raw(MAGIC)?;
        w.u32(VERSION)?;
        w.u8(ARCH)?;
        w.u8(if parts.capture.live { NATIVE } else { 0 })?;
        w.u16(0)?;
        w.u64(count.count())?;
        parts.write(&mut w)
    }
    /// Decode one complete PV snapshot under explicit wire, logical allocation
    /// and item quotas, requiring exact EOF. Pure: no filesystem, worker, mmap,
    /// device, RNG or executable lookup. Only this host's architecture and
    /// restorable native format are accepted, before body allocation. Nonlive
    /// captures remain supported on Linux ARM64. Native shapes are checked, not
    /// native register semantics, host inventories or rings versus guest RAM;
    /// later native restore remains fallible. Integrity is not authenticity.
    /// Arbitrary readers/allocators retain their normal blocking/failure behavior.
    // TODO: Re-evaluate: untrusted collection parsing must be closed and budgeted
    // at the concrete owner; private worker JSON is reconstructed only afterward.
    pub fn read_from(input: &mut impl Read, limits: &DecodeLimits) -> Result<Self> {
        if limits.encoded_bytes < HEADER_BYTES {
            return Err(CodecError::Limit("encoded bytes"));
        }
        let mut fixed = [0u8; HEADER_BYTES as usize];
        input.read_exact(&mut fixed)?;
        if &fixed[..8] != MAGIC || u32::from_le_bytes(fixed[8..12].try_into().unwrap()) != VERSION {
            return Err(invalid("format or version"));
        }
        let native = fixed[13];
        if fixed[12] != ARCH || (native != 0 && native != NATIVE) || fixed[14..16] != [0, 0] {
            return Err(invalid("architecture or native format"));
        }
        let length = u64::from_le_bytes(fixed[16..24].try_into().unwrap());
        let mut r = Reader::new(input, length, HEADER_BYTES, limits)?;
        let capture = Capture::decode(&mut r)?;
        let aux = retained::Aux::decode(&mut r)?;
        // Imported cache validation borrows page bytes; charge only its small
        // region/index/digest scratch and the expected setup's name copy.
        if let Some(cache) = &aux.cache {
            r.claim_items(cache.data.parts.len() as u64 + 7)?;
            r.charge(
                (cache.data.parts.len() * std::mem::size_of::<&str>()) as u64
                    + 7 * 16
                    + 4 * 64
                    + 22 * 8,
            )?;
        }
        if capture.live {
            if let Some(policy) = &aux.policy {
                r.charge(policy.name.len() as u64)?;
            }
        }
        aux.validate(&capture)
            .map_err(|_| invalid("retained policy/evidence"))?;
        if capture.live != (native != 0) {
            return Err(invalid("native format ownership"));
        }
        let header = if capture.live {
            Some(worker::snapshot::Header::read(&mut r)?)
        } else {
            None
        };
        let disk_tag = r.u8()?;
        let capacity = match disk_tag {
            0 => None,
            1 | 2 => Some(r.u64()?),
            _ => return Err(invalid("disk base tag")),
        };
        if capacity.is_some_and(|c| c == 0 || !c.is_multiple_of(512))
            || capture.disk != capacity.map(|c| c / 512)
        {
            return Err(invalid("disk base capacity"));
        }
        // No materialized base is needed to validate outer capacity/ownership.
        let zero = capacity
            .map(DiskImage::zeroed)
            .transpose()
            .map_err(|_| invalid("disk capacity"))?;
        // RamLayout has at most three segments (two fixed x86 holes; ARM
        // has one). Charge every validation construction's logical scratch.
        for _ in 0..(2 + u64::from(capture.live)) {
            r.claim_items(3)?;
            r.charge(3 * std::mem::size_of::<std::ops::Range<u64>>() as u64)?;
        }
        validate(&capture, zero.as_ref())?;
        if let Some(header) = &header {
            header.validate_witness(
                &super::execution::setup(&aux, aux.active.expect("validated image owner"))
                    .map_err(|_| invalid("native witness origin"))?,
            )?;
            header.validate_policy(
                aux.policy
                    .as_ref()
                    .map(|p| p.setup(aux.active.expect("validated image owner")))
                    .as_ref(),
            )?;
            header.validate(
                capture.memory_bytes,
                capture.disk.is_some(),
                capture.network,
                capture.reached,
            )?;
        }
        let mut lengths = [0usize; 6];
        for n in &mut lengths {
            *n = r.blob_length()?;
        }
        let [kernel_len, initrd_len, ram_len, native_overlay_len, client_len, base_len] = lengths;
        if sum(&lengths.map(|n| n as u64))? != r.remaining() {
            return Err(invalid("raw body framing"));
        }
        let expected_ram = if capture.live {
            sum(&[PAGE, capture.memory_bytes])?
        } else {
            0
        };
        if ram_len as u64 != expected_ram
            || (!capture.live && native_overlay_len != 0)
            || (capture.live && client_len != 0)
            || (capacity.is_none() && (native_overlay_len != 0 || client_len != 0))
            || (disk_tag == 2 && Some(base_len as u64) != capacity)
            || (disk_tag != 2 && base_len != 0)
        {
            return Err(invalid("raw body ownership or capacity"));
        }
        if let Some(sectors) = capture.disk {
            let overlays = if capture.live {
                native_overlay_len
            } else {
                client_len
            };
            if overlays < 32
                || overlays as u128 > seedbox_machine::machine::disk::codec::encoded_limit(sectors)
            {
                return Err(invalid("overlay length exceeds disk geometry"));
            }
        }
        // Exact sizing before the only retained-body allocation. No later
        // Vec growth or staging buffer can copy the full mapped RAM image.
        let outer_json = json_len(&capture)?;
        let native_json = header
            .as_ref()
            .map(|h| h.json_len())
            .transpose()?
            .unwrap_or(0);
        let image_len = if header.is_some() {
            sum(&[
                4,
                native_json as u64,
                ram_len as u64,
                native_overlay_len as u64,
            ])?
        } else {
            0
        };
        let structure = sum(&[
            4,
            outer_json as u64,
            24,
            if capacity.is_some() { 8 } else { 0 },
            if header.is_some() {
                4 + native_json as u64
            } else {
                0
            },
        ])?;
        // Preflight the base's separate Vec -> Arc<[u8]> ownership copy as
        // well, before reading any raw body or reserving mapped RAM storage.
        r.charge(sum(&[
            structure,
            std::mem::size_of::<Vec<u8>>() as u64,
            base_len as u64,
        ])?)?;
        let retained = sum(&[
            structure,
            kernel_len as u64,
            initrd_len as u64,
            ram_len as u64,
            native_overlay_len as u64,
            client_len as u64,
        ])?;
        let mut bytes = Vec::new();
        reserve(&mut bytes, host(retained)?)?;
        json_write(&capture, &mut bytes)?;
        let kernel_range = append(&mut r, &mut bytes, kernel_len, true)?;
        let initrd_range = append(&mut r, &mut bytes, initrd_len, true)?;
        // Kernel/initrd precede RAM on the wire; fail before a large RAM read.
        // Reuse the already charged layout scratch and the final-body slices.
        boot_validate(&capture, &bytes[kernel_range], &bytes[initrd_range])?;
        bytes.extend_from_slice(&image_len.to_le_bytes());
        if let Some(header) = &header {
            header.json_write(&mut bytes)?;
        }
        append(&mut r, &mut bytes, ram_len, false)?;
        let native_overlay = append(&mut r, &mut bytes, native_overlay_len, false)?;
        let client_overlay = append(&mut r, &mut bytes, client_len, capacity.is_some())?;
        if bytes.len() != host(retained)? {
            return Err(invalid("retained body size"));
        }
        if capture.live {
            validate_overlay(&bytes[native_overlay], capture.disk)?;
        } else {
            validate_overlay(&bytes[client_overlay], capture.disk)?;
        }
        let disk = if disk_tag == 2 {
            // Both this Vec and DiskImage's Arc copy were preflighted above.
            let mut base = Vec::new();
            reserve(&mut base, base_len)?;
            base.resize(base_len, 0);
            for chunk in base.chunks_mut(65536) {
                r.exact(chunk)?;
            }
            Some(DiskImage::from_bytes(base).map_err(|_| invalid("disk base"))?)
        } else {
            zero
        };
        r.finish()?;
        Ok(Snapshot {
            bytes: std::sync::Arc::new(bytes),
            aux,
            disk,
        })
    }
}
fn append<R: Read>(
    r: &mut Reader<'_, R>,
    out: &mut Vec<u8>,
    length: usize,
    prefix: bool,
) -> Result<std::ops::Range<usize>> {
    if prefix {
        out.extend_from_slice(&(length as u64).to_le_bytes());
    }
    let start = out.len();
    let end = start
        .checked_add(length)
        .ok_or(invalid("retained length overflow"))?;
    out.resize(end, 0);
    for chunk in out[start..end].chunks_mut(65536) {
        r.exact(chunk)?;
    }
    Ok(start..end)
}
impl Record for State {
    const MIN: u64 = 1;
    fn encode<W: Write>(&self, w: &mut Writer<'_, W>) -> Result<()> {
        match self {
            Self::Ready => w.u8(0),
            Self::Blocked(d) => {
                w.u8(1)?;
                d.encode(w)
            }
            Self::Exited => w.u8(2),
            Self::Crashed => w.u8(3),
            Self::Invalid => w.u8(4),
        }
    }
    fn decode<R: Read>(r: &mut Reader<'_, R>) -> Result<Self> {
        match r.u8()? {
            0 => Ok(Self::Ready),
            1 => Ok(Self::Blocked(Option::decode(r)?)),
            2 => Ok(Self::Exited),
            3 => Ok(Self::Crashed),
            4 => Ok(Self::Invalid),
            _ => Err(invalid("state tag")),
        }
    }
}
#[cfg(test)]
mod tests;
