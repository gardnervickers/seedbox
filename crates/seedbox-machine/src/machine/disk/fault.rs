//! Disk fault selection beside the shared request semantics, so every
//! transport offers its requests to the same policy (docs/faults.md).
//!
//! A transport reads a request's header ([`Request::decode`]), offers it on
//! arrival ([`DiskFaults::arrive`]) to the holds of `block_request` triggers
//! and to `disk_latency`, then executes it through [`DiskFaults::execute`],
//! where `disk_error` selects it. Faults match by op, then by scope; each
//! match draws once on that fault's stream for this boot,
//! `disk-error/<boot>` or `disk-latency/<boot>` keyed by the fault's seed,
//! which the Driver derives from the machine and fault. A fresh boot starts
//! fresh streams and zero counts; the machine adds each boot's counts.
use super::{
    ext4,
    request::{self, Buffers, Completion, Malformed},
    Store, SECTOR,
};
use crate::pv::{DeviceFault, DiskEffect, DiskOp, DiskScope, FaultCounts};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const HEADER: usize = 16;
const TYPE_IN: u32 = 0;
const TYPE_OUT: u32 = 1;
const TYPE_FLUSH: u32 = 4;

/// What a request asks of the disk, from its header, before any effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    pub kind: Kind,
    pub sector: u64,
    /// Whole data sectors the request carries.
    pub sectors: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Read,
    Write,
    Flush,
    /// Any other type, or a request without a header; never selected.
    Other,
}
impl Request {
    /// Classify a request from its first 16 readable bytes and its stream
    /// lengths. Malformed requests classify as [`Kind::Other`].
    pub fn decode(header: &[u8; HEADER], readable: usize, writable: usize) -> Self {
        let kind = u32::from_le_bytes(header[..4].try_into().expect("header kind"));
        let sector = u64::from_le_bytes(header[8..].try_into().expect("header sector"));
        let (kind, data) = match kind {
            _ if readable < HEADER || writable == 0 => (Kind::Other, 0),
            TYPE_IN => (Kind::Read, writable - 1),
            TYPE_OUT => (Kind::Write, readable - HEADER),
            TYPE_FLUSH => (Kind::Flush, 0),
            _ => (Kind::Other, 0),
        };
        Self {
            kind,
            sector,
            sectors: (data / SECTOR) as u64,
        }
    }
}
fn selects(op: DiskOp, kind: Kind) -> bool {
    matches!(
        (op, kind),
        (DiskOp::Read | DiskOp::Any, Kind::Read)
            | (DiskOp::Write | DiskOp::Any, Kind::Write)
            | (DiskOp::Flush | DiskOp::Any, Kind::Flush)
    )
}

/// A named SHA-256 counter stream, as the machine's other policy streams.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    key: [u8; 32],
    counter: u64,
}
impl Stream {
    fn new(seed: u64, name: &str, boot: u64) -> Self {
        let mut h = Sha256::new();
        h.update(b"seedbox-v1");
        h.update(seed.to_le_bytes());
        h.update(format!("{name}/{boot}").as_bytes());
        Self {
            key: h.finalize().into(),
            counter: 0,
        }
    }
    fn next(&mut self) -> u64 {
        let mut h = Sha256::new();
        h.update(self.key);
        h.update(self.counter.to_le_bytes());
        // 2^64 draws on one boot's stream cannot happen in a bounded run.
        self.counter = self.counter.checked_add(1).expect("disk fault stream");
        u64::from_le_bytes(h.finalize()[..8].try_into().expect("eight digest bytes"))
    }
    /// Uniform in `0..n` by rejection; zero for `n == 0`.
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        let zone = u64::MAX - u64::MAX % n;
        loop {
            let v = self.next();
            if v < zone {
                return v % n;
            }
        }
    }
}

/// One installed disk fault on this boot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub id: u64,
    pub seed: u64,
    pub fault: DeviceFault,
    stream: Stream,
    /// This boot's counts.
    pub counts: FaultCounts,
}

/// A `block_request` trigger's hold, counting matching requests across boots.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hold {
    pub trigger: u32,
    pub op: DiskOp,
    pub nth: u64,
    /// Matching requests already counted, below `nth`.
    pub seen: u64,
    /// Requests arriving at or after this machine time are not counted.
    pub until: Option<u64>,
}

