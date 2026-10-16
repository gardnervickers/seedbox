//! Modern virtio MMIO block device over the shared disk store: one request
//! queue, write-back cache with FLUSH, synchronous completion.
//!
//! A notify completes every available request in ring order before it
//! returns. Each request executes through its descriptors one sector at a
//! time (`request::execute`), then its used-ring entry is written. A malformed
//! ring, a request without header or status, or a chain longer than
//! `u32::MAX` bytes is an error; requests completed before it remain
//! completed. A driver reset (status 0) clears transport state only;
//! the store, including unflushed writes, is not a power loss.
use super::virtio::*;
use super::Result;
use crate::machine::disk::{
    request::{self, representable, Buffers},
    Store,
};
use serde::{Deserialize, Serialize};

const DEVICE_BLOCK: u32 = 2;
const FEATURE_SEG_MAX: u32 = 1 << 2;
const FEATURE_FLUSH: u32 = 1 << 9;
const FEATURES: u32 = FEATURE_SEG_MAX | FEATURE_FLUSH;
const FEATURE_VERSION_1_HIGH: u32 = 1;
const CAPACITY_LOW: u64 = 0x100;
const CAPACITY_HIGH: u64 = 0x104;
const SEG_MAX: u64 = 0x10c;

/// Guest-visible transport registers and queue cursor, captured separately
/// from the store, which crosses processes in its own encoding.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Transport {
    queue: [Queue; 1],
    selector: u32,
    feature_selector: u32,
    driver_selector: u32,
    features: [u32; 2],
    status: u32,
    interrupt: u32,
}

#[derive(Clone)]
pub(super) struct Block {
    transport: Transport,
    store: Store,
}

impl Block {
    pub(super) fn new(store: Store) -> Self {
        Self::restore(Transport::default(), store)
    }
    /// Reassemble a captured device; neither part is changed.
    pub(super) fn restore(transport: Transport, store: Store) -> Self {
        Self { transport, store }
    }
    pub(super) fn transport(&self) -> &Transport {
        &self.transport
    }
    pub(super) fn store(&self) -> &Store {
        &self.store
    }
    pub(super) fn irq(&self) -> bool {
        self.transport.interrupt != 0
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
        self.write_traced(ram, offset, value, |_, _, _| {})
    }
    pub(super) fn write_traced(
        &mut self,
        ram: &mut [u8],
        offset: u64,
        value: u32,
        mut completed: impl FnMut(u32, u8, u64),
    ) -> Result<()> {
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
                return self.process(ram, &mut completed);
            }
            INTERRUPT_ACK => self.transport.interrupt &= !value,
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
        Ok(())
    }
    fn process(&mut self, ram: &mut [u8], completed: &mut impl FnMut(u32, u8, u64)) -> Result<()> {
        if self.transport.status & STATUS_DRIVER_OK == 0 {
            return Ok(());
        }
        while let Some((head, chain)) =
            self.transport.queue[0].take_chain(ram, Direction::ReadThenWrite)?
        {
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
            let completion = request::execute(&mut self.store, &mut buffers)
                .map_err(|_| "virtio block request without header or status")?;
            if self.transport.queue[0].finish(ram, head, completion.written as usize)? {
                self.transport.interrupt |= USED_RING_INTERRUPT;
            }
            completed(kind, completion.status, sector);
        }
        Ok(())
    }
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
impl crate::machine::pv::codec::record::Record for Transport {
    const MIN: u64 = <[Queue; 1] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <[u32; 2] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN;
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
        } = self;
        queue.encode(w)?;
        selector.encode(w)?;
        feature_selector.encode(w)?;
        driver_selector.encode(w)?;
        features.encode(w)?;
        status.encode(w)?;
        interrupt.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            queue: crate::machine::pv::codec::record::Record::decode(r)?,
            selector: crate::machine::pv::codec::record::Record::decode(r)?,
            feature_selector: crate::machine::pv::codec::record::Record::decode(r)?,
            driver_selector: crate::machine::pv::codec::record::Record::decode(r)?,
            features: crate::machine::pv::codec::record::Record::decode(r)?,
            status: crate::machine::pv::codec::record::Record::decode(r)?,
            interrupt: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Transport {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        validate_transport(&self.queue, self.interrupt)?;
        Ok(())
    }
}
