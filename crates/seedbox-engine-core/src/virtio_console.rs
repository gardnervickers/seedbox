//! Console handshake, backlogs and queue effects shared by platform adapters.
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy, Queue};
use std::collections::VecDeque;

const LIMIT: usize = 65536;
type Result<T> = std::result::Result<T, String>;

/// Adapter checkpoint identity; admission and completion rules are shared.
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
/// Owned checkpoint metadata; constructor-owned policy is verified on restore.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
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
    /// Preserve per-chain event boundaries for adapter delivery.
    pub payloads: Vec<Vec<u8>>,
}

impl Console {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            policy: self.policy,
            device_ready: self.device_ready,
            port_ready: self.port_ready,
            guest_open: self.guest_open,
            rx: self.rx.clone(),
            control_rx: self.control_rx.clone(),
            tx_bytes: self.tx_bytes,
            rx_dma_bytes: self.rx_dma_bytes,
        }
    }

    /// Consume a configured model; failure never publishes partially restored state.
    pub fn restore_snapshot(self, snapshot: Snapshot) -> std::result::Result<Self, String> {
        if snapshot.policy != self.policy || snapshot.rx.len() > LIMIT {
            return Err("console snapshot policy or backlog mismatch".into());
        }
        Ok(Self {
            policy: snapshot.policy,
            device_ready: snapshot.device_ready,
            port_ready: snapshot.port_ready,
            guest_open: snapshot.guest_open,
            rx: snapshot.rx,
            control_rx: snapshot.control_rx,
            tx_bytes: snapshot.tx_bytes,
            rx_dma_bytes: snapshot.rx_dma_bytes,
        })
    }

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
            QueuePolicy {
                limit: LIMIT,
                direction: Direction::Uniform(writable),
                reject_empty: true,
                diagnostics: Diagnostics::Console,
            },
        )
    }
    fn packet(&mut self, event: u16, data: &[u8]) -> Result<()> {
        if self.control_rx.len() >= 16 {
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
        Ok(())
    }
    fn copy_in(
        &self,
        memory: &mut [u8],
        chain: &Chain,
        bytes: &[u8],
        dirty: &mut impl FnMut(usize, usize),
    ) -> Result<()> {
        // Validate direction and capacity before any DMA, including aliases.
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
        if index == 1 || index == 3 {
            while let Some(chain) = self.chain(&transport.queues[index], memory, base, false)? {
                let bytes = chain.read(memory)?;
                if index == 3 {
                    self.control(&bytes)?;
                } else {
                    if !self.port_ready || !self.guest_open {
                        return Err("console transmit before port open".into());
                    }
                    self.tx_bytes = self
                        .tx_bytes
                        .checked_add(bytes.len() as u64)
                        .ok_or("control transmit byte counter overflow")?;
                    out.payloads.push(bytes);
                }
                transport.queues[index].complete_chain(memory, &chain, 0, &mut dirty);
                out.irq = true;
                transport.isr |= 1;
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
            if chain.len < bytes.len() {
                return Err("console control receive buffer too small".into());
            }
            self.copy_in(memory, &chain, bytes, dirty)?;
            transport.queues[2].complete_chain(memory, &chain, bytes.len(), &mut *dirty);
            self.control_rx.pop_front();
            transport.isr |= 1;
            irq = true;
        }
        while !self.rx.is_empty() && (self.port_ready && self.guest_open) {
            let Some(chain) = self.chain(&transport.queues[0], memory, base, true)? else {
                break;
            };
            let n = chain.len.min(self.rx.len());
            let total = self
                .rx_dma_bytes
                .checked_add(n as u64)
                .ok_or("control DMA byte counter overflow")?;
            let bytes: Vec<_> = self.rx.iter().take(n).copied().collect();
            self.copy_in(memory, &chain, &bytes, dirty)?;
            transport.queues[0].complete_chain(memory, &chain, n, &mut *dirty);
            self.rx_dma_bytes = total;
            self.rx.drain(..n);
            transport.isr |= 1;
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