/// How a transport handles a request on arrival.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Execute it now, unless earlier requests still wait.
    Now,
    /// Complete it no earlier than this many nanoseconds after arrival.
    Delay(u64),
    /// Hold it for this trigger: no effect, and nothing after it runs.
    Held(u32),
}

/// The disk faults and holds of one boot.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskFaults {
    boot: u64,
    installed: Vec<Installed>,
    holds: Vec<Hold>,
}
impl DiskFaults {
    /// No faults, drawing on the streams of `boot`.
    pub fn new(boot: u64) -> Self {
        Self {
            boot,
            ..Self::default()
        }
    }
    pub fn boot(&self) -> u64 {
        self.boot
    }
    pub fn installed(&self) -> &[Installed] {
        &self.installed
    }
    pub fn holds(&self) -> &[Hold] {
        &self.holds
    }
    /// Whether anything can select, delay or hold a request.
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty() && self.holds.is_empty()
    }
    /// Install a disk fault with zero counts on this boot's stream. Faults
    /// other than `disk_error` and `disk_latency` are not disk faults.
    pub fn install(&mut self, id: u64, seed: u64, fault: DeviceFault) -> Result<(), String> {
        let name = match fault {
            DeviceFault::DiskError { .. } => "disk-error",
            DeviceFault::DiskLatency { .. } => "disk-latency",
            DeviceFault::Preempt { .. } => return Err("preempt is not a disk fault".into()),
        };
        if self.installed.iter().any(|f| f.id >= id) {
            return Err("disk fault identity reused or out of order".into());
        }
        self.installed.push(Installed {
            id,
            seed,
            fault,
            stream: Stream::new(seed, name, self.boot),
            counts: FaultCounts::default(),
        });
        Ok(())
    }
    /// Remove a fault, returning this boot's counts.
    pub fn heal(&mut self, id: u64) -> Option<FaultCounts> {
        let index = self.installed.iter().position(|f| f.id == id)?;
        Some(self.installed.remove(index).counts)
    }
    pub fn hold(&mut self, hold: Hold) -> Result<(), String> {
        if hold.nth == 0 || hold.seen >= hold.nth {
            return Err("block request hold needs a positive remaining count".into());
        }
        if self.holds.iter().any(|h| h.trigger == hold.trigger) {
            return Err("block request hold reused its trigger".into());
        }
        self.holds.push(hold);
        Ok(())
    }
    /// Stop counting for `trigger`, returning its hold if it was installed.
    pub fn release(&mut self, trigger: u32) -> Option<Hold> {
        let index = self.holds.iter().position(|h| h.trigger == trigger)?;
        Some(self.holds.remove(index))
    }
    /// Offer a request on arrival at machine time `now`: holds count it in
    /// installation order and the one reaching its `nth` holds it, then every
    /// matching latency fault delays it by its latency plus a jitter draw.
    pub fn arrive(&mut self, request: Request, now: u64) -> Arrival {
        if request.kind == Kind::Other {
            return Arrival::Now;
        }
        for index in 0..self.holds.len() {
            let hold = &mut self.holds[index];
            if !selects(hold.op, request.kind) || hold.until.is_some_and(|until| now >= until) {
                continue;
            }
            hold.seen += 1;
            if hold.seen == hold.nth {
                let trigger = hold.trigger;
                self.holds.remove(index);
                return Arrival::Held(trigger);
            }
        }
        let mut delay = None;
        for fault in &mut self.installed {
            let DeviceFault::DiskLatency {
                op,
                latency_ns,
                jitter_ns,
            } = fault.fault
            else {
                continue;
            };
            if !selects(op, request.kind) {
                continue;
            }
            let jitter = fault.stream.below(jitter_ns.saturating_add(1));
            fault.counts.matched += 1;
            fault.counts.affected += 1;
            let this = latency_ns.saturating_add(jitter);
            delay = Some(delay.map_or(this, |d: u64| d.max(this)));
        }
        delay.map_or(Arrival::Now, Arrival::Delay)
    }
    /// Execute a request through [`request::execute`] after `disk_error`
    /// selection: each matching fault draws once, and the first hit acts.
    /// `eio` completes with IOERR and no store or data effect; `corrupt`
    /// replaces one seeded sector of the data a read returns or a write
    /// stores. A selected request counts as affected only if its effect
    /// happened: a corrupted request must otherwise complete normally.
    pub fn execute(
        &mut self,
        store: &mut Store,
        buffers: &mut impl Buffers,
        request: Request,
    ) -> Result<Completion, Malformed> {
        if request.kind == Kind::Other || self.installed.is_empty() {
            return request::execute(store, buffers);
        }
        let mut layout = None;
        let mut chosen: Option<(usize, DiskEffect)> = None;
        for index in 0..self.installed.len() {
            let DeviceFault::DiskError {
                op,
                ppm,
                effect,
                scope,
            } = self.installed[index].fault
            else {
                continue;
            };
            if !selects(op, request.kind) {
                continue;
            }
            if scope == DiskScope::Data {
                let layout = layout.get_or_insert_with(|| ext4::layout(store));
                // Unformatted disks have no data yet; flushes have no sectors.
                match layout {
                    Some(layout)
                        if request.kind != Kind::Flush
                            && !layout.overlaps(request.sector, request.sectors) => {}
                    _ => continue,
                }
            }
            let fault = &mut self.installed[index];
            fault.counts.matched += 1;
            if fault.stream.below(1_000_000) < u64::from(ppm) && chosen.is_none() {
                chosen = Some((index, effect));
            }
        }
        let Some((index, effect)) = chosen else {
            return request::execute(store, buffers);
        };
        match effect {
            DiskEffect::Eio => {
                let completion = request::fail(buffers)?;
                self.installed[index].counts.affected += 1;
                Ok(completion)
            }
            DiskEffect::Corrupt => {
                let fault = &mut self.installed[index];
                // Draw the bytes even for a request without data, so the
                // stream's position depends on selections, not on sizes.
                let target = fault.stream.below(request.sectors.max(1));
                let mut bytes = [0u8; SECTOR];
                for chunk in bytes.chunks_mut(8) {
                    chunk.copy_from_slice(&fault.stream.next().to_le_bytes());
                }
                let mut corrupting = Corrupting {
                    inner: buffers,
                    kind: request.kind,
                    sector: target as usize,
                    bytes,
                };
                let completion = request::execute(store, &mut corrupting)?;
                if completion.status == request::STATUS_OK && request.sectors > 0 {
                    fault.counts.affected += 1;
                }
                Ok(completion)
            }
        }
    }
}

