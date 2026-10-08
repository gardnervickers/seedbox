//! Modern virtio MMIO registers, independent of RAM, device effects and IRQ wiring.
//! Compatibility profiles retain the policies of the migrated device adapters.
use crate::virtqueue::Queue;
use serde::Serialize;

pub const QUEUE_SIZE: u16 = 256;
const DRIVER_OK: u32 = 4;
const FEATURES_OK: u32 = 8;

#[derive(Clone, Copy)]
pub struct Spec {
    pub device_id: u32,
    pub features: [u32; 2],
}

/// Existing register behavior; see docs/virtio-transport.md before converging profiles.
#[derive(Clone, Copy, PartialEq, Eq)]
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
                let selected = if self.policy == Policy::Network {
                    (self.queue_sel as usize).min(1)
                } else {
                    self.queue_sel as usize
                };
                let Some(queue) = self.queues.get_mut(selected) else {
                    return match self.policy {
                        Policy::Application => Err("virtio queue selection out of range".into()),
                        Policy::Console => Err("invalid console queue selector".into()),
                        Policy::Block | Policy::Network => Ok(Effect::None),
                    };
                };
                if self.policy == Policy::Console && queue.ready && offset != 0x44 {
                    return Err("console queue changed while ready".into());
                }
                match offset {
                    0x38 => {
                        if matches!(self.policy, Policy::Application | Policy::Console)
                            && (value == 0 || value > QUEUE_SIZE as u32 || !value.is_power_of_two())
                        {
                            return Err(match self.policy {
                                Policy::Console => "invalid console queue size",
                                _ => "invalid virtio queue size",
                            }
                            .into());
                        }
                        queue.num = if self.policy == Policy::Network {
                            (value as u16).clamp(1, QUEUE_SIZE)
                        } else {
                            value as u16
                        };
                    }
                    0x44 => {
                        if matches!(self.policy, Policy::Application | Policy::Console) && value > 1
                        {
                            return Err(match self.policy {
                                Policy::Console => "invalid console queue ready value",
                                _ => "invalid virtio queue ready flag",
                            }
                            .into());
                        }
                        queue.ready = value == 1;
                        if self.policy != Policy::Console || value == 0 {
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
                match self.policy {
                    Policy::Application if value as usize >= self.queues.len() => {
                        return Err("virtio notification queue out of range".into())
                    }
                    Policy::Console => {
                        if value as usize >= self.queues.len() {
                            return Err("invalid console notify queue".into());
                        }
                        if self.status & DRIVER_OK == 0 {
                            return Err("console notify before DRIVER_OK".into());
                        }
                    }
                    Policy::Block if value != 0 => return Ok(Effect::None),
                    Policy::Network if value > 1 => return Ok(Effect::None),
                    _ => (),
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
                if self.policy == Policy::Console {
                    if value & FEATURES_OK != 0 && self.features != self.spec.features {
                        self.status &= !FEATURES_OK;
                    }
                    if value & DRIVER_OK != 0 && self.status & FEATURES_OK == 0 {
                        return Err("console DRIVER_OK without negotiated features".into());
                    }
                }
            }
            _ => (),
        }
        Ok(Effect::None)
    }
}
