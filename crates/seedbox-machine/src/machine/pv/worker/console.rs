//! Modern virtio MMIO console: a single named lifecycle port, no application
//! control personality. Split rings are checked in full before every DMA.
use super::Result;
use crate::pv::transport::LifecycleOutput;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
const BASE: u64 = 0x4000_0000;
const PORT: &[u8] = b"seedbox-lifecycle";
const MAX_PORTS: u64 = 0x104;
const EMERGENCY_WRITE: u64 = 0x108;
// Virtio 1.x MMIO register offsets and console control events. Queue ordering
// is fixed by the multiport console ABI: port RX/TX, then control RX/TX.
const MAGIC: u64 = 0x00;
const VERSION: u64 = 0x04;
const DEVICE_ID: u64 = 0x08;
const VENDOR_ID: u64 = 0x0c;
const DEVICE_FEATURES: u64 = 0x10;
const DEVICE_FEATURES_SEL: u64 = 0x14;
const DRIVER_FEATURES: u64 = 0x20;
const DRIVER_FEATURES_SEL: u64 = 0x24;
const QUEUE_SEL: u64 = 0x30;
const QUEUE_NUM_MAX: u64 = 0x34;
const QUEUE_NUM: u64 = 0x38;
const QUEUE_READY: u64 = 0x44;
const QUEUE_NOTIFY: u64 = 0x50;
const INTERRUPT_STATUS: u64 = 0x60;
const INTERRUPT_ACK: u64 = 0x64;
const STATUS: u64 = 0x70;
const QUEUE_DESC_LOW: u64 = 0x80;
const QUEUE_DESC_HIGH: u64 = 0x84;
const QUEUE_AVAIL_LOW: u64 = 0x90;
const QUEUE_AVAIL_HIGH: u64 = 0x94;
const QUEUE_USED_LOW: u64 = 0xa0;
const QUEUE_USED_HIGH: u64 = 0xa4;
const CONFIG_GENERATION: u64 = 0xfc;
const CONSOLE_SIZE: u64 = 0x100;
const FEATURE_MULTIPORT: u32 = 1 << 1;
const FEATURE_VERSION_1_HIGH: u32 = 1;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;
const USED_RING_INTERRUPT: u32 = 1;
const QUEUE_LIMIT: u32 = 256;
const PORT_RX: usize = 0;
const PORT_TX: usize = 1;
const CONTROL_RX: usize = 2;
const CONTROL_TX: usize = 3;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;
const DESCRIPTOR_NEXT: u16 = 1;
const DESCRIPTOR_WRITE: u16 = 2;
const AVAILABLE_NO_INTERRUPT: u16 = 1;
#[derive(Clone, Default, Serialize, Deserialize)]
struct Queue {
    size: u16,
    ready: bool,
    descriptor: u64,
    available: u64,
    used: u64,
    cursor: u16,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Console {
    queues: [Queue; 4],
    selector: u32,
    feature_selector: u32,
    driver_selector: u32,
    features: [u32; 2],
    status: u32,
    interrupt: u32,
    controls: VecDeque<Vec<u8>>,
    data: VecDeque<u8>,
    announced: bool,
    open: bool,
    pub(super) output: LifecycleOutput,
}
#[derive(Clone)]
struct Descriptor {
    address: usize,
    length: usize,
    writable: bool,
}
fn offset(ram: &[u8], address: u64, length: usize) -> Result<usize> {
    let at = address
        .checked_sub(BASE)
        .and_then(|x| usize::try_from(x).ok())
        .ok_or("virtio address outside RAM")?;
    if at.checked_add(length).is_none_or(|end| end > ram.len()) {
        return Err("virtio range outside RAM".into());
    }
    Ok(at)
}
fn u16_at(ram: &[u8], address: u64) -> Result<u16> {
    let at = offset(ram, address, 2)?;
    Ok(u16::from_le_bytes(ram[at..at + 2].try_into().unwrap()))
}
fn add(address: u64, bytes: usize) -> Result<u64> {
    address
        .checked_add(bytes as u64)
        .ok_or("virtio address overflow".into())
}
impl Queue {
    fn take(&self, ram: &[u8], writable: bool) -> Result<Option<(u16, Vec<Descriptor>)>> {
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
            if flags & !(DESCRIPTOR_NEXT | DESCRIPTOR_WRITE) != 0
                || (flags & DESCRIPTOR_WRITE != 0) != writable
            {
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
    fn finish(&mut self, ram: &mut [u8], head: u16, length: usize) -> Result<bool> {
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
impl Console {
    pub(super) fn irq(&self) -> bool {
        self.interrupt != 0
    }
    pub(super) fn input(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
    }
    pub(super) fn shutdown(&mut self) {
        self.input(b"shutdown\n");
    }
    fn control(event: u16, value: u16, bytes: &[u8]) -> Vec<u8> {
        let mut out = vec![0; 4];
        out.extend(event.to_le_bytes());
        out.extend(value.to_le_bytes());
        out.extend(bytes);
        out
    }
    pub(super) fn read(&self, offset: u64) -> Result<u32> {
        let q = self.queues.get(self.selector as usize);
        Ok(match offset {
            MAGIC => 0x74726976,
            VERSION => 2,
            DEVICE_ID => 3,
            VENDOR_ID => 0x534258,
            DEVICE_FEATURES => match self.feature_selector {
                0 => FEATURE_MULTIPORT,
                1 => FEATURE_VERSION_1_HIGH,
                _ => 0,
            },
            QUEUE_NUM_MAX => {
                if q.is_some() {
                    QUEUE_LIMIT
                } else {
                    0
                }
            }
            QUEUE_READY => q.is_some_and(|q| q.ready) as u32,
            INTERRUPT_STATUS => self.interrupt,
            STATUS => self.status,
            CONFIG_GENERATION => 0,
            CONSOLE_SIZE | EMERGENCY_WRITE => 0,
            MAX_PORTS => 1,
            _ => return Err(format!("virtio unsupported read {offset:#x}")),
        })
    }
    pub(super) fn write(
        &mut self,
        ram: &mut [u8],
        offset: u64,
        value: u32,
        now: u64,
    ) -> Result<()> {
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
                let q = self
                    .queues
                    .get_mut(self.selector as usize)
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
                    QUEUE_DESC_HIGH => {
                        q.descriptor = (q.descriptor & 0xffff_ffff) | ((value as u64) << 32)
                    }
                    QUEUE_AVAIL_LOW => q.available = (q.available & !0xffff_ffff) | value as u64,
                    QUEUE_AVAIL_HIGH => {
                        q.available = (q.available & 0xffff_ffff) | ((value as u64) << 32)
                    }
                    QUEUE_USED_LOW => q.used = (q.used & !0xffff_ffff) | value as u64,
                    QUEUE_USED_HIGH => q.used = (q.used & 0xffff_ffff) | ((value as u64) << 32),
                    _ => unreachable!(),
                }
            }
            QUEUE_NOTIFY => {
                if value >= 4 {
                    return Err("virtio notify invalid queue".into());
                }
            }
            INTERRUPT_ACK => self.interrupt &= !value,
            STATUS => {
                if value == 0 {
                    let data = std::mem::take(&mut self.data);
                    let output = std::mem::take(&mut self.output);
                    *self = Self::default();
                    self.data = data;
                    self.output = output;
                } else {
                    if value & STATUS_FEATURES_OK != 0
                        && (self.features[1] != FEATURE_VERSION_1_HIGH
                            || self.features[0] & !FEATURE_MULTIPORT != 0)
                    {
                        return Err("virtio unsupported features".into());
                    }
                    self.status = value;
                }
            }
            _ => return Err(format!("virtio unsupported write {offset:#x}")),
        }
        self.process(ram, now)
    }
    pub(super) fn process(&mut self, ram: &mut [u8], now: u64) -> Result<()> {
        if self.status & STATUS_DRIVER_OK == 0 {
            return Ok(());
        }
        for queue in [CONTROL_TX, PORT_TX] {
            while let Some((head, descriptors)) = self.queues[queue].take(ram, false)? {
                let mut bytes = Vec::new();
                for d in descriptors {
                    debug_assert!(!d.writable);
                    if bytes.len() + d.length > 65536 {
                        return Err("virtio console tx exceeds bound".into());
                    }
                    bytes.extend_from_slice(&ram[d.address..d.address + d.length]);
                }
                if queue == CONTROL_TX {
                    if bytes.len() != 8 {
                        return Err("virtio console control frame size".into());
                    }
                    let event = u16::from_le_bytes(bytes[4..6].try_into().unwrap());
                    let value = u16::from_le_bytes(bytes[6..8].try_into().unwrap());
                    match event {
                        DEVICE_READY if value == 1 => {
                            if !self.announced {
                                self.controls.push_back(Self::control(PORT_ADD, 1, &[]));
                                self.announced = true;
                            }
                        }
                        PORT_READY if value == 1 => {
                            self.controls.push_back(Self::control(PORT_NAME, 1, PORT));
                            self.controls.push_back(Self::control(PORT_OPEN, 1, &[]));
                        }
                        PORT_OPEN => self.open = value != 0,
                        _ => (),
                    }
                }
                if queue == PORT_TX {
                    self.output.push(now, bytes);
                }
                if self.queues[queue].finish(ram, head, 0)? {
                    self.interrupt |= USED_RING_INTERRUPT;
                }
            }
        }
        while let Some(bytes) = self.controls.front().cloned() {
            if !self.receive(ram, CONTROL_RX, &bytes)? {
                break;
            }
            self.controls.pop_front();
        }
        if self.open && !self.data.is_empty() {
            let bytes: Vec<_> = self.data.iter().copied().collect();
            let count = self.receive_data(ram, &bytes)?;
            self.data.drain(..count);
        }
        Ok(())
    }
    fn receive(&mut self, ram: &mut [u8], queue: usize, bytes: &[u8]) -> Result<bool> {
        let Some((head, descriptors)) = self.queues[queue].take(ram, true)? else {
            return Ok(false);
        };
        let capacity: usize = descriptors.iter().map(|d| d.length).sum();
        if capacity < bytes.len() {
            return Err("virtio control buffer too short".into());
        }
        let mut cursor = 0;
        for d in descriptors {
            let length = d.length.min(bytes.len() - cursor);
            ram[d.address..d.address + length].copy_from_slice(&bytes[cursor..cursor + length]);
            cursor += length;
        }
        if self.queues[queue].finish(ram, head, bytes.len())? {
            self.interrupt |= USED_RING_INTERRUPT;
        }
        Ok(true)
    }
    fn receive_data(&mut self, ram: &mut [u8], bytes: &[u8]) -> Result<usize> {
        let mut cursor = 0;
        while cursor < bytes.len() {
            let Some((head, descriptors)) = self.queues[PORT_RX].take(ram, true)? else {
                break;
            };
            let start = cursor;
            for d in descriptors {
                let length = d.length.min(bytes.len() - cursor);
                ram[d.address..d.address + length].copy_from_slice(&bytes[cursor..cursor + length]);
                cursor += length;
            }
            if cursor == start {
                return Err("virtio zero length receive chain".into());
            }
            if self.queues[PORT_RX].finish(ram, head, cursor - start)? {
                self.interrupt |= USED_RING_INTERRUPT;
            }
        }
        Ok(cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queue(ram: &mut [u8], writable: bool) -> Queue {
        ram[..8].copy_from_slice(&(BASE + 256).to_le_bytes());
        ram[8..12].copy_from_slice(&64u32.to_le_bytes());
        ram[12..14].copy_from_slice(&(if writable { 2u16 } else { 0 }).to_le_bytes());
        ram[66..68].copy_from_slice(&1u16.to_le_bytes());
        Queue {
            size: 8,
            ready: true,
            descriptor: BASE,
            available: BASE + 64,
            used: BASE + 128,
            cursor: 0,
        }
    }
    #[test]
    fn advertises_one_port_in_standard_config_field() {
        let console = Console::default();
        assert_eq!(console.read(MAX_PORTS).unwrap(), 1);
        assert_eq!(console.read(EMERGENCY_WRITE).unwrap(), 0);
    }
    #[test]
    fn pending_input_survives_snapshot_and_is_consumed_once() {
        let mut ram = vec![0; 512];
        let mut console = Console {
            status: STATUS_DRIVER_OK,
            open: true,
            ..Console::default()
        };
        console.shutdown();
        console.queues[0] = queue(&mut ram, true);
        let snapshot = serde_json::to_vec(&console).unwrap();
        let mut branch: Console = serde_json::from_slice(&snapshot).unwrap();
        branch.process(&mut ram, 0).unwrap();
        assert_eq!(&ram[256..265], b"shutdown\n");
        assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 1);
        assert!(branch.data.is_empty());
        branch.process(&mut ram, 0).unwrap();
        assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 1);
        assert!(!console.data.is_empty());
    }
    #[test]
    fn tx_is_timestamped_and_survives_snapshot_without_repeating_consumed_ring() {
        let mut ram = vec![0; 512];
        let mut console = Console {
            status: STATUS_DRIVER_OK,
            ..Console::default()
        };
        console.queues[PORT_TX] = queue(&mut ram, false);
        ram[8..12].copy_from_slice(&3u32.to_le_bytes());
        ram[256..259].copy_from_slice(b"raw");
        console.process(&mut ram, 17).unwrap();
        let image = serde_json::to_vec(&console).unwrap();
        let mut restored: Console = serde_json::from_slice(&image).unwrap();
        restored.process(&mut ram, 20).unwrap();
        let writes = restored.output.drain();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].time, 17);
        assert_eq!(writes[0].bytes, b"raw");
        assert!(restored.output.drain().is_empty());
        let image = serde_json::to_vec(&restored).unwrap();
        let mut restored: Console = serde_json::from_slice(&image).unwrap();
        restored.process(&mut ram, 30).unwrap();
        assert!(restored.output.drain().is_empty());
        assert_eq!(console.output.drain(), writes);
    }
    #[test]
    fn fragmented_input_preserves_backpressured_suffix_across_snapshot() {
        let mut ram = vec![0; 512];
        let mut console = Console {
            status: STATUS_DRIVER_OK,
            open: true,
            ..Console::default()
        };
        console.queues[PORT_RX] = queue(&mut ram, true);
        ram[8..12].copy_from_slice(&3u32.to_le_bytes());
        console.input(b"ab");
        console.input(b"cde");
        console.process(&mut ram, 11).unwrap();
        assert_eq!(&ram[256..259], b"abc");
        assert_eq!(console.data.iter().copied().collect::<Vec<_>>(), b"de");
        let image = serde_json::to_vec(&console).unwrap();
        let mut restored: Console = serde_json::from_slice(&image).unwrap();
        restored.process(&mut ram, 12).unwrap();
        assert_eq!(restored.data.len(), 2);
        restored.input(b"fg");
        ram[16..24].copy_from_slice(&(BASE + 272).to_le_bytes());
        ram[24..28].copy_from_slice(&4u32.to_le_bytes());
        ram[28..30].copy_from_slice(&DESCRIPTOR_WRITE.to_le_bytes());
        ram[66..68].copy_from_slice(&2u16.to_le_bytes());
        ram[70..72].copy_from_slice(&1u16.to_le_bytes());
        restored.process(&mut ram, 13).unwrap();
        assert_eq!(&ram[272..276], b"defg");
        assert!(restored.data.is_empty());
        let image = serde_json::to_vec(&restored).unwrap();
        let mut restored: Console = serde_json::from_slice(&image).unwrap();
        restored.process(&mut ram, 14).unwrap();
        assert!(restored.data.is_empty());
        assert_eq!(u16_at(&ram, BASE + 130).unwrap(), 2);
        assert_eq!(console.data.iter().copied().collect::<Vec<_>>(), b"de");
    }
    #[test]
    fn malformed_rings_reject_before_dma() {
        let mut ram = vec![0; 512];
        let mut q = queue(&mut ram, true);
        ram[12..14].copy_from_slice(&3u16.to_le_bytes());
        ram[14..16].copy_from_slice(&0u16.to_le_bytes());
        assert!(q.take(&ram, true).is_err());
        q.available = u64::MAX;
        assert!(q.take(&ram, true).is_err());
    }
}