/// Buffers that substitute one sector of seeded bytes: in the writable
/// stream for a read, in the readable stream for a write.
struct Corrupting<'b, B> {
    inner: &'b mut B,
    kind: Kind,
    sector: usize,
    bytes: [u8; SECTOR],
}
impl<B: Buffers> Corrupting<'_, B> {
    /// The overlap of `[offset, offset + length)` with the target sector at
    /// stream offset `base`, as (range in the access, offset in the sector).
    fn overlap(&self, base: usize, offset: usize, length: usize) -> Option<(usize, usize, usize)> {
        let start = base + self.sector * SECTOR;
        let (from, to) = (offset.max(start), (offset + length).min(start + SECTOR));
        (from < to).then(|| (from - offset, to - offset, from - start))
    }
}
impl<B: Buffers> Buffers for Corrupting<'_, B> {
    fn readable(&self) -> usize {
        self.inner.readable()
    }
    fn writable(&self) -> usize {
        self.inner.writable()
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        self.inner.read(offset, out);
        if self.kind == Kind::Write {
            if let Some((a, b, at)) = self.overlap(HEADER, offset, out.len()) {
                out[a..b].copy_from_slice(&self.bytes[at..at + (b - a)]);
            }
        }
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        // The status byte is past the data, so it never overlaps the sector.
        match self.overlap(0, offset, bytes.len()) {
            Some((a, b, at)) if self.kind == Kind::Read => {
                let mut copy = bytes.to_vec();
                copy[a..b].copy_from_slice(&self.bytes[at..at + (b - a)]);
                self.inner.write(offset, &copy);
            }
            _ => self.inner.write(offset, bytes),
        }
    }
}

