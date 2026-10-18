//! Shared block requests and disk effects over bounded split queues.
use crate::disk::{Disk, SECTOR};
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy};

type Result<T> = std::result::Result<T, String>;
const ID: &[u8; 20] = b"seedbox-disk\0\0\0\0\0\0\0\0";
/// Request kinds (VIRTIO_BLK_T_*).
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
/// Completion status bytes (VIRTIO_BLK_S_*).
const S_OK: u8 = 0;
const S_IOERR: u8 = 1;
const S_UNSUPP: u8 = 2;

/// Adapter accounting identity; request and completion rules are shared.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Application,
    Block,
}

#[derive(Clone, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub sectors_read: u64,
    pub sectors_written: u64,
    pub sectors_flushed: u64,
    pub errors: u64,
}

#[derive(Default, Debug, PartialEq, Eq)]
pub struct Notified {
    pub completed: u64,
    pub flushed: u64,
    pub irq: bool,
    pub log: Vec<(u32, u64, u8)>,
    /// The trigger whose hold stopped processing at a selected request.
    pub held: Option<u32>,
}

/// A `block_request` trigger's rule, installed in the device: on admission
/// of the `nth` matching request it stops processing before that request
/// has any effect, and the machine suspends until the coordinator crashes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hold {
    pub trigger_id: u32,
    pub op: seedbox_control::DiskOp,
    pub nth: u64,
    /// Matching requests admitted so far.
    #[serde(default)]
    pub seen: u64,
}
impl Hold {
    fn matches(&self, kind: u32) -> bool {
        disk_op_matches(self.op, kind)
    }
}
fn disk_op_matches(op: seedbox_control::DiskOp, kind: u32) -> bool {
    use seedbox_control::DiskOp;
    matches!(
        (op, kind),
        (DiskOp::Any, T_IN | T_OUT | T_FLUSH)
            | (DiskOp::Read, T_IN)
            | (DiskOp::Write, T_OUT)
            | (DiskOp::Flush, T_FLUSH)
    )
}

#[derive(Clone)]
pub struct Block {
    disk: Disk,
    policy: Policy,
    // Application counters stay disabled and are not part of retained state.
    stats: Stats,
    errors: Option<InjectedErrors>,
    latency: Option<Box<InjectedLatency>>,
    /// Admitted requests not yet completed, in submission order.
    pending: std::collections::VecDeque<Pending>,
    hold: Option<Hold>,
    /// A request was held: the device processes nothing more.
    held: Option<u32>,
}

/// A request admitted from the queue and due to complete at `due_ns`. It is
/// decoded again on completion; the driver may not touch its buffers before.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    chain: crate::virtqueue::Chain,
    due_ns: u64,
    /// The queue layout it was admitted under: size and ring addresses.
    queue: (u16, u64, u64, u64),
}

fn layout(queue: &crate::virtqueue::Queue) -> (u16, u64, u64, u64) {
    (queue.num, queue.desc, queue.avail, queue.used)
}

/// An active `disk_latency` fault: which requests it delays, by how much,
/// its own seeded jitter stream, and what it has done so far.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedLatency {
    op: seedbox_control::DiskOp,
    latency_ns: u64,
    jitter_ns: u64,
    rng: crate::rng::Rng,
    counts: LatencyCounts,
}

/// Requests a `disk_latency` fault delayed, and the most it held at once.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyCounts {
    pub delayed: u64,
    pub deepest: u64,
}

impl std::fmt::Debug for InjectedLatency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectedLatency")
            .field("op", &self.op)
            .field("latency_ns", &self.latency_ns)
            .field("jitter_ns", &self.jitter_ns)
            .field("draws", &self.rng.counter())
            .field("counts", &self.counts)
            .finish()
    }
}

impl InjectedLatency {
    pub fn new(
        op: seedbox_control::DiskOp,
        latency_ns: u64,
        jitter_ns: u64,
        rng: crate::rng::Rng,
    ) -> Result<Self> {
        seedbox_control::validate_disk_latency(latency_ns, jitter_ns)?;
        Ok(Self {
            op,
            latency_ns,
            jitter_ns,
            rng,
            counts: LatencyCounts::default(),
        })
    }
    /// One request's delay; only matching requests draw jitter.
    fn delay(&mut self, kind: u32) -> u64 {
        if !disk_op_matches(self.op, kind) {
            return 0;
        }
        self.counts.delayed += 1;
        let jitter = if self.jitter_ns > 0 {
            self.rng.below(self.jitter_ns + 1)
        } else {
            0
        };
        self.latency_ns + jitter
    }
}

