//! Modern virtio MMIO block device over the shared disk store: one request
//! queue, write-back cache with FLUSH.
//!
//! Without disk faults a notify completes every available request in ring
//! order before it returns. Each request executes through its descriptors
//! one sector at a time (`request::execute`), then its used-ring entry is
//! written. A malformed ring, a request without header or status, or a chain
//! longer than `u32::MAX` bytes is an error; requests completed before it
//! remain completed. A driver reset (status 0) clears transport state only;
//! the store, including unflushed writes, is not a power loss.
//!
//! The shared disk fault policy (`disk::fault`) sees each request when the
//! notify takes it and when it executes (docs/faults.md). A held request
//! stops processing where it is. A `disk_latency` fault consumes requests
//! into a pending queue instead, each with a due machine time, never before
//! the request ahead of it; later requests queue behind them. At its due
//! time a request is decoded again from guest RAM and completes exactly as
//! an immediate one would. The queue belongs to the transport, so a driver
//! reset drops it; the faults survive a reset.
use super::virtio::*;
use super::Result;
use seedbox_machine::machine::disk::{
    fault::{Arrival, DiskFaults, Request},
    request::{representable, Buffers},
    Store,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const DEVICE_BLOCK: u32 = 2;
const FEATURE_SEG_MAX: u32 = 1 << 2;
const FEATURE_FLUSH: u32 = 1 << 9;
const FEATURES: u32 = FEATURE_SEG_MAX | FEATURE_FLUSH;
const FEATURE_VERSION_1_HIGH: u32 = 1;
const CAPACITY_LOW: u64 = 0x100;
const CAPACITY_HIGH: u64 = 0x104;
const SEG_MAX: u64 = 0x10c;

/// Guest-visible transport registers, queue cursor and the requests it
/// consumed but has not completed, captured separately from the store,
/// which crosses processes in its own encoding.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Transport {
    queue: [Queue; 1],
    selector: u32,
    feature_selector: u32,
    driver_selector: u32,
    features: [u32; 2],
    status: u32,
    interrupt: u32,
    /// Consumed requests awaiting completion, in submission order.
    #[serde(default, skip_serializing_if = "VecDeque::is_empty")]
    pending: VecDeque<Pending>,
}

/// A consumed request: its chain head and when it completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pending {
    head: u16,
    due: u64,
}

#[derive(Clone)]
pub(super) struct Block {
    transport: Transport,
    store: Store,
    /// Disk faults and holds of this boot; they survive a driver reset.
    faults: DiskFaults,
}

/// A request completed by the device: its type, status and first sector.
pub(super) type Completed<'c> = &'c mut dyn FnMut(u32, u8, u64);

