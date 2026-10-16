//! Modern virtio MMIO and bounded split queues over ARM guest physical RAM.
//! Every DMA span and descriptor chain is checked before a device effect.
use super::Event;
use crate::{disk::Disk, Result, RAM_BASE};
use seedbox_engine_core::virtio_block::{Block, Policy as BlockPolicy};
use seedbox_engine_core::virtio_console::{Console, Policy as ConsolePolicy};
use seedbox_engine_core::virtio_network::{Network, Policy as NetworkPolicy};

#[cfg(test)]
fn u16_at(m: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(m[offset..offset + 2].try_into().expect("validated DMA"))
}
#[cfg(test)]
fn u32_at(m: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(m[offset..offset + 4].try_into().expect("validated DMA"))
}

use seedbox_engine_core::virtio_mmio::{
    Effect as MmioEffect, Policy as MmioPolicy, Spec, Transport,
};
#[derive(Clone)]
pub enum Model {
    Net(Network),
    Console(Console),
    Block(Block),
}
#[derive(Clone)]
pub struct Virtio {
    transport: Transport,
    model: Model,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    transport: seedbox_engine_core::virtio_mmio::Snapshot,
    model: ModelSnapshot,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(
    tag = "kind",
    content = "state",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum ModelSnapshot {
    Net(seedbox_engine_core::virtio_network::Snapshot),
    Console(seedbox_engine_core::virtio_console::Snapshot),
    Block(seedbox_engine_core::virtio_block::Snapshot),
}

impl Virtio {
    /// Disk sectors are streamed separately through `Disk::write_checkpoint`.
    pub fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            transport: self.transport.snapshot(),
            model: match &self.model {
                Model::Net(net) => ModelSnapshot::Net(net.snapshot()),
                Model::Console(console) => ModelSnapshot::Console(console.snapshot()),
                Model::Block(block) => ModelSnapshot::Block(block.snapshot()?),
            },
        })
    }

    /// Restore into a configured device. A block device must already contain
    /// the restored disk; its digest binds the separate sectors to this metadata.
    pub fn restore_snapshot(self, snapshot: Snapshot) -> Result<Self> {
        let transport = self.transport.restore_snapshot(snapshot.transport)?;
        let model = match (self.model, snapshot.model) {
            (Model::Net(net), ModelSnapshot::Net(saved)) => {
                Model::Net(net.restore_snapshot(saved)?)
            }
            (Model::Console(console), ModelSnapshot::Console(saved)) => {
                Model::Console(console.restore_snapshot(saved)?)
            }
            (Model::Block(block), ModelSnapshot::Block(saved)) => {
                Model::Block(block.restore_snapshot(saved)?)
            }
            _ => return Err("virtio checkpoint device kind mismatch".into()),
        };
        Ok(Self { transport, model })
    }

    pub fn net(mac: [u8; 6]) -> Self {
        Self::new(Model::Net(Network::new(mac, NetworkPolicy::Application)), 2)
    }
    pub fn console() -> Self {
        Self::new(Model::Console(Console::new(ConsolePolicy::Application)), 4)
    }
    pub fn block(disk: Disk) -> Self {
        Self::new(Model::Block(Block::new(disk, BlockPolicy::Application)), 1)
    }
    fn new(model: Model, queues: usize) -> Self {
        let spec = match &model {
            Model::Net(_) => Spec {
                device_id: 1,
                features: [1 << 5, 1],
            },
            Model::Block(_) => Spec {
                device_id: 2,
                features: [1 << 9, 1],
            },
            Model::Console(_) => Spec {
                device_id: 3,
                features: [2, 1],
            },
        };
        Self {
            model,
            transport: Transport::new(spec, queues, MmioPolicy::Application),
        }
    }
    pub fn irq(&self) -> bool {
        self.transport.isr != 0
    }
    /// Versioned application state includes transport cursors, pending ingress
    /// and durable/volatile disk layers, not merely bytes visible to the guest.
    pub fn state_digest(&self) -> Result<String> {
        let model = match &self.model {
            Model::Net(net) => net.projection(),
            Model::Console(console) => console.projection(),
            Model::Block(disk) if disk.next_due().is_some() => serde_json::json!({
                "kind": "block",
                "disk": disk.disk().digest(),
                "pending_due": disk.pending_due(),
            }),
            Model::Block(disk) => serde_json::json!({"kind":"block","disk":disk.disk().digest()}),
        };
        let bytes =
            serde_json::to_vec(&(self.transport.projection(), model)).map_err(|e| e.to_string())?;
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
        Ok(self.transport.read(offset) as u64)
    }
    fn config_byte(&self, offset: u64) -> u8 {
        match &self.model {
            Model::Net(net) => net.mac().get(offset as usize).copied().unwrap_or(0),
            Model::Console(_) => {
                if offset == 4 {
                    1
                } else {
                    0
                }
            }
            Model::Block(disk) => disk
                .disk()
                .capacity()
                .to_le_bytes()
                .get(offset as usize)
                .copied()
                .unwrap_or(0),
        }
    }
    /// `now` is the guest time of the write: when a delayed block request
    /// becomes due.
    pub fn write(
        &mut self,
        memory: &mut [u8],
        offset: u64,
        width: u8,
        value: u64,
        now: u64,
    ) -> Result<Vec<Event>> {
        if width != 4 || !offset.is_multiple_of(4) || value > u32::MAX as u64 {
            return Err("virtio transport requires aligned 32-bit writes".into());
        }
        match self.transport.write(offset, value as u32)? {
            MmioEffect::None => (),
            MmioEffect::Notify(index) => return self.notify(memory, index, now),
            MmioEffect::Reset => {
                let old = std::mem::replace(self, Self::console());
                *self = match old.model {
                    Model::Net(net) => Self::net(net.mac()),
                    Model::Console(_) => Self::console(),
                    // Injected disk errors are a world fault, not device state:
                    // they outlive a guest reset like the disk itself.
                    // Pending requests go with the old device, unanswered.
                    Model::Block(mut block) => {
                        let errors = block.take_errors();
                        let latency = block.take_latency();
                        let hold = block.take_hold();
                        let mut device = Self::block(block.into_disk());
                        if let Model::Block(rebuilt) = &mut device.model {
                            rebuilt.set_errors(errors);
                            rebuilt.set_latency(latency);
                            rebuilt.set_hold(hold)?;
                        }
                        device
                    }
                };
            }
        }
        Ok(Vec::new())
    }
    fn notify(&mut self, memory: &mut [u8], index: usize, now: u64) -> Result<Vec<Event>> {
        if index >= self.transport.queues.len() {
            return Err("virtio notification queue out of range".into());
        }
        if let Model::Console(console) = &mut self.model {
            let output = console.notify(&mut self.transport, memory, RAM_BASE, index, |_, _| {})?;
            return Ok(output.payloads.into_iter().map(Event::Control).collect());
        }
        if let Model::Block(block) = &mut self.model {
            let notified = block.notify(&mut self.transport, memory, RAM_BASE, now, |_, _| {})?;
            return Ok(notified
                .held
                .map(|trigger| Event::Held { trigger })
                .into_iter()
                .collect());
        }
        if let Model::Net(net) = &mut self.model {
            let output = net.notify(&mut self.transport, memory, RAM_BASE, index, |_, _| {})?;
            return Ok(output.frames.into_iter().map(Event::Frame).collect());
        }
        unreachable!("all device models handled")
    }
    /// Install or remove a block request hold.
    pub fn set_hold(
        &mut self,
        hold: Option<seedbox_engine_core::virtio_block::Hold>,
    ) -> Result<()> {
        match &mut self.model {
            Model::Block(block) => block.set_hold(hold),
            _ => Err("a block request hold needs a block device".into()),
        }
    }
    /// The trigger whose hold stopped this block device, if any.
    pub fn held(&self) -> Option<u32> {
        match &self.model {
            Model::Block(block) => block.held(),
            _ => None,
        }
    }
    /// Complete a block device's delayed requests due by `now`; whether any did.
    pub fn complete_due(&mut self, memory: &mut [u8], now: u64) -> Result<bool> {
        match &mut self.model {
            Model::Block(block) => Ok(block
                .complete_due(&mut self.transport, memory, now, |_, _| {})?
                .irq),
            _ => Ok(false),
        }
    }
    /// When a block device's earliest delayed request is due.
    pub fn next_due(&self) -> Option<u64> {
        match &self.model {
            Model::Block(block) => block.next_due(),
            _ => None,
        }
    }
    /// Start or end injected latency on a block device; ending returns counts.
    pub fn set_disk_latency(
        &mut self,
        latency: Option<seedbox_engine_core::virtio_block::InjectedLatency>,
    ) -> Result<Option<seedbox_engine_core::virtio_block::LatencyCounts>> {
        match &mut self.model {
            Model::Block(block) => Ok(block.set_latency(latency)),
            _ => Err("disk latency needs a block device".into()),
        }
    }
    /// Start or end injected errors on a block device; ending returns counts.
    pub fn set_disk_errors(
        &mut self,
        errors: Option<seedbox_engine_core::virtio_block::InjectedErrors>,
    ) -> Result<Option<seedbox_engine_core::virtio_block::ErrorCounts>> {
        match &mut self.model {
            Model::Block(block) => Ok(block.set_errors(errors)),
            _ => Err("disk errors need a block device".into()),
        }
    }
    pub fn receive(&mut self, memory: &mut [u8], bytes: &[u8]) -> Result<bool> {
        match &mut self.model {
            Model::Net(net) => Ok(net
                .receive(&mut self.transport, memory, RAM_BASE, bytes, |_, _| {})?
                .accepted),
            Model::Console(console) => {
                console.receive(&mut self.transport, memory, RAM_BASE, bytes, |_, _| {})?;
                Ok(true)
            }
            Model::Block(_) => Err("block device cannot receive a frame".into()),
        }
    }
    pub fn disk(&self) -> Option<&Disk> {
        match &self.model {
            Model::Block(disk) => Some(disk.disk()),
            _ => None,
        }
    }
    pub fn control_channel(&self) -> Option<super::ControlChannel> {
        match &self.model {
            Model::Console(console) => {
                let (tx_bytes, rx_dma_bytes) = console.counters();
                Some(super::ControlChannel {
                    transport: "virtio-console-v1".into(),
                    tx_bytes,
                    rx_dma_bytes,
                })
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const NET_HEADER: usize = 12;
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
        device.transport.status = 15;
        for (offset, value) in [
            (0x30, queue),
            (0x38, 8),
            (0x80, RAM_BASE as u32 + DESC as u32),
            (0x90, RAM_BASE as u32 + AVAIL as u32),
            (0xa0, RAM_BASE as u32 + USED as u32),
            (0x44, 1),
        ] {
            device.write(memory, offset, 4, value as u64, 0).unwrap();
        }
        wr16(memory, AVAIL + 2, 1);
    }
    #[test]
    fn console_counts_payload_transmission_and_only_completed_receive_dma() {
        let mut tx = vec![0; 0x4000];
        let mut device = Virtio::console();
        // Drive the real multiport handshake before data admission.
        let mut control = vec![0; 0x4000];
        setup(&mut device, &mut control, 3);
        for (i, (id, event)) in [(u32::MAX, 0u16), (0, 3), (0, 6)].into_iter().enumerate() {
            descriptor(&mut control, 0, DATA, 8, 0, 0);
            wr32(&mut control, DATA, id);
            wr16(&mut control, DATA + 4, event);
            wr16(&mut control, DATA + 6, 1);
            wr16(&mut control, AVAIL + 2, i as u16 + 1);
            device.write(&mut control, 0x50, 4, 3, 0).unwrap();
        }
        device.transport.queues[3].ready = false;
        descriptor(&mut tx, 0, DATA, 5, 0, 0);
        tx[DATA..DATA + 5].copy_from_slice(b"hello");
        setup(&mut device, &mut tx, 1);
        assert_eq!(
            device.write(&mut tx, 0x50, 4, 1, 0).unwrap(),
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
        device.write(&mut rx, 0x50, 4, 0, 0).unwrap();
        assert_eq!(&rx[DATA..DATA + 3], b"rep");
        assert_eq!(device.control_channel().unwrap().rx_dma_bytes, 3);
        descriptor(&mut rx, 1, DATA + 16, 10, 2, 0);
        wr16(&mut rx, AVAIL + 6, 1);
        wr16(&mut rx, AVAIL + 2, 2);
        device.write(&mut rx, 0x50, 4, 0, 0).unwrap();
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
            device.write(&mut m, 0x50, 4, 1, 0).unwrap(),
            vec![Event::Frame(packet.to_vec())]
        );
        assert_eq!(u16_at(&m, USED + 2), 1);
        assert_eq!(u32_at(&m, USED + 8), 0);
        assert!(device.irq());
        device.write(&mut m, 0x64, 4, 1, 0).unwrap();
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
        device.write(&mut m, 0x50, 4, 0, 0).unwrap();
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
            assert!(device.write(&mut m, 0x50, 4, 1, 0).is_err());
            assert_eq!(m, before);
        }
        let mut m = vec![0; 0x4000];
        let mut device = Virtio::net([0; 6]);
        descriptor(&mut m, 0, DATA, 32, 1, 0);
        setup(&mut device, &mut m, 1);
        let before = m.clone();
        assert!(device
            .write(&mut m, 0x50, 4, 1, 0)
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
            .write(&mut m, 0x50, 4, 0, 0)
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
        device.write(&mut m, 0x50, 4, 0, 0).unwrap();
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
        device.write(&mut m, 0x50, 4, 0, 0).unwrap();
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
        device.write(&mut m, 0x50, 4, 0, 0).unwrap();
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
        assert!(device.write(&mut m, 0x50, 4, 0, 0).is_err());
        assert_eq!(m, before);
        assert_eq!(device.disk().unwrap().digest(), disk);
    }
}
