//! Bounded network queues and ingress shared by application and KVM adapters.
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy, Queue};
use std::collections::VecDeque;

const HEADER: usize = 12;
const CHAIN_LIMIT: usize = 4 << 20;
const BACKLOG: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Application,
    Network,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Received {
    pub accepted: bool,
    pub irq: bool,
}
#[derive(Default)]
pub struct Notified {
    pub frames: Vec<Vec<u8>>,
    pub irq: bool,
}
#[derive(Clone, Default)]
pub struct Stats {
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
}
#[derive(Clone)]
pub struct Network {
    policy: Policy,
    mac: [u8; 6],
    backlog: VecDeque<Vec<u8>>,
    stats: Stats,
}
impl Network {
    pub fn new(mac: [u8; 6], policy: Policy) -> Self {
        Self {
            policy,
            mac,
            backlog: VecDeque::new(),
            stats: Stats::default(),
        }
    }
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn projection(&self) -> serde_json::Value {
        serde_json::json!({"kind":"net","mac":self.mac,"backlog":self.backlog})
    }
    fn chain(
        &self,
        queue: &Queue,
        memory: &[u8],
        base: u64,
        writable: bool,
    ) -> Result<Option<Chain>, String> {
        queue.validated_chain(
            memory,
            base,
            QueuePolicy {
                limit: CHAIN_LIMIT,
                direction: if self.policy == Policy::Application {
                    Direction::Any
                } else {
                    Direction::Uniform(writable)
                },
                reject_empty: self.policy == Policy::Application,
                diagnostics: Diagnostics::Virtio,
            },
        )
    }
    pub fn notify(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        index: usize,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Notified, String> {
        let mut out = Notified::default();
        if index == 0 {
            out.irq = self.drain(transport, memory, base, &mut dirty)?;
            return Ok(out);
        }
        while let Some(chain) = self.chain(&transport.queues[index], memory, base, false)? {
            let bytes = chain.read(memory)?;
            if self.policy == Policy::Application {
                if bytes.len() < HEADER {
                    return Err("short virtio network header".into());
                }
                if bytes[..10].iter().any(|b| *b != 0) {
                    return Err("unnegotiated virtio network offload".into());
                }
            }
            transport.queues[index].complete_chain(memory, &chain, 0, &mut dirty);
            if bytes.len() > HEADER {
                out.frames.push(bytes[HEADER..].to_vec());
                if self.policy == Policy::Network {
                    self.stats.tx_frames += 1;
                }
            }
            out.irq = true;
            if self.policy == Policy::Application {
                transport.isr |= 1;
            }
        }
        if self.policy == Policy::Application {
            out.irq |= self.drain(transport, memory, base, &mut dirty)?;
        } else if out.irq {
            transport.isr |= 1;
        }
        Ok(out)
    }
    fn put_frame(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        frame: &[u8],
        dirty: &mut impl FnMut(usize, usize),
    ) -> Result<bool, String> {
        let Some(chain) = self.chain(&transport.queues[0], memory, base, true)? else {
            return Ok(false);
        };
        if chain.len < HEADER + frame.len() || chain.segments.iter().any(|s| !s.writable) {
            return Err(if self.policy == Policy::Application {
                "invalid/short virtio receive chain"
            } else {
                "network receive buffer too small"
            }
            .into());
        }
        let mut packet = vec![0; HEADER];
        packet[10] = 1;
        packet.extend_from_slice(frame);
        let mut written = 0;
        for segment in &chain.segments {
            let n = segment.len.min(packet.len() - written);
            memory[segment.offset..segment.offset + n]
                .copy_from_slice(&packet[written..written + n]);
            dirty(segment.offset, n);
            written += n;
            if written == packet.len() {
                break;
            }
        }
        transport.queues[0].complete_chain(memory, &chain, packet.len(), dirty);
        if self.policy == Policy::Network {
            self.stats.rx_frames += 1;
        }
        Ok(true)
    }
    fn drain(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        dirty: &mut impl FnMut(usize, usize),
    ) -> Result<bool, String> {
        let mut any = false;
        while let Some(frame) = self.backlog.front().cloned() {
            if !self.put_frame(transport, memory, base, &frame, dirty)? {
                break;
            }
            self.backlog.pop_front();
            any = true;
            if self.policy == Policy::Application {
                transport.isr |= 1;
            }
        }
        if any {
            transport.isr |= 1;
        }
        Ok(any)
    }
    pub fn receive(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        frame: &[u8],
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Received, String> {
        if self.policy == Policy::Application {
            if !transport.queues[0].ready || self.backlog.len() >= BACKLOG {
                return Ok(Received::default());
            }
            if frame.len() > 65536 {
                return Err("network frame too large".into());
            }
            self.backlog.push_back(frame.to_vec());
            let irq = self.drain(transport, memory, base, &mut dirty)?;
            return Ok(Received {
                accepted: true,
                irq,
            });
        }
        if !transport.queues[0].ready {
            self.stats.rx_dropped += 1;
            return Ok(Received::default());
        }
        if frame.len() > CHAIN_LIMIT - HEADER {
            return Err("network frame exceeds DMA limit".into());
        }
        if !self.backlog.is_empty()
            || !self.put_frame(transport, memory, base, frame, &mut dirty)?
        {
            if self.backlog.len() >= BACKLOG {
                self.stats.rx_dropped += 1;
                return Ok(Received::default());
            }
            self.backlog.push_back(frame.to_vec());
            return Ok(Received {
                accepted: true,
                irq: false,
            });
        }
        transport.isr |= 1;
        Ok(Received {
            accepted: true,
            irq: true,
        })
    }
}

#[cfg(test)]
#[path = "virtio_network_tests.rs"]
mod tests;