impl Block {
    pub(super) fn new(store: Store, faults: DiskFaults) -> Self {
        Self::restore(Transport::default(), store, faults)
    }
    /// Reassemble a captured device; no part is changed.
    pub(super) fn restore(transport: Transport, store: Store, faults: DiskFaults) -> Self {
        Self {
            transport,
            store,
            faults,
        }
    }
    pub(super) fn transport(&self) -> &Transport {
        &self.transport
    }
    pub(super) fn faults(&self) -> &DiskFaults {
        &self.faults
    }
    pub(super) fn faults_mut(&mut self) -> &mut DiskFaults {
        &mut self.faults
    }
    /// The machine time the earliest pending request completes.
    pub(super) fn next_due(&self) -> Option<u64> {
        self.transport.pending.front().map(|p| p.due)
    }
    pub(super) fn store(&self) -> &Store {
        &self.store
    }
    pub(super) fn interrupt_status(&self) -> u32 {
        self.transport.interrupt
    }
    pub(super) fn acknowledge_interrupt(&mut self, bits: u32) {
        self.transport.interrupt &= !bits;
    }
    pub(super) fn irq(&self) -> bool {
        self.interrupt_status() != 0
    }
    pub(super) fn read(&self, offset: u64) -> Result<u32> {
        let selected = self.transport.selector == 0;
        Ok(match offset {
            MAGIC => 0x74726976,
            VERSION => 2,
            DEVICE_ID => DEVICE_BLOCK,
            VENDOR_ID => 0x534258,
            DEVICE_FEATURES => match self.transport.feature_selector {
                0 => FEATURES,
                1 => FEATURE_VERSION_1_HIGH,
                _ => 0,
            },
            QUEUE_NUM_MAX => selected as u32 * QUEUE_LIMIT,
            QUEUE_READY => (selected && self.transport.queue[0].ready) as u32,
            INTERRUPT_STATUS => self.transport.interrupt,
            STATUS => self.transport.status,
            CONFIG_GENERATION => 0,
            CAPACITY_LOW => self.store.capacity() as u32,
            CAPACITY_HIGH => (self.store.capacity() >> 32) as u32,
            // A chain also needs its header and status descriptors.
            SEG_MAX => QUEUE_LIMIT - 2,
            _ => return Err(format!("virtio block unsupported read {offset:#x}")),
        })
    }
    #[cfg(test)]
    pub(super) fn write(&mut self, ram: &mut [u8], offset: u64, value: u32) -> Result<()> {
        self.write_traced(ram, offset, value, 0, &mut |_, _, _| {})
            .map(|_| ())
    }
    /// A register write at machine time `now`. A notify returns the trigger
    /// whose hold now holds a request; the device then processes nothing
    /// more, and the machine must not be entered again.
    pub(super) fn write_traced(
        &mut self,
        ram: &mut [u8],
        offset: u64,
        value: u32,
        now: u64,
        completed: Completed<'_>,
    ) -> Result<Option<u32>> {
        match offset {
            DEVICE_FEATURES_SEL => self.transport.feature_selector = value,
            DRIVER_FEATURES_SEL => self.transport.driver_selector = value,
            DRIVER_FEATURES => {
                if self.transport.driver_selector < 2 {
                    self.transport.features[self.transport.driver_selector as usize] = value;
                }
            }
            QUEUE_SEL => self.transport.selector = value,
            QUEUE_NUM | QUEUE_READY | QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_AVAIL_LOW
            | QUEUE_AVAIL_HIGH | QUEUE_USED_LOW | QUEUE_USED_HIGH => configure(
                &mut self.transport.queue,
                self.transport.selector,
                offset,
                value,
            )?,
            QUEUE_NOTIFY => {
                if value != 0 {
                    return Err("virtio block notify invalid queue".into());
                }
                return self.process(ram, now, completed);
            }
            INTERRUPT_ACK => self.acknowledge_interrupt(value),
            STATUS if value == 0 => self.transport = Transport::default(),
            STATUS => {
                if value & STATUS_FEATURES_OK != 0
                    && (self.transport.features[1] != FEATURE_VERSION_1_HIGH
                        || self.transport.features[0] & !FEATURES != 0)
                {
                    return Err("virtio block unsupported features".into());
                }
                self.transport.status = value;
            }
            _ => return Err(format!("virtio block unsupported write {offset:#x}")),
        }
        Ok(None)
    }
    fn process(
        &mut self,
        ram: &mut [u8],
        now: u64,
        completed: Completed<'_>,
    ) -> Result<Option<u32>> {
        if self.transport.status & STATUS_DRIVER_OK == 0 {
            return Ok(None);
        }
        while let Some((head, chain)) =
            self.transport.queue[0].take_chain(ram, Direction::ReadThenWrite)?
        {
            let request = if self.faults.is_empty() {
                None
            } else {
                Some(classify(ram, &chain)?)
            };
            let arrival = match request {
                Some(request) => self.faults.arrive(request, now),
                None => Arrival::Now,
            };
            let last = self.transport.pending.back().map(|p| p.due);
            let due = match arrival {
                Arrival::Held(trigger) => return Ok(Some(trigger)),
                Arrival::Now if last.is_none() => {
                    self.complete(ram, head, chain, true, completed)?;
                    continue;
                }
                Arrival::Now => now,
                Arrival::Delay(delay) => now
                    .checked_add(delay)
                    .ok_or("virtio block completion time overflow")?,
            };
            // Completion order stays submission order.
            let due = due.max(last.unwrap_or(due));
            self.transport.queue[0].consume();
            self.transport.pending.push_back(Pending { head, due });
        }
        Ok(None)
    }
    /// Complete every pending request due by machine time `now`, decoding
    /// each again from guest RAM, in submission order.
    pub(super) fn complete_due(
        &mut self,
        ram: &mut [u8],
        now: u64,
        completed: Completed<'_>,
    ) -> Result<()> {
        while let Some(&Pending { head, due }) = self.transport.pending.front() {
            if due > now {
                break;
            }
            self.transport.pending.pop_front();
            let chain = self.transport.queue[0].chain(ram, head, Direction::ReadThenWrite)?;
            self.complete(ram, head, chain, false, completed)?;
        }
        Ok(())
    }
    /// Execute one request and write its used entry; `take` also advances
    /// the available cursor, for a request completed when it is taken.
    fn complete(
        &mut self,
        ram: &mut [u8],
        head: u16,
        chain: Vec<Descriptor>,
        take: bool,
        completed: Completed<'_>,
    ) -> Result<()> {
        let (readable, writable): (Vec<_>, Vec<_>) = chain.iter().partition(|d| !d.writable);
        let mut buffers = Chain {
            ram,
            readable: Streams::new(&readable),
            writable: Streams::new(&writable),
        };
        if !representable(buffers.readable.length, buffers.writable.length) {
            return Err("virtio block chain exceeds 2^32 - 1 bytes".into());
        }
        // Read only the actual request header before effects/aliasing can
        // overwrite it; completion order is the used-ring commit order.
        let mut header = [0u8; 16];
        if buffers.readable() >= header.len() {
            buffers.read(0, &mut header);
        }
        let kind = u32::from_le_bytes(header[..4].try_into().expect("header kind"));
        let sector = u64::from_le_bytes(header[8..].try_into().expect("header sector"));
        let request = Request::decode(&header, buffers.readable(), buffers.writable());
        let completion = self
            .faults
            .execute(&mut self.store, &mut buffers, request)
            .map_err(|_| "virtio block request without header or status")?;
        let queue = &mut self.transport.queue[0];
        let interrupt = if take {
            queue.finish(ram, head, completion.written as usize)?
        } else {
            queue.used(ram, head, completion.written as usize)?
        };
        if interrupt {
            self.transport.interrupt |= USED_RING_INTERRUPT;
        }
        completed(kind, completion.status, sector);
        Ok(())
    }
}