/// An active `disk_error` fault: which requests it fails, how often, its own
/// seeded draw stream, and what it has done so far.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InjectedErrors {
    op: seedbox_control::DiskOp,
    ppm: u32,
    rng: crate::rng::Rng,
    counts: ErrorCounts,
    #[serde(default, skip_serializing_if = "seedbox_control::DiskEffect::is_error")]
    effect: seedbox_control::DiskEffect,
    #[serde(default, skip_serializing_if = "seedbox_control::DiskScope::is_disk")]
    scope: seedbox_control::DiskScope,
    /// A data scope's static filesystem metadata, read once it exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<crate::disk::ext4::Layout>,
}

/// Requests a `disk_error` fault matched, and how many of them it failed
/// (or, with the `corrupt` effect, corrupted).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorCounts {
    pub matched: u64,
    pub failed: u64,
}

impl std::fmt::Debug for InjectedErrors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectedErrors")
            .field("op", &self.op)
            .field("ppm", &self.ppm)
            .field("draws", &self.rng.counter())
            .field("counts", &self.counts)
            .field("effect", &self.effect)
            .field("scope", &self.scope)
            .finish()
    }
}

impl InjectedErrors {
    pub fn new(op: seedbox_control::DiskOp, ppm: u32, rng: crate::rng::Rng) -> Result<Self> {
        if !seedbox_control::DISK_ERROR_PPM.contains(&ppm) {
            return Err("invalid disk error probability".into());
        }
        Ok(Self {
            op,
            ppm,
            rng,
            counts: ErrorCounts::default(),
            effect: seedbox_control::DiskEffect::Error,
            scope: seedbox_control::DiskScope::Disk,
            metadata: None,
        })
    }
    pub fn with_effect(mut self, effect: seedbox_control::DiskEffect) -> Self {
        self.effect = effect;
        self
    }
    pub fn with_scope(mut self, scope: seedbox_control::DiskScope) -> Self {
        self.scope = scope;
        self
    }
    /// Whether a request may be selected at all: under a data scope only a
    /// read or write clear of the filesystem's static metadata whose blocks
    /// (the data written, or the disk's content for a read) do not look like
    /// metadata, and nothing before the filesystem exists. Ineligible
    /// requests draw nothing.
    fn eligible(&mut self, disk: &crate::disk::Disk, request: &Request) -> bool {
        if self.scope == seedbox_control::DiskScope::Disk {
            return true;
        }
        let sectors = match request.kind {
            T_IN => (request.room / SECTOR) as u64,
            T_OUT => (request.input.len() / SECTOR) as u64,
            _ => return false,
        };
        if self.metadata.is_none() {
            self.metadata =
                crate::disk::ext4::layout(&mut |at, buf: &mut [u8]| read_bytes(disk, at, buf));
        }
        let Some(layout) = &self.metadata else {
            return false;
        };
        if sectors == 0 || layout.overlaps(request.sector, sectors) {
            return false;
        }
        let read;
        let bytes = if request.kind == T_OUT {
            &request.input[..]
        } else {
            let mut buf = vec![0; sectors as usize * SECTOR];
            if disk.read(request.sector, &mut buf).is_err() {
                return false;
            }
            read = buf;
            &read[..]
        };
        // Whole filesystem blocks inside the request, by absolute position.
        let per = layout.block_sectors.max(1);
        let first = request.sector.div_ceil(per) * per;
        let mut at = first;
        while at + per <= request.sector + sectors {
            let offset = ((at - request.sector) as usize) * SECTOR;
            if crate::disk::ext4::is_metadata_block(&bytes[offset..offset + per as usize * SECTOR])
            {
                return false;
            }
            at += per;
        }
        true
    }
    /// Draw for one request; only matching requests consume the stream.
    /// Corruption never selects a flush.
    fn fails(&mut self, kind: u32) -> bool {
        if !disk_op_matches(self.op, kind)
            || (self.effect == seedbox_control::DiskEffect::Corrupt && kind == T_FLUSH)
        {
            return false;
        }
        self.counts.matched += 1;
        let fail = self.ppm == *seedbox_control::DISK_ERROR_PPM.end()
            || self.rng.below(1_000_000) < u64::from(self.ppm);
        self.counts.failed += u64::from(fail);
        fail
    }
    /// Replace one seeded sector of `data` with seeded bytes.
    fn corrupt(&mut self, data: &mut [u8]) {
        let sectors = (data.len() / SECTOR) as u64;
        if sectors == 0 {
            return;
        }
        let at = self.rng.below(sectors) as usize * SECTOR;
        for chunk in data[at..at + SECTOR].chunks_mut(8) {
            chunk.copy_from_slice(&self.rng.next().to_le_bytes()[..chunk.len()]);
        }
    }
}

