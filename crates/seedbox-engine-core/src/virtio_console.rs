//! Console handshake, backlogs and queue effects shared by platform adapters.
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy, Queue};
use std::collections::VecDeque;

const LIMIT: usize = 65536;
type Result<T> = std::result::Result<T, String>;

/// Preserve the two existing console contracts during extraction.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    Application,
    Console,
}

#[derive(Clone)]
pub struct Console {
    policy: Policy,
    device_ready: bool,
    port_ready: bool,
    guest_open: bool,
    rx: VecDeque<u8>,
    control_rx: VecDeque<Vec<u8>>,
    tx_bytes: u64,
    rx_dma_bytes: u64,
}

#[derive(Default)]
pub struct Output {
    pub irq: bool,
    /// Application consoles preserve per-chain event boundaries.
    pub payloads: Vec<Vec<u8>>,
}

impl Console {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            device_ready: false,
            port_ready: false,
            guest_open: false,
            rx: VecDeque::new(),
            control_rx: VecDeque::new(),
            tx_bytes: 0,
            rx_dma_bytes: 0,
        }
    }
    pub fn pending_rx_bytes(&self) -> usize {
        self.rx.len()
    }
    pub fn counters(&self) -> (u64, u64) {
        (self.tx_bytes, self.rx_dma_bytes)
    }
    /// Explicit existing ARM model projection; policy is constructor-owned.
    pub fn projection(&self) -> serde_json::Value {
        serde_json::json!({"kind":"console","device_ready":self.device_ready,
            "port_ready":self.port_ready,"guest_open":self.guest_open,"rx":self.rx,
            "control_rx":self.control_rx,"tx_bytes":self.tx_bytes,"rx_dma_bytes":self.rx_dma_bytes})
    }
    fn chain(
        &self,
        queue: &Queue,
        memory: &[u8],
        base: u64,
        writable: bool,
    ) -> Result<Option<Chain>> {
        queue.validated_chain(
            memory,
            base,
            match self.policy {
                Policy::Application => QueuePolicy {
                    limit: 4 << 20,
                    direction: Direction::Any,
                    reject_empty: true,
                    diagnostics: Diagnostics::Virtio,
                },
                Policy::Console => QueuePolicy {
                    limit: LIMIT,
                    direction: Direction::Uniform(writable),
                    reject_empty: true,
                    diagnostics: Diagnostics::Console,
                },
            },
        )
    }
    fn packet(&mut self, event: u16, data: &[u8]) -> Result<()> {
        if self.policy == Policy::Console && self.control_rx.len() >= 16 {
            return Err("console control backlog full".into());
        }
        let mut bytes = 0u32.to_le_bytes().to_vec();
        bytes.extend(event.to_le_bytes());
        bytes.extend(1u16.to_le_bytes());
        bytes.extend(data);
        self.control_rx.push_back(bytes);
        Ok(())
    }
    fn control(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != 8 {
            return Err("invalid console control packet length".into());
        }
        let id = u32::from_le_bytes(bytes[..4].try_into().expect("validated packet"));
        let event = u16::from_le_bytes(bytes[4..6].try_into().expect("validated packet"));
        let value = u16::from_le_bytes(bytes[6..8].try_into().expect("validated packet"));
        match (id, event, value) {
            (u32::MAX, 0, 1) if !self.device_ready => {
                self.device_ready = true;
                self.packet(1, &[])?;
            }
            (0, 3, 1) if self.device_ready && !self.port_ready => {
                self.port_ready = true;
                self.packet(7, b"seedbox")?;
                self.packet(6, &[])?;
            }
            (0, 6, 0 | 1) if self.port_ready => self.guest_open = value == 1,
            _ => return Err("invalid console control handshake".into()),
        }
        if self.policy == Policy::Application && self.control_rx.len() > 16 {
            return Err("console control backlog full".into());
        }
        Ok(())
    }
    fn copy_in(
        &self,
        memory: &mut [u8],
        chain: &Chain,
        bytes: &[u8],
        dirty: &mut impl FnMut(usize, usize),
    ) -> Result<()> {
        if self.policy == Policy::Application {
            // Direction and capacity are checked before any write.
            chain.write(memory, bytes)?;
            let mut reported = 0;
            for segment in &chain.segments {
                let n = segment.len.min(bytes.len() - reported);
                if n > 0 {
                    dirty(segment.offset, n);
                    reported += n;
                }
                if reported == bytes.len() {
                    break;
                }
            }
        } else {
            let mut copied = 0;
            for segment in &chain.segments {
                let n = segment.len.min(bytes.len() - copied);
                if n > 0 {
                    memory[segment.offset..segment.offset + n]
                        .copy_from_slice(&bytes[copied..copied + n]);
                    dirty(segment.offset, n);
                    copied += n;
                }
                if copied == bytes.len() {
                    break;
                }
            }
        }
        Ok(())
    }
    pub fn notify(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        index: usize,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Output> {
        let mut out = Output::default();
        let mut batch_len = 0;
        if index == 1 || index == 3 {
            while let Some(chain) = self.chain(&transport.queues[index], memory, base, false)? {
                let bytes = chain.read(memory)?;
                if self.policy == Policy::Application && bytes.len() > LIMIT {
                    return Err("console transmit chain too large".into());
                }
                if index == 3 {
                    self.control(&bytes)?;
                } else {
                    if self.policy == Policy::Application {
                        self.tx_bytes = self
                            .tx_bytes
                            .checked_add(bytes.len() as u64)
                            .ok_or("control transmit byte counter overflow")?;
                    } else {
                        if !self.port_ready || !self.guest_open {
                            return Err("console transmit before port open".into());
                        }
                        if bytes.len() > LIMIT - batch_len {
                            return Err("console transmit batch too large".into());
                        }
                        batch_len += bytes.len();
                    }
                    out.payloads.push(bytes);
                }
                transport.queues[index].complete_chain(memory, &chain, 0, &mut dirty);
                out.irq = true;
                if self.policy == Policy::Application {
                    transport.isr |= 1;
                }
            }
        }
        out.irq |= self.drain(transport, memory, base, &mut dirty)?;
        if out.irq {
            transport.isr |= 1;
        }
        Ok(out)
    }
    pub fn receive(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        bytes: &[u8],
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<bool> {
        if self.policy == Policy::Console && transport.status & 4 == 0 {
            return Err("console driver not ready".into());
        }
        if self
            .rx
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > LIMIT)
        {
            return Err("console receive backlog full".into());
        }
        self.rx.extend(bytes);
        self.drain(transport, memory, base, &mut dirty)
    }
    fn drain(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        dirty: &mut impl FnMut(usize, usize),
    ) -> Result<bool> {
        let mut irq = false;
        while let Some(bytes) = self.control_rx.front() {
            let Some(chain) = self.chain(&transport.queues[2], memory, base, true)? else {
                break;
            };
            if self.policy == Policy::Console {
                if chain.len < bytes.len() {
                    return Err("console control receive buffer too small".into());
                }
                let bytes = self.control_rx.pop_front().expect("nonempty backlog");
                self.copy_in(memory, &chain, &bytes, dirty)?;
                transport.queues[2].complete_chain(memory, &chain, bytes.len(), &mut *dirty);
            } else {
                self.copy_in(memory, &chain, bytes, dirty)?;
                transport.queues[2].complete_chain(memory, &chain, bytes.len(), &mut *dirty);
                self.control_rx.pop_front();
                transport.isr |= 1;
            }
            irq = true;
        }
        while !self.rx.is_empty()
            && (self.policy == Policy::Application || self.port_ready && self.guest_open)
        {
            let Some(chain) = self.chain(&transport.queues[0], memory, base, true)? else {
                break;
            };
            let n = chain.len.min(self.rx.len());
            let bytes: Vec<_> = if self.policy == Policy::Console {
                self.rx.drain(..n).collect()
            } else {
                self.rx.iter().take(n).copied().collect()
            };
            self.copy_in(memory, &chain, &bytes, dirty)?;
            transport.queues[0].complete_chain(memory, &chain, n, &mut *dirty);
            if self.policy == Policy::Application {
                self.rx_dma_bytes = self
                    .rx_dma_bytes
                    .checked_add(n as u64)
                    .ok_or("control DMA byte counter overflow")?;
                self.rx.drain(..n);
                transport.isr |= 1;
            }
            irq = true;
        }
        if irq {
            transport.isr |= 1;
        }
        Ok(irq)
    }
}

#[cfg(test)]
#[path = "virtio_console_tests.rs"]
mod tests;
