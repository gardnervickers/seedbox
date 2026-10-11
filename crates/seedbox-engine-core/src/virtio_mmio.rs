//! Modern virtio MMIO registers, independent of RAM, device effects and IRQ wiring.
//! Adapter identities are retained in checkpoints; register rules are common.
use crate::virtqueue::Queue;
use serde::Serialize;

pub const QUEUE_SIZE: u16 = 256;
pub(crate) const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    pub device_id: u32,
    pub features: [u32; 2],
}

/// Adapter checkpoint identity.
/// See docs/internals.md#device-completion-and-disk-state before converging profiles.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Policy {
    Application,
    Console,
    Block,
    Network,
}

/// RAM and model effects remain in the adapter, after register admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Notify(usize),
    /// Transport has reset; the adapter must perform its model reset as well.
    Reset,
}

#[derive(Clone)]
pub struct Transport {
    spec: Spec,
    policy: Policy,
    dev_sel: u32,
    drv_sel: u32,
    features: [u32; 2],
    queue_sel: u32,
    pub queues: Vec<Queue>,
    pub status: u32,
    pub isr: u32,
}
/// Owned checkpoint metadata; constructor-owned policy is verified on restore.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    spec: Spec,
    policy: Policy,
    dev_sel: u32,
    drv_sel: u32,
    features: [u32; 2],
    queue_sel: u32,
    queues: Vec<Queue>,
    status: u32,
    isr: u32,
}

/// Preserve the existing ARM retained-state field names and order explicitly.
#[derive(Serialize)]
pub struct Projection<'a> {
    dev_sel: u32,
    drv_sel: u32,
    features: [u32; 2],
    queue_sel: u32,
    queues: &'a [Queue],
    status: u32,
    isr: u32,
}

