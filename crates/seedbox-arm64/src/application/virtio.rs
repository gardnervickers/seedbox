//! Modern virtio MMIO and bounded split queues over ARM guest physical RAM.
//! Every DMA span and descriptor chain is checked before a device effect.
use super::Event;
use crate::{disk::Disk, Result, RAM_BASE};
use serde::Serialize;
use std::collections::VecDeque;

const QUEUE_SIZE: u16 = 256;
const CHAIN_LIMIT: usize = 4 << 20;
const NET_HEADER: usize = 12;

fn u16_at(m: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(m[offset..offset + 2].try_into().expect("validated DMA"))
}
fn u32_at(m: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(m[offset..offset + 4].try_into().expect("validated DMA"))
}
fn u64_at(m: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(m[offset..offset + 8].try_into().expect("validated DMA"))
}

use seedbox_engine_core::virtqueue::{Chain, Diagnostics, Direction, Policy, Queue};

trait ArmQueue {
    fn chain(&self, memory: &[u8]) -> Result<Option<Chain>>;
    fn complete(&mut self, memory: &mut [u8], chain: &Chain, len: usize);
}
impl ArmQueue for Queue {
    fn chain(&self, memory: &[u8]) -> Result<Option<Chain>> {
        self.validated_chain(
            memory,
            RAM_BASE,
            Policy {
                limit: CHAIN_LIMIT,
                direction: Direction::Any,
                reject_empty: true,
                diagnostics: Diagnostics::Virtio,
            },
        )
    }
    fn complete(&mut self, memory: &mut [u8], chain: &Chain, len: usize) {
        self.complete_chain(memory, chain, len, |_, _| {});
    }
}

