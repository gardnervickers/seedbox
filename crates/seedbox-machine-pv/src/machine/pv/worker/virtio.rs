//! Virtio 1.x MMIO transport registers and split-ring mechanics shared by the
//! worker's devices. Rings and chains are checked in full before every DMA;
//! device personalities, features and request semantics stay with each device.
use super::boot::RAM_BASE as BASE;
use super::Result;
use serde::{Deserialize, Serialize};
pub(super) const MAGIC: u64 = 0x00;
pub(super) const VERSION: u64 = 0x04;
pub(super) const DEVICE_ID: u64 = 0x08;
pub(super) const VENDOR_ID: u64 = 0x0c;
pub(super) const DEVICE_FEATURES: u64 = 0x10;
pub(super) const DEVICE_FEATURES_SEL: u64 = 0x14;
pub(super) const DRIVER_FEATURES: u64 = 0x20;
pub(super) const DRIVER_FEATURES_SEL: u64 = 0x24;
pub(super) const QUEUE_SEL: u64 = 0x30;
pub(super) const QUEUE_NUM_MAX: u64 = 0x34;
pub(super) const QUEUE_NUM: u64 = 0x38;
pub(super) const QUEUE_READY: u64 = 0x44;
pub(super) const QUEUE_NOTIFY: u64 = 0x50;
pub(super) const INTERRUPT_STATUS: u64 = 0x60;
pub(super) const INTERRUPT_ACK: u64 = 0x64;
pub(super) const STATUS: u64 = 0x70;
pub(super) const QUEUE_DESC_LOW: u64 = 0x80;
pub(super) const QUEUE_DESC_HIGH: u64 = 0x84;
pub(super) const QUEUE_AVAIL_LOW: u64 = 0x90;
pub(super) const QUEUE_AVAIL_HIGH: u64 = 0x94;
pub(super) const QUEUE_USED_LOW: u64 = 0xa0;
pub(super) const QUEUE_USED_HIGH: u64 = 0xa4;
pub(super) const CONFIG_GENERATION: u64 = 0xfc;
pub(super) const STATUS_DRIVER_OK: u32 = 4;
pub(super) const STATUS_FEATURES_OK: u32 = 8;
pub(super) const USED_RING_INTERRUPT: u32 = 1;
pub(super) const QUEUE_LIMIT: u32 = 256;
pub(super) const DESCRIPTOR_NEXT: u16 = 1;
pub(super) const DESCRIPTOR_WRITE: u16 = 2;
pub(super) const AVAILABLE_NO_INTERRUPT: u16 = 1;
/// Descriptor directions a device accepts within one chain.
#[derive(Clone, Copy)]
pub(super) enum Direction {
    Readable,
    Writable,
    /// Device-readable descriptors, then device-writable ones.
    ReadThenWrite,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Queue {
    pub(super) size: u16,
    pub(super) ready: bool,
    pub(super) descriptor: u64,
    pub(super) available: u64,
    pub(super) used: u64,
    pub(super) cursor: u16,
}
#[derive(Clone)]
pub(super) struct Descriptor {
    pub(super) address: usize,
    pub(super) length: usize,
    pub(super) writable: bool,
}
pub(super) fn offset(ram: &[u8], address: u64, length: usize) -> Result<usize> {
    super::boot::RamLayout::offset_in_span(BASE, ram.len(), address, length)
}
pub(super) fn u16_at(ram: &[u8], address: u64) -> Result<u16> {
    let at = offset(ram, address, 2)?;
    Ok(u16::from_le_bytes(ram[at..at + 2].try_into().unwrap()))
}
fn add(address: u64, bytes: usize) -> Result<u64> {
    address
        .checked_add(bytes as u64)
        .ok_or("virtio address overflow".into())
}
impl Queue {
    /// Next chain whose descriptors are all device-readable or all writable.
    pub(super) fn take(
        &self,
        ram: &[u8],
        writable: bool,
    ) -> Result<Option<(u16, Vec<Descriptor>)>> {
        let direction = if writable {
            Direction::Writable
        } else {
            Direction::Readable
        };
        self.take_chain(ram, direction)
    }
    /// Validate the ring and the next available chain in full before any DMA.
    pub(super) fn take_chain(
        &self,
        ram: &[u8],
        direction: Direction,
    ) -> Result<Option<(u16, Vec<Descriptor>)>> {
        if !self.ready {
            return Ok(None);
        }
        if self.size == 0 || self.size > QUEUE_LIMIT as u16 || !self.size.is_power_of_two() {
            return Err("virtio invalid queue size".into());
        }
        offset(ram, self.descriptor, self.size as usize * 16)?;
        offset(ram, self.available, 6 + self.size as usize * 2)?;
        offset(ram, self.used, 6 + self.size as usize * 8)?;
        let available = u16_at(ram, add(self.available, 2)?)?;
        let pending = available.wrapping_sub(self.cursor);
        if pending > self.size {
            return Err("virtio available cursor overflow".into());
        }
        if pending == 0 {
            return Ok(None);
        }
        let head = u16_at(
            ram,
            add(self.available, 4 + (self.cursor % self.size) as usize * 2)?,
        )?;
        let mut result = Vec::new();
        let mut current = head;
        let mut seen = vec![false; self.size as usize];
        loop {
            if current >= self.size || seen[current as usize] {
                return Err("virtio invalid descriptor chain".into());
            }
            seen[current as usize] = true;
            let at = offset(ram, add(self.descriptor, current as usize * 16)?, 16)?;
            let d = &ram[at..at + 16];
            let address = u64::from_le_bytes(d[..8].try_into().unwrap());
            let length = u32::from_le_bytes(d[8..12].try_into().unwrap()) as usize;
            let flags = u16::from_le_bytes(d[12..14].try_into().unwrap());
            let writable = flags & DESCRIPTOR_WRITE != 0;
            let allowed = match direction {
                Direction::Readable => !writable,
                Direction::Writable => writable,
                Direction::ReadThenWrite => {
                    writable || result.iter().all(|d: &Descriptor| !d.writable)
                }
            };
            if flags & !(DESCRIPTOR_NEXT | DESCRIPTOR_WRITE) != 0 || !allowed {
                return Err("virtio descriptor direction or indirect flag".into());
            }
            result.push(Descriptor {
                address: offset(ram, address, length)?,
                length,
                writable,
            });
            if flags & DESCRIPTOR_NEXT == 0 {
                break;
            }
            current = u16::from_le_bytes(d[14..16].try_into().unwrap());
        }
        Ok(Some((head, result)))
    }
    pub(super) fn finish(&mut self, ram: &mut [u8], head: u16, length: usize) -> Result<bool> {
        let index = u16_at(ram, add(self.used, 2)?)?;
        let at = offset(
            ram,
            add(self.used, 4 + (index % self.size) as usize * 8)?,
            8,
        )?;
        ram[at..at + 4].copy_from_slice(&(head as u32).to_le_bytes());
        ram[at + 4..at + 8].copy_from_slice(&(length as u32).to_le_bytes());
        let at = offset(ram, add(self.used, 2)?, 2)?;
        ram[at..at + 2].copy_from_slice(&index.wrapping_add(1).to_le_bytes());
        self.cursor = self.cursor.wrapping_add(1);
        Ok(u16_at(ram, self.available)? & AVAILABLE_NO_INTERRUPT == 0)
    }
}

/// Apply a queue configuration register write to the selected queue.
pub(super) fn configure(
    queues: &mut [Queue],
    selector: u32,
    offset: u64,
    value: u32,
) -> Result<()> {
    let q = queues
        .get_mut(selector as usize)
        .ok_or("virtio invalid selected queue")?;
    if q.ready && offset != QUEUE_READY {
        return Err("virtio modify ready queue".into());
    }
    match offset {
        QUEUE_NUM => {
            if value > QUEUE_LIMIT {
                return Err("virtio queue too large".into());
            }
            q.size = value as u16;
        }
        QUEUE_READY => {
            if value > 1 {
                return Err("virtio invalid queue ready".into());
            }
            q.ready = value == 1;
        }
        QUEUE_DESC_LOW => q.descriptor = (q.descriptor & !0xffff_ffff) | value as u64,
        QUEUE_DESC_HIGH => q.descriptor = (q.descriptor & 0xffff_ffff) | ((value as u64) << 32),
        QUEUE_AVAIL_LOW => q.available = (q.available & !0xffff_ffff) | value as u64,
        QUEUE_AVAIL_HIGH => q.available = (q.available & 0xffff_ffff) | ((value as u64) << 32),
        QUEUE_USED_LOW => q.used = (q.used & !0xffff_ffff) | value as u64,
        QUEUE_USED_HIGH => q.used = (q.used & 0xffff_ffff) | ((value as u64) << 32),
        _ => return Err(format!("virtio unsupported write {offset:#x}")),
    }
    Ok(())
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Queue {
    const MIN: u64 = <u16 as crate::machine::pv::codec::record::Record>::MIN
        + <bool as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u16 as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            size,
            ready,
            descriptor,
            available,
            used,
            cursor,
        } = self;
        size.encode(w)?;
        ready.encode(w)?;
        descriptor.encode(w)?;
        available.encode(w)?;
        used.encode(w)?;
        cursor.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            size: crate::machine::pv::codec::record::Record::decode(r)?,
            ready: crate::machine::pv::codec::record::Record::decode(r)?,
            descriptor: crate::machine::pv::codec::record::Record::decode(r)?,
            available: crate::machine::pv::codec::record::Record::decode(r)?,
            used: crate::machine::pv::codec::record::Record::decode(r)?,
            cursor: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Queue {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        // Configure accepts zero/non-power-of-two sizes and ring addresses
        // independently. Actual DMA validation remains Queue::take_chain.
        if u32::from(self.size) > QUEUE_LIMIT {
            return Err("PV snapshot queue size".into());
        }
        Ok(())
    }
}
pub(super) fn validate_transport(queues: &[Queue], interrupt: u32) -> Result<()> {
    for q in queues {
        q.validate_snapshot()?;
    }
    if interrupt & !USED_RING_INTERRUPT != 0 {
        return Err("PV snapshot interrupt bits".into());
    }
    // Feature words remain guest-writable even after FEATURES_OK. A guest may
    // rewrite them without another STATUS write, so no current-value mask or
    // FEATURES_OK implication is a capture invariant.
    Ok(())
}