/// Fill `buf` from byte offset `at` of the disk through whole sectors.
fn read_bytes(disk: &crate::disk::Disk, at: u64, buf: &mut [u8]) -> bool {
    let first = at / SECTOR as u64;
    let skip = (at % SECTOR as u64) as usize;
    let mut sectors = vec![0; (skip + buf.len()).div_ceil(SECTOR) * SECTOR];
    if disk.read(first, &mut sectors).is_err() {
        return false;
    }
    buf.copy_from_slice(&sectors[skip..skip + buf.len()]);
    true
}

/// Device metadata paired with the separate streaming disk checkpoint.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    policy: Policy,
    stats: Stats,
    disk_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    errors: Option<InjectedErrors>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    hold: Option<Hold>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latency: Option<InjectedLatency>,
    #[serde(default, skip_serializing_if = "std::collections::VecDeque::is_empty")]
    pending: std::collections::VecDeque<Pending>,
}

struct Request {
    kind: u32,
    sector: u64,
    input: Vec<u8>,
    output: Vec<(usize, usize)>,
    room: usize,
    status: usize,
}

impl Request {
    fn decode(memory: &[u8], chain: &Chain) -> Result<Self> {
        let segments = &chain.segments;
        let (kind, sector, input, output, room, status) = {
            let readable = segments
                .iter()
                .filter(|s| !s.writable)
                .map(|s| s.len)
                .sum::<usize>();
            let writable = segments
                .iter()
                .filter(|s| s.writable)
                .map(|s| s.len)
                .sum::<usize>();
            if readable < 16 || writable < 1 {
                return Err("disk request without a header or status byte".into());
            }
            // Copy all readable bytes before any disk or RAM write, including aliases.
            let mut bytes = Vec::with_capacity(readable);
            for span in segments.iter().filter(|s| !s.writable) {
                bytes.extend_from_slice(&memory[span.offset..span.offset + span.len]);
            }
            let kind = u32::from_le_bytes(bytes[..4].try_into().expect("checked header"));
            let sector = u64::from_le_bytes(bytes[8..16].try_into().expect("checked header"));
            let room = writable - 1;
            let mut left = room;
            let mut output = Vec::new();
            let mut status = None;
            for span in segments.iter().filter(|s| s.writable) {
                let n = span.len.min(left);
                output.push((span.offset, n));
                left -= n;
                if left == 0 && n < span.len {
                    status = Some(span.offset + n);
                }
            }
            (
                kind,
                sector,
                bytes[16..].to_vec(),
                output,
                room,
                status.expect("nonempty writable capacity"),
            )
        };
        Ok(Self {
            kind,
            sector,
            input,
            output,
            room,
            status,
        })
    }
}

impl Block {
    /// A device holding a request has no reusable state.
    pub fn snapshot(&self) -> Result<Snapshot> {
        if self.held.is_some() {
            return Err("a block device holding a request cannot be checkpointed".into());
        }
        Ok(Snapshot {
            policy: self.policy,
            stats: self.stats.clone(),
            disk_digest: self.disk.digest(),
            errors: self.errors.clone(),
            hold: self.hold,
            latency: self.latency.as_deref().cloned(),
            pending: self.pending.clone(),
        })
    }

    /// Construct with the restored disk first. Reject mismatched sidecars before
    /// publishing the device, including application counters that cannot occur.
    pub fn restore_snapshot(mut self, snapshot: Snapshot) -> Result<Self> {
        if self.policy != snapshot.policy
            || self.disk.digest() != snapshot.disk_digest
            || (self.policy == Policy::Application && snapshot.stats != Stats::default())
        {
            return Err("block snapshot policy, disk or counters mismatch".into());
        }
        self.stats = snapshot.stats;
        self.errors = snapshot.errors;
        self.hold = snapshot.hold;
        self.latency = snapshot.latency.map(Box::new);
        self.pending = snapshot.pending;
        Ok(self)
    }