// Closed persisted records for the worker snapshot.
impl crate::snapshot::Record for DiskOp {
    const MIN: u64 = 1;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        w.u8(match self {
            Self::Read => 0,
            Self::Write => 1,
            Self::Flush => 2,
            Self::Any => 3,
        })
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(match r.u8()? {
            0 => Self::Read,
            1 => Self::Write,
            2 => Self::Flush,
            3 => Self::Any,
            _ => return Err(crate::snapshot::CodecError::Invalid("disk op tag")),
        })
    }
}
impl crate::snapshot::Record for DeviceFault {
    const MIN: u64 = 1;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        match *self {
            Self::DiskError {
                op,
                ppm,
                effect,
                scope,
            } => {
                w.u8(0)?;
                op.encode(w)?;
                w.u32(ppm)?;
                w.u8(u8::from(effect == DiskEffect::Corrupt))?;
                w.u8(u8::from(scope == DiskScope::Data))
            }
            Self::DiskLatency {
                op,
                latency_ns,
                jitter_ns,
            } => {
                w.u8(1)?;
                op.encode(w)?;
                w.u64(latency_ns)?;
                w.u64(jitter_ns)
            }
            Self::Preempt { ppm } => {
                w.u8(2)?;
                w.u32(ppm)
            }
        }
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        use crate::snapshot::CodecError;
        let flag = |r: &mut crate::snapshot::Reader<'_, R>| match r.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(CodecError::Invalid("disk fault flag")),
        };
        Ok(match r.u8()? {
            0 => Self::DiskError {
                op: DiskOp::decode(r)?,
                ppm: r.u32()?,
                effect: if flag(r)? {
                    DiskEffect::Corrupt
                } else {
                    DiskEffect::Eio
                },
                scope: if flag(r)? {
                    DiskScope::Data
                } else {
                    DiskScope::All
                },
            },
            1 => Self::DiskLatency {
                op: DiskOp::decode(r)?,
                latency_ns: r.u64()?,
                jitter_ns: r.u64()?,
            },
            2 => Self::Preempt { ppm: r.u32()? },
            _ => return Err(CodecError::Invalid("device fault tag")),
        })
    }
}
impl crate::snapshot::Record for FaultCounts {
    const MIN: u64 = 16;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut crate::snapshot::Writer<'_, W>,
    ) -> crate::snapshot::Result<()> {
        w.u64(self.matched)?;
        w.u64(self.affected)
    }
    fn decode<R: std::io::Read>(
        r: &mut crate::snapshot::Reader<'_, R>,
    ) -> crate::snapshot::Result<Self> {
        Ok(Self {
            matched: r.u64()?,
            affected: r.u64()?,
        })
    }
}
crate::record!(Stream {
    key: [u8; 32],
    counter: u64
});
crate::record!(Installed {
    id: u64,
    seed: u64,
    fault: DeviceFault,
    stream: Stream,
    counts: FaultCounts
});
crate::record!(Hold {
    trigger: u32,
    op: DiskOp,
    nth: u64,
    seen: u64,
    until: Option<u64>
});
crate::record!(DiskFaults {
    boot: u64,
    installed: Vec<Installed>,
    holds: Vec<Hold>
});
impl DiskFaults {
    /// Restored state is what this boot's policy can produce: increasing
    /// identities, disk faults only, streams of this boot and live holds.
    pub fn validate_snapshot(&self) -> Result<(), String> {
        let mut previous = None;
        for fault in &self.installed {
            let name = match fault.fault {
                DeviceFault::DiskError { .. } => "disk-error",
                DeviceFault::DiskLatency { .. } => "disk-latency",
                DeviceFault::Preempt { .. } => return Err("PV snapshot disk fault kind".into()),
            };
            if previous.is_some_and(|p| p >= fault.id)
                || fault.stream.key != Stream::new(fault.seed, name, self.boot).key
                || fault.counts.affected > fault.counts.matched
            {
                return Err("PV snapshot disk fault state".into());
            }
            previous = Some(fault.id);
        }
        let mut triggers = std::collections::BTreeSet::new();
        for hold in &self.holds {
            if hold.nth == 0 || hold.seen >= hold.nth || !triggers.insert(hold.trigger) {
                return Err("PV snapshot block request hold".into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