pub enum Model {
    Net {
        mac: [u8; 6],
        backlog: VecDeque<Vec<u8>>,
    },
    Console {
        device_ready: bool,
        port_ready: bool,
        guest_open: bool,
        rx: VecDeque<u8>,
        control_rx: VecDeque<Vec<u8>>,
        tx_bytes: u64,
        rx_dma_bytes: u64,
    },
    Block(Disk),
}
#[derive(Serialize)]
struct Transport {
    dev_sel: u32,
    drv_sel: u32,
    features: [u32; 2],
    queue_sel: u32,
    queues: Vec<Queue>,
    status: u32,
    isr: u32,
}
pub struct Virtio {
    transport: Transport,
    model: Model,
}
impl Virtio {
    pub fn net(mac: [u8; 6]) -> Self {
        Self::new(
            Model::Net {
                mac,
                backlog: VecDeque::new(),
            },
            2,
        )
    }
    pub fn console() -> Self {
        Self::new(
            Model::Console {
                device_ready: false,
                port_ready: false,
                guest_open: false,
                rx: VecDeque::new(),
                control_rx: VecDeque::new(),
                tx_bytes: 0,
                rx_dma_bytes: 0,
            },
            4,
        )
    }
    pub fn block(disk: Disk) -> Self {
        Self::new(Model::Block(disk), 1)
    }
    fn new(model: Model, queues: usize) -> Self {
        Self {
            model,
            transport: Transport {
                dev_sel: 0,
                drv_sel: 0,
                features: [0; 2],
                queue_sel: 0,
                queues: vec![Queue::default(); queues],
                status: 0,
                isr: 0,
            },
        }
    }
    pub fn irq(&self) -> bool {
        self.transport.isr != 0
    }
    /// Versioned application state includes transport cursors, pending ingress
    /// and durable/volatile disk layers, not merely bytes visible to the guest.
    pub fn state_digest(&self) -> Result<String> {
        let model = match &self.model {
            Model::Net { mac, backlog } => {
                serde_json::json!({"kind":"net","mac":mac,"backlog":backlog})
            }
            Model::Console {
                device_ready,
                port_ready,
                guest_open,
                rx,
                control_rx,
                tx_bytes,
                rx_dma_bytes,
            } => {
                serde_json::json!({"kind":"console","device_ready":device_ready,"port_ready":port_ready,"guest_open":guest_open,"rx":rx,"control_rx":control_rx,"tx_bytes":tx_bytes,"rx_dma_bytes":rx_dma_bytes})
            }
            Model::Block(disk) => serde_json::json!({"kind":"block","disk":disk.digest()}),
        };
        let bytes = serde_json::to_vec(&(&self.transport, model)).map_err(|e| e.to_string())?;
        Ok(crate::witness::hash(&bytes))
    }
    pub fn read(&self, offset: u64, width: u8) -> Result<u64> {
        if offset >= 0x100 {
            if !matches!(width, 1 | 2 | 4 | 8) {
                return Err("invalid virtio configuration access width".into());
            }
            if offset
                .checked_add(width as u64)
                .is_none_or(|end| end > 0x10000)
            {
                return Err("virtio configuration access outside device window".into());
            }
            let mut value = 0u64;
            for byte in 0..width {
                value |= (self.config_byte(offset - 0x100 + byte as u64) as u64) << (byte * 8);
            }
            return Ok(value);
        }
        if width != 4 || !offset.is_multiple_of(4) {
            return Err("virtio transport requires aligned 32-bit access".into());
        }
        let queue = self.transport.queues.get(self.transport.queue_sel as usize);
        Ok(match offset {
            0 => 0x7472_6976,
            4 => 2,
            8 => match self.model {
                Model::Net { .. } => 1,
                Model::Block(_) => 2,
                Model::Console { .. } => 3,
            },
            12 => 0x5345_4544,
            0x10 => match self.transport.dev_sel {
                0 => match self.model {
                    Model::Net { .. } => 1 << 5,
                    Model::Block(_) => 1 << 9,
                    Model::Console { .. } => 2,
                },
                1 => 1,
                _ => 0,
            },
            0x34 => {
                if queue.is_some() {
                    QUEUE_SIZE as u64
                } else {
                    0
                }
            }
            0x44 => queue.is_some_and(|q| q.ready) as u64,
            0x60 => self.transport.isr as u64,
            0x70 => self.transport.status as u64,
            _ => 0,
        })
    }
    fn config_byte(&self, offset: u64) -> u8 {
        match &self.model {
            Model::Net { mac, .. } => mac.get(offset as usize).copied().unwrap_or(0),
            Model::Console { .. } => {
                if offset == 4 {
                    1
                } else {
                    0
                }
            }
            Model::Block(disk) => disk
                .capacity()
                .to_le_bytes()
                .get(offset as usize)
                .copied()
                .unwrap_or(0),
        }
    }
    pub fn write(
        &mut self,
        memory: &mut [u8],
        offset: u64,
        width: u8,
        value: u64,
    ) -> Result<Vec<Event>> {
        if width != 4 || !offset.is_multiple_of(4) || value > u32::MAX as u64 {
            return Err("virtio transport requires aligned 32-bit writes".into());
        }
        let value = value as u32;
        match offset {
            0x14 => self.transport.dev_sel = value,
            0x20 => {
                if self.transport.drv_sel < 2 {
                    self.transport.features[self.transport.drv_sel as usize] = value;
                }
            }
            0x24 => self.transport.drv_sel = value,
            0x30 => self.transport.queue_sel = value,
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 => {
                let q = self
                    .transport
                    .queues
                    .get_mut(self.transport.queue_sel as usize)
                    .ok_or("virtio queue selection out of range")?;
                match offset {
                    0x38 => {
                        if value == 0 || value > QUEUE_SIZE as u32 || !value.is_power_of_two() {
                            return Err("invalid virtio queue size".into());
                        }
                        q.num = value as u16;
                    }
                    0x44 => {
                        if value > 1 {
                            return Err("invalid virtio queue ready flag".into());
                        }
                        q.ready = value == 1;
                        q.last_avail = 0;
                        q.used_idx = 0;
                    }
                    0x80 => q.desc = (q.desc & !0xffff_ffff) | value as u64,
                    0x84 => q.desc = (q.desc & 0xffff_ffff) | ((value as u64) << 32),
                    0x90 => q.avail = (q.avail & !0xffff_ffff) | value as u64,
                    0x94 => q.avail = (q.avail & 0xffff_ffff) | ((value as u64) << 32),
                    0xa0 => q.used = (q.used & !0xffff_ffff) | value as u64,
                    0xa4 => q.used = (q.used & 0xffff_ffff) | ((value as u64) << 32),
                    _ => unreachable!(),
                }
            }
            0x50 => return self.notify(memory, value as usize),
            0x64 => self.transport.isr &= !value,
            0x70 => {
                self.transport.status = value;
                if value == 0 {
                    let old = std::mem::replace(self, Self::console());
                    *self = match old.model {
                        Model::Net { mac, .. } => Self::net(mac),
                        Model::Console { .. } => Self::console(),
                        Model::Block(disk) => Self::block(disk),
                    };
                }
            }
            _ => {}
        }
        Ok(Vec::new())
    }
    fn notify(&mut self, memory: &mut [u8], index: usize) -> Result<Vec<Event>> {
        if index >= self.transport.queues.len() {
            return Err("virtio notification queue out of range".into());
        }
        let mut events = Vec::new();
        if matches!(self.model, Model::Net { .. }) && index == 0
            || matches!(self.model, Model::Console { .. }) && matches!(index, 0 | 2)
        {
            self.drain(memory)?;
            return Ok(events);
        }
        while let Some(chain) = self.transport.queues[index].chain(memory)? {
            let written = match &mut self.model {
                Model::Net { .. } => {
                    let bytes = chain.read(memory)?;
                    if bytes.len() < NET_HEADER {
                        return Err("short virtio network header".into());
                    }
                    if bytes[..10].iter().any(|b| *b != 0) {
                        return Err("unnegotiated virtio network offload".into());
                    }
                    if bytes.len() > NET_HEADER {
                        events.push(Event::Frame(bytes[NET_HEADER..].to_vec()));
                    }
                    0
                }
                Model::Console {
                    device_ready,
                    port_ready,
                    guest_open,
                    control_rx,
                    tx_bytes,
                    ..
                } => {
                    let bytes = chain.read(memory)?;
                    if bytes.len() > 65536 {
                        return Err("console transmit chain too large".into());
                    }
                    if index == 1 {
                        *tx_bytes = tx_bytes
                            .checked_add(bytes.len() as u64)
                            .ok_or("control transmit byte counter overflow")?;
                        events.push(Event::Control(bytes));
                    } else {
                        if bytes.len() != 8 {
                            return Err("invalid console control packet length".into());
                        }
                        let id = u32_at(&bytes, 0);
                        let event = u16_at(&bytes, 4);
                        let value = u16_at(&bytes, 6);
                        let packet = |event: u16, value: u16, data: &[u8]| {
                            let mut b = 0u32.to_le_bytes().to_vec();
                            b.extend(event.to_le_bytes());
                            b.extend(value.to_le_bytes());
                            b.extend(data);
                            b
                        };
                        match (id, event, value) {
                            (u32::MAX, 0, 1) if !*device_ready => {
                                *device_ready = true;
                                control_rx.push_back(packet(1, 1, &[]));
                            }
                            (0, 3, 1) if *device_ready && !*port_ready => {
                                *port_ready = true;
                                control_rx.push_back(packet(7, 1, b"seedbox"));
                                control_rx.push_back(packet(6, 1, &[]));
                            }
                            (0, 6, 0 | 1) if *port_ready => *guest_open = value == 1,
                            _ => return Err("invalid console control handshake".into()),
                        }
                        if control_rx.len() > 16 {
                            return Err("console control backlog full".into());
                        }
                    }
                    0
                }
                Model::Block(disk) => block_request(disk, memory, &chain)?,
            };
            self.transport.queues[index].complete(memory, &chain, written);
            self.transport.isr |= 1;
        }
        self.drain(memory)?;
        Ok(events)
    }
    fn drain(&mut self, memory: &mut [u8]) -> Result<()> {
        match &mut self.model {
            Model::Net { backlog, .. } => {
                while let Some(bytes) = backlog.front() {
                    let Some(chain) = self.transport.queues[0].chain(memory)? else {
                        break;
                    };
                    let mut packet = vec![0; NET_HEADER];
                    packet[10] = 1;
                    packet.extend(bytes);
                    chain.write(memory, &packet)?;
                    self.transport.queues[0].complete(memory, &chain, packet.len());
                    backlog.pop_front();
                    self.transport.isr |= 1;
                }
            }
            Model::Console {
                rx,
                control_rx,
                rx_dma_bytes,
                ..
            } => {
                while let Some(bytes) = control_rx.front() {
                    let Some(chain) = self.transport.queues[2].chain(memory)? else {
                        break;
                    };
                    chain.write(memory, bytes)?;
                    self.transport.queues[2].complete(memory, &chain, bytes.len());
                    control_rx.pop_front();
                    self.transport.isr |= 1;
                }
                while !rx.is_empty() {
                    let Some(chain) = self.transport.queues[0].chain(memory)? else {
                        break;
                    };
                    let count = rx.len().min(chain.len);
                    let bytes = rx.iter().take(count).copied().collect::<Vec<_>>();
                    chain.write(memory, &bytes)?;
                    self.transport.queues[0].complete(memory, &chain, count);
                    *rx_dma_bytes = rx_dma_bytes
                        .checked_add(count as u64)
                        .ok_or("control DMA byte counter overflow")?;
                    rx.drain(..count);
                    self.transport.isr |= 1;
                }
            }
            Model::Block(_) => {}
        }
        Ok(())
    }
    pub fn receive(&mut self, memory: &mut [u8], bytes: &[u8]) -> Result<bool> {
        match &mut self.model {
            Model::Net { backlog, .. } => {
                if !self.transport.queues[0].ready || backlog.len() >= 256 {
                    return Ok(false);
                }
                if bytes.len() > 65536 {
                    return Err("network frame too large".into());
                }
                backlog.push_back(bytes.to_vec());
            }
            Model::Console { rx, .. } => {
                if rx.len().checked_add(bytes.len()).is_none_or(|n| n > 65536) {
                    return Err("console receive backlog full".into());
                }
                rx.extend(bytes);
            }
            Model::Block(_) => return Err("block device cannot receive a frame".into()),
        }
        self.drain(memory)?;
        Ok(true)
    }
    pub fn disk(&self) -> Option<&Disk> {
        match &self.model {
            Model::Block(disk) => Some(disk),
            _ => None,
        }
    }
    pub fn control_channel(&self) -> Option<super::ControlChannel> {
        match &self.model {
            Model::Console {
                tx_bytes,
                rx_dma_bytes,
                ..
            } => Some(super::ControlChannel {
                transport: "virtio-console-v1".into(),
                tx_bytes: *tx_bytes,
                rx_dma_bytes: *rx_dma_bytes,
            }),
            _ => None,
        }
    }
}