    pub fn new(disk: Disk, policy: Policy) -> Self {
        Self {
            disk,
            policy,
            stats: Stats::default(),
            errors: None,
            latency: None,
            pending: std::collections::VecDeque::new(),
            hold: None,
            held: None,
        }
    }
    /// Install or remove a hold. Like injected errors it is a world rule and
    /// outlives a guest reset; it starts counting at the next request.
    pub fn set_hold(&mut self, hold: Option<Hold>) -> Result<()> {
        if self.held.is_some() {
            return Err("block device already holds a request".into());
        }
        if hold.is_some_and(|h| h.nth == 0 || h.seen >= h.nth) {
            return Err("invalid block request hold".into());
        }
        self.hold = hold;
        Ok(())
    }
    pub fn take_hold(&mut self) -> Option<Hold> {
        self.hold.take()
    }
    pub fn held(&self) -> Option<u32> {
        self.held
    }
    /// Remove the injected errors with their stream and counts, so a device
    /// rebuilt on reset can keep them.
    pub fn take_errors(&mut self) -> Option<InjectedErrors> {
        self.errors.take()
    }
    /// Start or end injected disk errors; ending returns what they did.
    /// Takes effect from the next request the device processes.
    pub fn set_errors(&mut self, errors: Option<InjectedErrors>) -> Option<ErrorCounts> {
        std::mem::replace(&mut self.errors, errors).map(|old| old.counts)
    }
    /// Remove the injected latency with its stream and counts, so a device
    /// rebuilt on reset can keep it. Pending requests go with the old device.
    pub fn take_latency(&mut self) -> Option<InjectedLatency> {
        self.latency.take().map(|latency| *latency)
    }
    /// Start or end injected latency; ending returns what it did. Requests
    /// already pending keep their due times, and later ones still queue
    /// behind them.
    pub fn set_latency(&mut self, latency: Option<InjectedLatency>) -> Option<LatencyCounts> {
        std::mem::replace(&mut self.latency, latency.map(Box::new)).map(|old| old.counts)
    }
    /// The driver stopped the queue: the device gives up its pending
    /// requests, unanswered. Adapters call this after every transport write.
    pub fn queue_changed(&mut self, transport: &Transport) {
        if !transport.queues[0].ready {
            self.pending.clear();
        }
    }
    /// When the earliest pending request completes.
    pub fn next_due(&self) -> Option<u64> {
        self.pending.front().map(|p| p.due_ns)
    }
    /// Pending requests and their due times, for state projections.
    pub fn pending_due(&self) -> Vec<u64> {
        self.pending.iter().map(|p| p.due_ns).collect()
    }
    pub fn into_disk(self) -> Disk {
        self.disk
    }
    pub fn disk(&self) -> &Disk {
        &self.disk
    }
    pub fn disk_mut(&mut self) -> &mut Disk {
        &mut self.disk
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The adapter supplies the one-queue transport and handles register resets.
    /// A reset retains this model: disk layers and KVM statistics survive it.
    /// Without injected latency or pending requests, every request completes
    /// here; otherwise each queues behind earlier ones until its due time,
    /// and those due by `now_ns` complete before this returns.
    pub fn notify(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        now_ns: u64,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Notified> {
        let mut out = Notified::default();
        let policy = QueuePolicy {
            limit: 4 << 20,
            direction: Direction::ReadThenWrite,
            reject_empty: false,
            diagnostics: Diagnostics::Disk,
        };
        if self.held.is_some() {
            return Err("block device notified while holding a request".into());
        }
        while let Some(chain) = transport.queues[0].validated_chain(memory, base, policy)? {
            let request = Request::decode(memory, &chain)?;
            // A held request gets no disk effect, used-ring entry or IRQ, and
            // nothing after it is processed. Earlier requests stay complete.
            if let Some(hold) = self.hold.as_mut().filter(|h| h.matches(request.kind)) {
                hold.seen += 1;
                if hold.seen == hold.nth {
                    self.held = Some(hold.trigger_id);
                    out.held = self.held;
                    self.hold = None;
                    break;
                }
            }
            if self.latency.is_none() && self.pending.is_empty() {
                self.finish(transport, memory, &chain, request, &mut dirty, &mut out);
                transport.queues[0].consume();
                continue;
            }
            // A driver cannot have more requests outstanding than queue
            // entries, nor reuse a chain the device still owns.
            if self.pending.len() >= usize::from(transport.queues[0].num) {
                return Err("more pending block requests than queue entries".into());
            }
            if self.pending.iter().any(|p| p.chain.head == chain.head) {
                return Err("block descriptor chain reused while pending".into());
            }
            transport.queues[0].consume();
            let delay = self.latency.as_mut().map_or(0, |l| l.delay(request.kind));
            let after = self.pending.back().map_or(0, |p| p.due_ns);
            let due_ns = now_ns.saturating_add(delay).max(after);
            let queue = layout(&transport.queues[0]);
            self.pending.push_back(Pending {
                chain,
                due_ns,
                queue,
            });
            if let Some(latency) = &mut self.latency {
                latency.counts.deepest = latency.counts.deepest.max(self.pending.len() as u64);
            }
        }
        if out.held.is_none() {
            self.complete_into(transport, memory, now_ns, &mut dirty, &mut out)?;
        }
        if out.irq {
            transport.isr |= 1;
        }
        Ok(out)
    }

    /// Complete every pending request due by `now_ns`, in submission order.
    pub fn complete_due(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        now_ns: u64,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Notified> {
        let mut out = Notified::default();
        if self.held.is_none() {
            self.complete_into(transport, memory, now_ns, &mut dirty, &mut out)?;
        }
        Ok(out)
    }

    fn complete_into(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        now_ns: u64,
        dirty: &mut impl FnMut(usize, usize),
        out: &mut Notified,
    ) -> Result<()> {
        while self.pending.front().is_some_and(|p| p.due_ns <= now_ns) {
            let pending = self.pending.pop_front().expect("checked front");
            // A queue the driver stopped (or reconfigured) no longer belongs
            // to its pending requests: they are dropped unanswered.
            let queue = &transport.queues[0];
            if !queue.ready || layout(queue) != pending.queue {
                continue;
            }
            let request = Request::decode(memory, &pending.chain)?;
            self.finish(transport, memory, &pending.chain, request, dirty, out);
        }
        Ok(())
    }

    /// One request's disk effect, data, status, used entry and interrupt.
    fn finish(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        chain: &crate::virtqueue::Chain,
        request: Request,
        dirty: &mut impl FnMut(usize, usize),
        out: &mut Notified,
    ) {
        let mut data = Vec::new();
        // An injected error fails a valid request before any disk effect;
        // injected corruption garbles the data the request moves instead.
        let disk = &self.disk;
        let selected = self
            .errors
            .as_mut()
            .is_some_and(|errors| errors.eligible(disk, &request) && errors.fails(request.kind));
        let corrupt = selected
            && self
                .errors
                .as_ref()
                .is_some_and(|e| e.effect == seedbox_control::DiskEffect::Corrupt);
        let injected = selected && !corrupt;
        let mut request = request;
        if corrupt && request.kind == T_OUT {
            if let Some(errors) = self.errors.as_mut() {
                errors.corrupt(&mut request.input);
            }
        }
        let status = match request.kind {
            T_IN | T_OUT | T_FLUSH if injected => S_IOERR,
            T_IN => {
                data = vec![0; request.room];
                if self.disk.read(request.sector, &mut data).is_ok() {
                    if corrupt {
                        if let Some(errors) = self.errors.as_mut() {
                            errors.corrupt(&mut data);
                        }
                    }
                    if self.policy == Policy::Block {
                        self.stats.reads += 1;
                        self.stats.sectors_read += (request.room / SECTOR) as u64;
                    }
                    S_OK
                } else {
                    data.clear();
                    S_IOERR
                }
            }
            T_OUT => {
                if self.disk.write(request.sector, &request.input).is_ok() {
                    if self.policy == Policy::Block {
                        self.stats.writes += 1;
                        self.stats.sectors_written += (request.input.len() / SECTOR) as u64;
                    }
                    S_OK
                } else {
                    S_IOERR
                }
            }
            T_FLUSH => {
                let n = self.disk.flush();
                out.flushed += n;
                if self.policy == Policy::Block {
                    self.stats.flushes += 1;
                    self.stats.sectors_flushed += n;
                }
                S_OK
            }
            T_GET_ID => {
                data = ID[..request.room.min(ID.len())].to_vec();
                S_OK
            }
            _ => S_UNSUPP,
        };
        if self.policy == Policy::Block && status != S_OK {
            self.stats.errors += 1;
        }
        let mut done = 0;
        for (offset, len) in request.output {
            let n = len.min(data.len().saturating_sub(done));
            if n > 0 {
                memory[offset..offset + n].copy_from_slice(&data[done..done + n]);
                dirty(offset, n);
                done += n;
            }
        }
        memory[request.status] = status;
        dirty(request.status, 1);
        transport.queues[0].complete_used(memory, chain, done + 1, &mut *dirty);
        out.irq = true;
        transport.isr |= 1;
        if self.policy == Policy::Block {
            out.completed += 1;
            out.log.push((request.kind, request.sector, status));
        }
    }
}

#[cfg(test)]
#[path = "virtio_block_tests.rs"]
mod tests;