impl Transport {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            spec: self.spec,
            policy: self.policy,
            dev_sel: self.dev_sel,
            drv_sel: self.drv_sel,
            features: self.features,
            queue_sel: self.queue_sel,
            queues: self.queues.clone(),
            status: self.status,
            isr: self.isr,
        }
    }

    /// Consume a configured model; failure never publishes partially restored state.
    pub fn restore_snapshot(self, snapshot: Snapshot) -> std::result::Result<Self, String> {
        if snapshot.spec != self.spec
            || snapshot.policy != self.policy
            || snapshot.queues.len() != self.queues.len()
        {
            return Err("virtio snapshot configuration mismatch".into());
        }
        Ok(Self {
            spec: snapshot.spec,
            policy: snapshot.policy,
            dev_sel: snapshot.dev_sel,
            drv_sel: snapshot.drv_sel,
            features: snapshot.features,
            queue_sel: snapshot.queue_sel,
            queues: snapshot.queues,
            status: snapshot.status,
            isr: snapshot.isr,
        })
    }

    pub fn new(spec: Spec, queues: usize, policy: Policy) -> Self {
        Self {
            spec,
            policy,
            dev_sel: 0,
            drv_sel: 0,
            features: [0; 2],
            queue_sel: 0,
            queues: vec![Queue::default(); queues],
            status: 0,
            isr: 0,
        }
    }

    pub fn projection(&self) -> Projection<'_> {
        Projection {
            dev_sel: self.dev_sel,
            drv_sel: self.drv_sel,
            features: self.features,
            queue_sel: self.queue_sel,
            queues: &self.queues,
            status: self.status,
            isr: self.isr,
        }
    }

    pub fn negotiated(&self, word: usize, feature: u32) -> bool {
        self.status & FEATURES_OK != 0
            && self
                .spec
                .features
                .get(word)
                .is_some_and(|bits| bits & feature != 0)
            && self
                .features
                .get(word)
                .is_some_and(|bits| bits & feature != 0)
    }
    pub fn selected_queue(&self) -> usize {
        (self.queue_sel as usize).min(self.queues.len() - 1)
    }

    /// Configuration-space reads are handled by the device adapter.
    pub fn read(&self, offset: u64) -> u32 {
        match offset {
            0 => 0x7472_6976,
            4 => 2,
            8 => self.spec.device_id,
            12 => 0x5345_4544,
            0x10 => self
                .spec
                .features
                .get(self.dev_sel as usize)
                .copied()
                .unwrap_or(0),
            0x34 => {
                if self.queues.get(self.queue_sel as usize).is_some() {
                    QUEUE_SIZE as u32
                } else {
                    0
                }
            }
            0x44 => self
                .queues
                .get(self.queue_sel as usize)
                .is_some_and(|queue| queue.ready) as u32,
            0x60 => self.isr,
            0x70 => self.status,
            _ => 0,
        }
    }

    pub fn write(&mut self, offset: u64, value: u32) -> Result<Effect, String> {
        match offset {
            0x14 => self.dev_sel = value,
            0x20 => {
                if let Some(word) = self.features.get_mut(self.drv_sel as usize) {
                    *word = value;
                }
            }
            0x24 => self.drv_sel = value,
            0x30 => self.queue_sel = value,
            0x38 | 0x44 | 0x80 | 0x84 | 0x90 | 0x94 | 0xa0 | 0xa4 => {
                let Some(queue) = self.queues.get_mut(self.queue_sel as usize) else {
                    return Err("virtio queue selection out of range".into());
                };
                if queue.ready && offset != 0x44 {
                    return Err("virtio queue changed while ready".into());
                }
                match offset {
                    0x38 => {
                        if value == 0 || value > QUEUE_SIZE as u32 || !value.is_power_of_two() {
                            return Err("invalid virtio queue size".into());
                        }
                        queue.num = value as u16;
                    }
                    0x44 => {
                        if value > 1 {
                            return Err("invalid virtio queue ready flag".into());
                        }
                        queue.ready = value == 1;
                        if value == 0 {
                            queue.last_avail = 0;
                            queue.used_idx = 0;
                        }
                    }
                    0x80 => queue.desc = (queue.desc & !0xffff_ffff) | value as u64,
                    0x84 => queue.desc = (queue.desc & 0xffff_ffff) | ((value as u64) << 32),
                    0x90 => queue.avail = (queue.avail & !0xffff_ffff) | value as u64,
                    0x94 => queue.avail = (queue.avail & 0xffff_ffff) | ((value as u64) << 32),
                    0xa0 => queue.used = (queue.used & !0xffff_ffff) | value as u64,
                    0xa4 => queue.used = (queue.used & 0xffff_ffff) | ((value as u64) << 32),
                    _ => unreachable!(),
                }
            }
            0x50 => {
                if value as usize >= self.queues.len() {
                    return Err("virtio notification queue out of range".into());
                }
                if self.status & DRIVER_OK == 0 {
                    return Err("virtio notify before DRIVER_OK".into());
                }
                return Ok(Effect::Notify(value as usize));
            }
            0x64 => self.isr &= !value,
            0x70 => {
                self.status = value;
                if value == 0 {
                    *self = Self::new(self.spec, self.queues.len(), self.policy);
                    return Ok(Effect::Reset);
                }
                // VERSION_1 is mandatory. Our control console requires MULTIPORT;
                // other advertised device features are optional.
                if value & FEATURES_OK != 0
                    && (self.features[1] & 1 == 0
                        || (self.spec.device_id == 3 && self.features[0] & 2 == 0)
                        || self
                            .features
                            .iter()
                            .zip(self.spec.features)
                            .any(|(selected, offered)| selected & !offered != 0))
                {
                    self.status &= !FEATURES_OK;
                }
                if value & DRIVER_OK != 0 && self.status & FEATURES_OK == 0 {
                    return Err("virtio DRIVER_OK without negotiated features".into());
                }
            }
            _ => (),
        }
        Ok(Effect::None)
    }
}

#[cfg(test)]
#[path = "virtio_mmio_tests.rs"]
mod tests;