fn block_request(disk: &mut Disk, memory: &mut [u8], chain: &Chain) -> Result<usize> {
    if chain.segments.len() < 2 {
        return Err("short block descriptor chain".into());
    }
    let header = &chain.segments[0];
    let status = chain.segments.last().expect("validated chain length");
    if header.writable || header.len != 16 || !status.writable || status.len != 1 {
        return Err("invalid block header/status descriptors".into());
    }
    let kind = u32_at(memory, header.offset);
    let sector = u64_at(memory, header.offset + 8);
    let data = &chain.segments[1..chain.segments.len() - 1];
    let len = data.iter().map(|s| s.len).sum::<usize>();
    let mut written = 0;
    let result = match kind {
        0 | 1 => {
            if !len.is_multiple_of(512) || data.iter().any(|s| s.writable != (kind == 0)) {
                return Err("invalid block data descriptors".into());
            }
            let mut bytes = vec![0; len];
            if kind == 1 {
                let mut at = 0;
                for s in data {
                    bytes[at..at + s.len].copy_from_slice(&memory[s.offset..s.offset + s.len]);
                    at += s.len;
                }
            }
            let result = if kind == 0 {
                disk.read(sector, &mut bytes)
            } else {
                disk.write(sector, &bytes)
            };
            if result.is_ok() && kind == 0 {
                let mut at = 0;
                for s in data {
                    memory[s.offset..s.offset + s.len].copy_from_slice(&bytes[at..at + s.len]);
                    at += s.len;
                }
                written = len;
            }
            if result.is_ok() {
                0
            } else {
                1
            }
        }
        4 => {
            if !data.is_empty() {
                return Err("block flush includes data descriptors".into());
            }
            disk.flush();
            0
        }
        8 => {
            if data.iter().any(|s| !s.writable) {
                return Err("block ID buffer is not writable".into());
            }
            let id = b"seedbox-disk\0\0\0\0\0\0\0\0";
            let mut at = 0;
            for s in data {
                let n = s.len.min(id.len() - at);
                memory[s.offset..s.offset + n].copy_from_slice(&id[at..at + n]);
                at += n;
                if at == id.len() {
                    break;
                }
            }
            written = at;
            0
        }
        _ => 2,
    };
    memory[status.offset] = result;
    Ok(written + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    const DESC: usize = 0;
    const AVAIL: usize = 0x1000;
    const USED: usize = 0x1100;
    const DATA: usize = 0x2000;
    fn wr16(m: &mut [u8], at: usize, n: u16) {
        m[at..at + 2].copy_from_slice(&n.to_le_bytes());
    }
    fn wr32(m: &mut [u8], at: usize, n: u32) {
        m[at..at + 4].copy_from_slice(&n.to_le_bytes());
    }
    fn wr64(m: &mut [u8], at: usize, n: u64) {
        m[at..at + 8].copy_from_slice(&n.to_le_bytes());
    }
    fn descriptor(m: &mut [u8], index: usize, data: usize, len: usize, flags: u16, next: u16) {
        let at = DESC + index * 16;
        wr64(m, at, RAM_BASE + data as u64);
        wr32(m, at + 8, len as u32);
        wr16(m, at + 12, flags);
        wr16(m, at + 14, next);
    }
    fn setup(device: &mut Virtio, memory: &mut [u8], queue: u32) {
        for (offset, value) in [
            (0x30, queue),
            (0x38, 8),
            (0x80, RAM_BASE as u32 + DESC as u32),
            (0x90, RAM_BASE as u32 + AVAIL as u32),
            (0xa0, RAM_BASE as u32 + USED as u32),
            (0x44, 1),
        ] {
            device.write(memory, offset, 4, value as u64).unwrap();
        }
        wr16(memory, AVAIL + 2, 1);
    }
    #[test]
    fn console_counts_payload_transmission_and_only_completed_receive_dma() {
        let mut tx = vec![0; 0x4000];
        let mut device = Virtio::console();
        descriptor(&mut tx, 0, DATA, 5, 0, 0);
        tx[DATA..DATA + 5].copy_from_slice(b"hello");
        setup(&mut device, &mut tx, 1);
        assert_eq!(
            device.write(&mut tx, 0x50, 4, 1).unwrap(),
            vec![Event::Control(b"hello".to_vec())]
        );
        let mut rx = vec![0; 0x4000];
        descriptor(&mut rx, 0, DATA, 3, 2, 0);
        setup(&mut device, &mut rx, 0);
        wr16(&mut rx, AVAIL + 2, 0);
        device.receive(&mut rx, b"reply").unwrap();
        assert_eq!(device.control_channel().unwrap().tx_bytes, 5);
        assert_eq!(device.control_channel().unwrap().rx_dma_bytes, 0);
        wr16(&mut rx, AVAIL + 2, 1);
        device.write(&mut rx, 0x50, 4, 0).unwrap();
        assert_eq!(&rx[DATA..DATA + 3], b"rep");
        assert_eq!(device.control_channel().unwrap().rx_dma_bytes, 3);
        descriptor(&mut rx, 1, DATA + 16, 10, 2, 0);
        wr16(&mut rx, AVAIL + 6, 1);
        wr16(&mut rx, AVAIL + 2, 2);
        device.write(&mut rx, 0x50, 4, 0).unwrap();
        assert_eq!(&rx[DATA + 16..DATA + 18], b"ly");
        assert_eq!(device.control_channel().unwrap().rx_dma_bytes, 5);
    }
    #[test]
    fn arm_physical_transmit_addresses_and_used_lengths_are_correct() {
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::net([1, 2, 3, 4, 5, 6]);
        let packet = b"ethernet payload";
        descriptor(&mut m, 0, DATA, NET_HEADER + packet.len(), 0, 0);
        m[DATA + NET_HEADER..DATA + NET_HEADER + packet.len()].copy_from_slice(packet);
        setup(&mut device, &mut m, 1);
        assert_eq!(
            device.write(&mut m, 0x50, 4, 1).unwrap(),
            vec![Event::Frame(packet.to_vec())]
        );
        assert_eq!(u16_at(&m, USED + 2), 1);
        assert_eq!(u32_at(&m, USED + 8), 0);
        assert!(device.irq());
        device.write(&mut m, 0x64, 4, 1).unwrap();
        assert!(!device.irq());
    }
    #[test]
    fn receive_backlog_acceptance_is_separate_from_interrupt_notification() {
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::net([0; 6]);
        descriptor(&mut m, 0, DATA, 1024, 2, 0);
        setup(&mut device, &mut m, 0);
        wr16(&mut m, AVAIL + 2, 0);
        assert!(device.receive(&mut m, b"frame").unwrap());
        assert!(!device.irq());
        wr16(&mut m, AVAIL + 2, 1);
        device.write(&mut m, 0x50, 4, 0).unwrap();
        assert!(device.irq());
        assert_eq!(&m[DATA + NET_HEADER..DATA + NET_HEADER + 5], b"frame");
        assert_eq!(u32_at(&m, USED + 8), 17);
    }
    #[test]
    fn dma_cannot_access_mmio_or_follow_cycles_and_failure_does_not_write_ram() {
        for address in [crate::TIME_ADDR, RAM_BASE - 1, RAM_BASE + 0x3fff, u64::MAX] {
            let mut m = vec![0; 0x4000];
            let mut device = Virtio::net([0; 6]);
            descriptor(&mut m, 0, DATA, 32, 0, 0);
            wr64(&mut m, 0, address);
            setup(&mut device, &mut m, 1);
            let before = m.clone();
            assert!(device.write(&mut m, 0x50, 4, 1).is_err());
            assert_eq!(m, before);
        }
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::net([0; 6]);
        descriptor(&mut m, 0, DATA, 32, 1, 0);
        setup(&mut device, &mut m, 1);
        let before = m.clone();
        assert!(device
            .write(&mut m, 0x50, 4, 1)
            .unwrap_err()
            .contains("cyclic"));
        assert_eq!(m, before);
    }
    #[test]
    fn available_overrun_and_wrong_receive_direction_fail_before_dma() {
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::net([0; 6]);
        descriptor(&mut m, 0, DATA, 32, 0, 0);
        setup(&mut device, &mut m, 0);
        let before = m.clone();
        assert!(device.receive(&mut m, b"frame").is_err());
        assert_eq!(m, before);
        wr16(&mut m, AVAIL + 2, 9);
        let before = m.clone();
        assert!(device
            .write(&mut m, 0x50, 4, 0)
            .unwrap_err()
            .contains("overrun"));
        assert_eq!(m, before);
    }
    #[test]
    fn block_write_flush_read_and_crash_use_the_existing_disk_policy() {
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::block(Disk::new(None, Some(8)).unwrap());
        descriptor(&mut m, 0, DATA, 16, 1, 1);
        descriptor(&mut m, 1, DATA + 512, 512, 1, 2);
        descriptor(&mut m, 2, DATA + 1024, 1, 2, 0);
        wr32(&mut m, DATA, 1);
        wr64(&mut m, DATA + 8, 1);
        m[DATA + 512..DATA + 1024].fill(0xab);
        setup(&mut device, &mut m, 0);
        device.write(&mut m, 0x50, 4, 0).unwrap();
        assert_eq!(m[DATA + 1024], 0);
        assert_eq!(device.disk().unwrap().volatile_sectors(), 1);
        let (before_flush, lost) = device.disk().unwrap().crashed();
        assert_eq!(lost, 1);
        let mut bytes = [0; 512];
        before_flush.read(1, &mut bytes).unwrap();
        assert_eq!(bytes, [0; 512]);
        wr32(&mut m, DATA, 4);
        wr16(&mut m, DESC + 14, 2);
        wr16(&mut m, AVAIL + 2, 2);
        device.write(&mut m, 0x50, 4, 0).unwrap();
        assert_eq!(device.disk().unwrap().stable_sectors(), 1);
        let (after_flush, lost) = device.disk().unwrap().crashed();
        assert_eq!(lost, 0);
        after_flush.read(1, &mut bytes).unwrap();
        assert_eq!(bytes, [0xab; 512]);
        wr32(&mut m, DATA, 0);
        wr16(&mut m, DESC + 14, 1);
        wr16(&mut m, DESC + 16 + 12, 3);
        m[DATA + 512..DATA + 1024].fill(0);
        wr16(&mut m, AVAIL + 2, 3);
        device.write(&mut m, 0x50, 4, 0).unwrap();
        assert_eq!(&m[DATA + 512..DATA + 1024], &[0xab; 512]);
        assert_eq!(u32_at(&m, USED + 4 + 2 * 8 + 4), 513);
    }
    #[test]
    fn malformed_block_chain_does_not_change_the_disk_or_used_ring() {
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::block(Disk::new(None, Some(8)).unwrap());
        descriptor(&mut m, 0, DATA, 16, 1, 1);
        descriptor(&mut m, 1, DATA + 512, 512, 1, 2);
        descriptor(&mut m, 2, DATA + 1024, 1, 0, 0);
        wr32(&mut m, DATA, 1);
        setup(&mut device, &mut m, 0);
        let before = m.clone();
        let disk = device.disk().unwrap().digest();
        assert!(device.write(&mut m, 0x50, 4, 0).is_err());
        assert_eq!(m, before);
        assert_eq!(device.disk().unwrap().digest(), disk);
    }
}