/// Classify a taken request from its header for the fault policy, without
/// any effect. Chains too long to answer fail as they would on completion.
fn classify(ram: &mut [u8], chain: &[Descriptor]) -> Result<Request> {
    let (readable, writable): (Vec<_>, Vec<_>) = chain.iter().partition(|d| !d.writable);
    let mut buffers = Chain {
        ram,
        readable: Streams::new(&readable),
        writable: Streams::new(&writable),
    };
    if !representable(buffers.readable.length, buffers.writable.length) {
        return Err("virtio block chain exceeds 2^32 - 1 bytes".into());
    }
    let mut header = [0u8; 16];
    if buffers.readable() >= header.len() {
        buffers.read(0, &mut header);
    }
    Ok(Request::decode(
        &header,
        buffers.readable(),
        buffers.writable(),
    ))
}

/// One direction of a validated chain as a logical stream of RAM spans.
struct Streams {
    spans: Vec<(usize, usize)>,
    length: usize,
}

impl Streams {
    fn new(descriptors: &[&Descriptor]) -> Self {
        let spans: Vec<_> = descriptors.iter().map(|d| (d.address, d.length)).collect();
        let length = spans.iter().map(|&(_, length)| length).sum();
        Self { spans, length }
    }
    /// Visit the RAM ranges covering `length` bytes from `offset`, in order.
    fn each(&self, mut offset: usize, mut length: usize, mut visit: impl FnMut(usize, usize)) {
        for &(address, span) in &self.spans {
            if length == 0 {
                break;
            }
            if offset >= span {
                offset -= span;
                continue;
            }
            let count = (span - offset).min(length);
            visit(address + offset, count);
            (offset, length) = (0, length - count);
        }
        assert_eq!(length, 0, "request stream access out of range");
    }
}

/// A request's descriptors over guest RAM. Accesses copy through RAM in chain
/// order, so aliasing descriptors observe earlier writes.
struct Chain<'a> {
    ram: &'a mut [u8],
    readable: Streams,
    writable: Streams,
}

impl Buffers for Chain<'_> {
    fn readable(&self) -> usize {
        self.readable.length
    }
    fn writable(&self) -> usize {
        self.writable.length
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        let mut done = 0;
        self.readable.each(offset, out.len(), |at, count| {
            out[done..done + count].copy_from_slice(&self.ram[at..at + count]);
            done += count;
        });
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        let mut done = 0;
        let ram = &mut *self.ram;
        self.writable.each(offset, bytes.len(), |at, count| {
            ram[at..at + count].copy_from_slice(&bytes[done..done + count]);
            done += count;
        });
    }
}

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Transport {
    const MIN: u64 = <[Queue; 1] as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <[u32; 2] as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <VecDeque<Pending> as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            queue,
            selector,
            feature_selector,
            driver_selector,
            features,
            status,
            interrupt,
            pending,
        } = self;
        queue.encode(w)?;
        selector.encode(w)?;
        feature_selector.encode(w)?;
        driver_selector.encode(w)?;
        features.encode(w)?;
        status.encode(w)?;
        interrupt.encode(w)?;
        pending.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            queue: seedbox_machine::snapshot::Record::decode(r)?,
            selector: seedbox_machine::snapshot::Record::decode(r)?,
            feature_selector: seedbox_machine::snapshot::Record::decode(r)?,
            driver_selector: seedbox_machine::snapshot::Record::decode(r)?,
            features: seedbox_machine::snapshot::Record::decode(r)?,
            status: seedbox_machine::snapshot::Record::decode(r)?,
            interrupt: seedbox_machine::snapshot::Record::decode(r)?,
            pending: seedbox_machine::snapshot::Record::decode(r)?,
        })
    }
}
seedbox_machine::record!(Pending {
    head: u16,
    due: u64
});

impl Transport {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        validate_transport(&self.queue, self.interrupt)?;
        let size = self.queue[0].size;
        if self.pending.len() > usize::from(size)
            || self.pending.iter().any(|p| p.head >= size)
            || self
                .pending
                .iter()
                .zip(self.pending.iter().skip(1))
                .any(|(a, b)| a.due > b.due)
        {
            return Err("PV snapshot pending block requests".into());
        }
        Ok(())
    }
}
