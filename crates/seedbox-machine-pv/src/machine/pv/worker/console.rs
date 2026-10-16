//! Modern virtio MMIO console: a single named lifecycle port, no application
//! control personality. Split rings are checked in full before every DMA
//! (`virtio.rs`).
#[cfg(test)]
use super::boot::RAM_BASE as BASE;
use super::virtio::*;
use super::Result;
use crate::pv::transport::LifecycleOutput;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
const PORT: &[u8] = b"seedbox-lifecycle";
const MAX_PORTS: u64 = 0x104;
const EMERGENCY_WRITE: u64 = 0x108;
// Console control events. Queue ordering is fixed by the multiport console
// ABI: port RX/TX, then control RX/TX.
const CONSOLE_SIZE: u64 = 0x100;
const FEATURE_MULTIPORT: u32 = 1 << 1;
const FEATURE_VERSION_1_HIGH: u32 = 1;
const PORT_RX: usize = 0;
const PORT_TX: usize = 1;
const CONTROL_RX: usize = 2;
const CONTROL_TX: usize = 3;
const DEVICE_READY: u16 = 0;
const PORT_ADD: u16 = 1;
const PORT_READY: u16 = 3;
const PORT_OPEN: u16 = 6;
const PORT_NAME: u16 = 7;
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
                super::virtio::configure(&mut self.queues, self.selector, offset, value)?
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
mod tests;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl crate::machine::pv::codec::record::Record for Console {
    const MIN: u64 = <[Queue; 4] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <[u32; 2] as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <u32 as crate::machine::pv::codec::record::Record>::MIN
        + <VecDeque<Vec<u8>> as crate::machine::pv::codec::record::Record>::MIN
        + <VecDeque<u8> as crate::machine::pv::codec::record::Record>::MIN
        + <bool as crate::machine::pv::codec::record::Record>::MIN
        + <bool as crate::machine::pv::codec::record::Record>::MIN
        + <LifecycleOutput as crate::machine::pv::codec::record::Record>::MIN;
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
            controls,
            data,
            announced,
            open,
            output,
        } = self;
        queues.encode(w)?;
        selector.encode(w)?;
        feature_selector.encode(w)?;
        driver_selector.encode(w)?;
        features.encode(w)?;
        status.encode(w)?;
        interrupt.encode(w)?;
        controls.encode(w)?;
        data.encode(w)?;
        announced.encode(w)?;
        open.encode(w)?;
        output.encode(w)?;
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
            controls: crate::machine::pv::codec::record::Record::decode(r)?,
            data: crate::machine::pv::codec::record::Record::decode(r)?,
            announced: crate::machine::pv::codec::record::Record::decode(r)?,
            open: crate::machine::pv::codec::record::Record::decode(r)?,
            output: crate::machine::pv::codec::record::Record::decode(r)?,
        })
    }
}

impl Console {
    pub(super) fn validate_snapshot(&self) -> Result<()> {
        validate_transport(&self.queues, self.interrupt)?;
        if !self.output.is_empty() {
            return Err("PV snapshot undrained lifecycle device output".into());
        }
        for control in &self.controls {
            let valid = control.len() >= 8
                && control[..4] == [0; 4]
                && control[6..8] == [1, 0]
                && match u16::from_le_bytes([control[4], control[5]]) {
                    PORT_ADD | PORT_OPEN => control.len() == 8,
                    PORT_NAME => &control[8..] == PORT,
                    _ => false,
                };
            if !valid {
                return Err("PV snapshot console control record".into());
            }
        }
        Ok(())
    }
}
