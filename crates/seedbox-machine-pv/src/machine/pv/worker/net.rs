//! Modern virtio MMIO network device: one Ethernet interface with a fixed MAC
//! and 1500-byte MTU. RX is queue 0 and TX queue 1. No offloads, control queue,
//! merged buffers, indirect descriptors or event index.
//!
//! Every frame is 14..=1514 bytes: Ethernet header and payload without FCS.
//! Each chain carries the 12-byte modern header first. Rings are checked in
//! full before DMA (`virtio.rs`). Malformed rings or descriptor directions are
//! errors, and chains completed before one remain completed.
//!
//! Before DRIVER_OK both queue notifies are ignored.
//!
//! A TX notify completes every available chain in order at the notifying
//! rendezvous. Each frame within bounds whose header requests no offload
//! (`flags` and `gso_type` zero) is stamped with that time. The remaining
//! header fields are ignored. Other frames are dropped, and their completion
//! is still visible to the guest. Their size is known before anything is
//! gathered, so a guest cannot force an allocation larger than one frame.
//!
//! The host offers RX frames through [`Net::receive`]. Admission always
//! consumes a valid frame; delivery to the guest is separate. Frames are
//! dropped before DRIVER_OK or while RX is not ready. A host frame outside the
//! bounds is a caller error. Admission first drains the backlog into free
//! buffers, then delivers the new frame or appends it to a FIFO backlog of
//! [`BACKLOG`] frames. A full backlog drops the new frame. A refill notify
//! drains the backlog. A structurally valid RX chain too short for header and
//! frame is completed with length 0 and that frame is dropped, with no payload DMA.
//! Driver reset and RX queue disable clear the backlog. Frames already
//! transmitted stay available for [`Net::take_transmitted`]. The whole device,
//! including backlog and untaken TX, belongs to machine snapshots.
//
// TODO: Re-evaluate: the 256-frame backlog follows legacy engine-core
// `virtio_network.rs`. Dropping into a short buffer, unlike legacy truncation,
// matches the TCG device and cannot stall the ring; Linux posts 1526 bytes.
use super::virtio::*;
use super::Result;
use crate::pv::ethernet::{
    MAX_FRAME_BYTES as MAX_FRAME, MIN_FRAME_BYTES as MIN_FRAME, RECEIVE_BACKLOG_FRAMES as BACKLOG,
};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// Modern virtio-net header, including `num_buffers`.
const HEADER: usize = 12;
/// Header bytes 0 and 1: `flags` and `gso_type`, which request offloads.
const OFFLOAD_FIELDS: usize = 2;
/// Header byte of the little-endian `num_buffers` field.
const NUM_BUFFERS: usize = 10;
const DEVICE_NET: u32 = 1;
const RX: usize = 0;
const TX: usize = 1;
const FEATURE_MTU: u32 = 1 << 3;
const FEATURE_MAC: u32 = 1 << 5;
const FEATURES: u32 = FEATURE_MTU | FEATURE_MAC;
const FEATURE_VERSION_1_HIGH: u32 = 1;
const CONFIG: u64 = 0x100;
const CONFIG_MTU: u64 = CONFIG + 10;
const CONFIG_END: u64 = CONFIG + 12;
const MTU: u16 = 1500;

/// One guest transmission, stamped at its TX notify rendezvous.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Frame {
    pub(super) time: u64,
    pub(super) bytes: Vec<u8>,
}

/// Disposition of one host frame at admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Received {
    /// Written to a guest RX buffer.
    Delivered,
    /// Queued until the guest posts a buffer.
    Backlogged,
    /// Discarded: driver not ready, full backlog or short RX buffer.
    Dropped,
}

/// Device counters; diagnostic only.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Stats {
    pub(super) tx_frames: u64,
    pub(super) tx_dropped: u64,
    pub(super) rx_frames: u64,
    pub(super) rx_dropped: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Net {
    queues: [Queue; 2],
    selector: u32,
    feature_selector: u32,
    driver_selector: u32,
    features: [u32; 2],
    status: u32,
    interrupt: u32,
    mac: [u8; 6],
    backlog: VecDeque<Vec<u8>>,
    transmitted: Vec<Frame>,
    stats: Stats,
}

impl Net {
    pub(super) fn new(mac: [u8; 6]) -> Self {
        Self {
            queues: Default::default(),
            selector: 0,
            feature_selector: 0,
            driver_selector: 0,
            features: [0; 2],
            status: 0,
            interrupt: 0,
            mac,
            backlog: VecDeque::new(),
            transmitted: Vec::new(),
            stats: Stats::default(),
        }
    }
    pub(super) fn mac(&self) -> [u8; 6] {
        self.mac
    }
    pub(super) fn irq(&self) -> bool {
        self.interrupt != 0
    }
    #[cfg_attr(not(test), allow(dead_code))]
    pub(super) fn stats(&self) -> &Stats {
        &self.stats
    }
    /// Frames transmitted since the last call, in guest order.
    pub(super) fn take_transmitted(&mut self) -> Vec<Frame> {
        std::mem::take(&mut self.transmitted)
    }
    /// Common registers take 32-bit accesses. Device configuration takes 8,
    /// 16 or 32-bit accesses within its fields: Linux reads `mac` bytewise.
    pub(super) fn read(&self, offset: u64, width: u8) -> Result<u32> {
        if (CONFIG..CONFIG_END).contains(&offset) {
            return self.config(offset, width);
        }
        if width != 4 {
            return Err(format!("virtio net {width}-byte read {offset:#x}"));
        }
        let queue = self.queues.get(self.selector as usize);
        Ok(match offset {
            MAGIC => 0x74726976,
            VERSION => 2,
            DEVICE_ID => DEVICE_NET,
            VENDOR_ID => 0x534258,
            DEVICE_FEATURES => match self.feature_selector {
                0 => FEATURES,
                1 => FEATURE_VERSION_1_HIGH,
                _ => 0,
            },
            QUEUE_NUM_MAX => queue.map_or(0, |_| QUEUE_LIMIT),
            QUEUE_READY => queue.is_some_and(|q| q.ready) as u32,
            INTERRUPT_STATUS => self.interrupt,
            STATUS => self.status,
            CONFIG_GENERATION => 0,
            _ => return Err(format!("virtio net unsupported read {offset:#x}")),
        })
    }
    fn config(&self, offset: u64, width: u8) -> Result<u32> {
        let at = (offset - CONFIG) as usize;
        let width = width as usize;
        if !matches!(width, 1 | 2 | 4) || !at.is_multiple_of(width) || at + width > 12 {
            return Err(format!("virtio net config access {offset:#x}/{width}"));
        }
        // mac[6], status (not offered), max_virtqueue_pairs (not offered), mtu.
        let mut space = [0u8; 12];
        space[..self.mac.len()].copy_from_slice(&self.mac);
        space[(CONFIG_MTU - CONFIG) as usize..].copy_from_slice(&MTU.to_le_bytes());
        let mut value = [0u8; 4];
        value[..width].copy_from_slice(&space[at..at + width]);
        Ok(u32::from_le_bytes(value))
    }
    /// One guest register write at the stopped rendezvous `now`. A TX notify
    /// stamps its frames with `now`; an RX notify drains the backlog.
    pub(super) fn write(
        &mut self,
        ram: &mut [u8],
        offset: u64,
        width: u8,
        value: u32,
        now: u64,
    ) -> Result<()> {
        if width != 4 {
            return Err(format!("virtio net {width}-byte write {offset:#x}"));
        }
        match offset {
            DEVICE_FEATURES_SEL => self.feature_selector = value,
            DRIVER_FEATURES_SEL => self.driver_selector = value,
            DRIVER_FEATURES => {
                if self.driver_selector < 2 {
                    self.features[self.driver_selector as usize] = value;
                }
            }
            QUEUE_SEL => self.selector = value,
            QUEUE_NUM | QUEUE_READY | QUEUE_DESC_LOW | QUEUE_DESC_HIGH | QUEUE_AVAIL_LOW
            | QUEUE_AVAIL_HIGH | QUEUE_USED_LOW | QUEUE_USED_HIGH => {
                configure(&mut self.queues, self.selector, offset, value)?;
                if self.selector as usize == RX && !self.queues[RX].ready {
                    self.backlog.clear();
                }
            }
            QUEUE_NOTIFY => match value as usize {
                RX => self.drain(ram)?,
                TX => self.transmit(ram, now)?,
                _ => return Err("virtio net notify invalid queue".into()),
            },
            INTERRUPT_ACK => self.interrupt &= !value,
            STATUS if value == 0 => {
                // Transport reset. Untaken TX frames were already emitted.
                let transmitted = std::mem::take(&mut self.transmitted);
                let stats = std::mem::take(&mut self.stats);
                *self = Self::new(self.mac);
                self.transmitted = transmitted;
                self.stats = stats;
            }
            STATUS => {
                if value & STATUS_FEATURES_OK != 0
                    && (self.features[1] != FEATURE_VERSION_1_HIGH
                        || self.features[0] & !FEATURES != 0)
                {
                    return Err("virtio net unsupported features".into());
                }
                self.status = value;
            }
            _ => return Err(format!("virtio net unsupported write {offset:#x}")),
        }
        Ok(())
    }
    fn ready(&self) -> bool {
        self.status & STATUS_DRIVER_OK != 0
    }
    fn complete(&mut self, ram: &mut [u8], queue: usize, head: u16, length: usize) -> Result<()> {
        if self.queues[queue].finish(ram, head, length)? {
            self.interrupt |= USED_RING_INTERRUPT;
        }
        Ok(())
    }
    fn transmit(&mut self, ram: &mut [u8], now: u64) -> Result<()> {
        if !self.ready() {
            return Ok(());
        }
        while let Some((head, chain)) = self.queues[TX].take_chain(ram, Direction::Readable)? {
            // At most QUEUE_LIMIT descriptors of < 2^32 bytes: no overflow.
            let length: usize = chain.iter().map(|d| d.length).sum();
            let mut frame = None;
            if (HEADER + MIN_FRAME..=HEADER + MAX_FRAME).contains(&length) {
                let mut bytes = Vec::with_capacity(length);
                for d in &chain {
                    bytes.extend_from_slice(&ram[d.address..d.address + d.length]);
                }
                if bytes[..OFFLOAD_FIELDS].iter().all(|&b| b == 0) {
                    bytes.drain(..HEADER);
                    frame = Some(bytes);
                }
            }
            match frame {
                Some(bytes) => {
                    self.stats.tx_frames += 1;
                    self.transmitted.push(Frame { time: now, bytes });
                }
                None => self.stats.tx_dropped += 1,
            }
            self.complete(ram, TX, head, 0)?;
        }
        Ok(())
    }
    /// Admit one host frame. It is always consumed; see the module docs.
    /// A frame outside [`MIN_FRAME`]..=[`MAX_FRAME`] violates the caller's
    /// contract and is an error, not a guest-visible drop.
    pub(super) fn receive(&mut self, ram: &mut [u8], frame: Vec<u8>) -> Result<Received> {
        if !(MIN_FRAME..=MAX_FRAME).contains(&frame.len()) {
            return Err(format!("virtio net {}-byte host frame", frame.len()));
        }
        if !self.ready() || !self.queues[RX].ready {
            self.stats.rx_dropped += 1;
            return Ok(Received::Dropped);
        }
        self.drain(ram)?;
        if self.backlog.is_empty() {
            match self.deliver(ram, &frame)? {
                Some(true) => return Ok(Received::Delivered),
                Some(false) => return Ok(Received::Dropped),
                None => (),
            }
        }
        if self.backlog.len() >= BACKLOG {
            self.stats.rx_dropped += 1;
            return Ok(Received::Dropped);
        }
        self.backlog.push_back(frame);
        Ok(Received::Backlogged)
    }
    fn drain(&mut self, ram: &mut [u8]) -> Result<()> {
        if !self.ready() {
            return Ok(());
        }
        while let Some(frame) = self.backlog.pop_front() {
            if self.deliver(ram, &frame)?.is_none() {
                self.backlog.push_front(frame);
                break;
            }
        }
        Ok(())
    }
    /// Offer `frame` to the next RX chain: `None` when no chain is free,
    /// `Some(false)` when the chain was too short and the frame was dropped.
    fn deliver(&mut self, ram: &mut [u8], frame: &[u8]) -> Result<Option<bool>> {
        let Some((head, chain)) = self.queues[RX].take_chain(ram, Direction::Writable)? else {
            return Ok(None);
        };
        // At most QUEUE_LIMIT descriptors of < 2^32 bytes: no overflow.
        let room: usize = chain.iter().map(|d| d.length).sum();
        if room < HEADER + frame.len() {
            self.stats.rx_dropped += 1;
            self.complete(ram, RX, head, 0)?;
            return Ok(Some(false));
        }
        // Bounded by MAX_FRAME; admission checked the length.
        let mut packet = Vec::with_capacity(HEADER + frame.len());
        packet.extend_from_slice(&[0; HEADER]);
        packet[NUM_BUFFERS] = 1;
        packet.extend_from_slice(frame);
        let mut written = 0;
        for d in &chain {
            let length = d.length.min(packet.len() - written);
            ram[d.address..d.address + length].copy_from_slice(&packet[written..written + length]);
            written += length;
        }
        self.stats.rx_frames += 1;
        self.complete(ram, RX, head, written)?;
        Ok(Some(true))
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod resource_tests;

#[cfg(test)]
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Frame {
    const MIN: u64 = <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <Vec<u8> as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self { time, bytes } = self;
        time.encode(w)?;
        bytes.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            time: crate::machine::pv::codec::record::Record::decode(r)?,
            bytes: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Stats {
    const MIN: u64 = <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN
        + <u64 as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            tx_frames,
            tx_dropped,
            rx_frames,
            rx_dropped,
        } = self;
        tx_frames.encode(w)?;
        tx_dropped.encode(w)?;
        rx_frames.encode(w)?;
        rx_dropped.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            tx_frames: crate::machine::pv::codec::record::Record::decode(r)?,
            tx_dropped: crate::machine::pv::codec::record::Record::decode(r)?,
            rx_frames: crate::machine::pv::codec::record::Record::decode(r)?,
            rx_dropped: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Net {
    const MIN: u64 = <[Queue; 2] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <[u32; 2] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <[u8; 6] as crate::machine::pv::codec::record::Record>::MIN
        + <VecDeque<Vec<u8>> as crate::machine::pv::codec::record::Record>::MIN
        + <Vec<Frame> as crate::machine::pv::codec::record::Record>::MIN
        + <Stats as crate::machine::pv::codec::record::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            queues,
            selector,
            feature_selector,
            driver_selector,
            features,
            status,
            interrupt,
            mac,
            backlog,
            transmitted,
            stats,
        } = self;
        queues.encode(w)?;
        selector.encode(w)?;
        feature_selector.encode(w)?;
        driver_selector.encode(w)?;
        features.encode(w)?;
        status.encode(w)?;
        interrupt.encode(w)?;
        mac.encode(w)?;
        backlog.encode(w)?;
        transmitted.encode(w)?;
        stats.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        Ok(Self {
            queues: crate::machine::pv::codec::record::Record::decode(r)?,
            selector: crate::machine::pv::codec::record::Record::decode(r)?,
            feature_selector: crate::machine::pv::codec::record::Record::decode(r)?,
            driver_selector: crate::machine::pv::codec::record::Record::decode(r)?,
            features: crate::machine::pv::codec::record::Record::decode(r)?,
            status: crate::machine::pv::codec::record::Record::decode(r)?,
            interrupt: crate::machine::pv::codec::record::Record::decode(r)?,
            mac: crate::machine::pv::codec::record::Record::decode(r)?,
            backlog: crate::machine::pv::codec::record::Record::decode(r)?,
            transmitted: crate::machine::pv::codec::record::Record::decode(r)?,
            stats: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Net {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        validate_transport(&self.queues, self.interrupt)?;
        if self.backlog.len() > BACKLOG
            || self.backlog.iter().any(|f| !crate::pv::frame_in_bounds(f))
            || !self.transmitted.is_empty()
        {
            return Err("PV snapshot NIC backlog or undrained TX".into());
        }
        // A nonzero STATUS write can clear DRIVER_OK without resetting FIFO.
        // Only RX-not-ready/reset clear backlog in every source transition.
        if !self.backlog.is_empty() && !self.queues[RX].ready {
            return Err("PV snapshot NIC backlog ownership".into());
        }
        Ok(())
    }
}
