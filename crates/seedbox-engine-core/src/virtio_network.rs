//! Bounded network queues and ingress shared by application and KVM adapters.
use crate::virtio_mmio::Transport;
use crate::virtqueue::{Chain, Diagnostics, Direction, Policy as QueuePolicy, Queue};
use std::collections::VecDeque;

const HEADER: usize = 12;
const CHAIN_LIMIT: usize = 4 << 20;
const BACKLOG: usize = 256;
pub const EVENT_IDX: u32 = 1 << 29;
pub const TX_POLL_NS: u64 = 50_000;
pub fn poll_limit(now: u64, limit: u64, deferred: bool) -> u64 {
    if deferred {
        limit.min(now.saturating_add(TX_POLL_NS))
    } else {
        limit
    }
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
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
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stats {
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
    pub doorbell_exits: u64,
    pub suppressed_interrupts: u64,
    pub tx_polled_at_boundary: u64,
    pub tx_polled: u64,
    pub tx_pickup_samples: u64,
    pub tx_pickup_delay_upper_ns_sum: u128,
    pub tx_pickup_delay_upper_ns_max: u64,
}
#[derive(Clone)]
pub struct Network {
    policy: Policy,
    mac: [u8; 6],
    backlog: VecDeque<Vec<u8>>,
    stats: Stats,
    deferred_tx: bool,
    tx_empty_at: Option<u64>,
}
/// Owned checkpoint metadata; constructor-owned policy is verified on restore.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    policy: Policy,
    mac: [u8; 6],
    backlog: VecDeque<Vec<u8>>,
    stats: Stats,
    deferred_tx: bool,
    tx_empty_at: Option<u64>,
}

impl Network {
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            policy: self.policy,
            mac: self.mac,
            backlog: self.backlog.clone(),
            stats: self.stats.clone(),
            deferred_tx: self.deferred_tx,
            tx_empty_at: self.tx_empty_at,
        }
    }

    /// Consume a configured model; failure never publishes partially restored state.
    pub fn restore_snapshot(self, snapshot: Snapshot) -> std::result::Result<Self, String> {
        if snapshot.policy != self.policy
            || snapshot.mac != self.mac
            || snapshot.backlog.len() > BACKLOG
        {
            return Err("network snapshot configuration or backlog mismatch".into());
        }
        Ok(Self {
            policy: snapshot.policy,
            mac: snapshot.mac,
            backlog: snapshot.backlog,
            stats: snapshot.stats,
            deferred_tx: snapshot.deferred_tx,
            tx_empty_at: snapshot.tx_empty_at,
        })
    }

    pub fn new(mac: [u8; 6], policy: Policy) -> Self {
        Self {
            policy,
            mac,
            backlog: VecDeque::new(),
            stats: Stats::default(),
            deferred_tx: false,
            tx_empty_at: None,
        }
    }
    pub fn mac(&self) -> [u8; 6] {
        self.mac
    }
    pub fn stats(&self) -> &Stats {
        &self.stats
    }
    pub fn set_deferred_tx(&mut self, enabled: bool) {
        self.deferred_tx = enabled && self.policy == Policy::Network;
    }
    pub fn deferred_tx(&self) -> bool {
        self.deferred_tx
    }
    pub fn reset(&mut self) {
        let deferred = self.deferred_tx;
        *self = Self::new(self.mac, self.policy);
        self.deferred_tx = deferred;
    }
    pub fn event_idx(&self, transport: &Transport) -> bool {
        self.policy == Policy::Network && transport.negotiated(0, EVENT_IDX)
    }
    pub fn doorbell(&mut self) {
        if self.policy == Policy::Network {
            self.stats.doorbell_exits += 1;
        }
    }
    pub fn pending_tx(
        &self,
        transport: &Transport,
        memory: &[u8],
        base: u64,
    ) -> Result<bool, String> {
        transport.queues[1].pending(memory, base)
    }
    pub fn after_write(
        &mut self,
        transport: &Transport,
        memory: &mut [u8],
        base: u64,
        offset: u64,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<(), String> {
        if self.policy != Policy::Network {
            return Ok(());
        }
        let event_idx = self.event_idx(transport);
        if offset == 0x44 && event_idx {
            let queue = &transport.queues[transport.selected_queue()];
            queue.suppress_notify(memory, base, false, &mut dirty)?;
            queue.arm_notify(memory, base, true, false, &mut dirty)?;
        }
        if self.deferred_tx {
            transport.queues[1].arm_notify(memory, base, event_idx, true, dirty)?;
        }
        Ok(())
    }
    fn interrupt(
        &mut self,
        transport: &mut Transport,
        memory: &[u8],
        base: u64,
        queue: usize,
        old_used: u16,
    ) -> Result<bool, String> {
        if transport.queues[queue].used_idx == old_used {
            return Ok(false);
        }
        let irq =
            transport.queues[queue].interrupt(memory, base, self.event_idx(transport), old_used)?;
        if irq {
            transport.isr |= 1;
        } else {
            self.stats.suppressed_interrupts += 1;
        }
        Ok(irq)
    }
    pub fn poll(
        &mut self,
        transport: &mut Transport,
        memory: &mut [u8],
        base: u64,
        mut dirty: impl FnMut(usize, usize),
    ) -> Result<Notified, String> {
        let event_idx = self.event_idx(transport);
        if !event_idx && !self.deferred_tx {
            return Ok(Notified::default());
        }
        let rx = event_idx && self.drain(transport, memory, base, &mut dirty)?;
        let mut out = self.notify(transport, memory, base, 1, &mut dirty)?;
        out.irq |= rx;
        if self.deferred_tx {
            transport.queues[1].arm_notify(memory, base, event_idx, true, dirty)?;
        }
        Ok(out)
    }
    pub fn pickup_stats(
        &mut self,
        transport: &Transport,
        memory: &[u8],
        base: u64,
        now: u64,
        packets: u64,
        polled: Option<bool>,
    ) -> Result<(), String> {
        if let Some(boundary) = polled {
            self.stats.tx_polled += packets;
            if boundary {
                self.stats.tx_polled_at_boundary += packets;
            }
        }
        if let Some(empty) = self.tx_empty_at {
            let upper = now.saturating_sub(empty);
            self.stats.tx_pickup_samples += packets;
            self.stats.tx_pickup_delay_upper_ns_sum += upper as u128 * packets as u128;
            if packets > 0 {
                self.stats.tx_pickup_delay_upper_ns_max =
                    self.stats.tx_pickup_delay_upper_ns_max.max(upper);
            }
        }
        if transport.queues[1].ready && !self.pending_tx(transport, memory, base)? {
            self.tx_empty_at = Some(now);
        }
        Ok(())
    }
    pub fn report(&self, transport: &Transport) -> serde_json::Value {
        let s = &self.stats;
        let pickup_mean = if s.tx_pickup_samples == 0 {
            0
        } else {
            (s.tx_pickup_delay_upper_ns_sum / s.tx_pickup_samples as u128) as u64
        };
        serde_json::json!({
            "tx": s.tx_frames,
            "rx": s.rx_frames,
            "rx_dropped": s.rx_dropped,
            "event_idx": self.event_idx(transport),
            "doorbell_exits": s.doorbell_exits,
            "suppressed_interrupts": s.suppressed_interrupts,
            "deferred_tx": self.deferred_tx,
            "tx_polled": s.tx_polled,
            "tx_polled_at_boundary": s.tx_polled_at_boundary,
            "tx_pickup_samples": s.tx_pickup_samples,
            "tx_pickup_delay_upper_ns_mean": pickup_mean,
            "tx_pickup_delay_upper_ns_max": s.tx_pickup_delay_upper_ns_max,
        })
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
                direction: Direction::Uniform(writable),
                reject_empty: true,
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
        let event_idx = self.event_idx(transport);
        if self.policy == Policy::Network {
            while transport.queues[index].pending(memory, base)? {
                if !event_idx && !self.deferred_tx {
                    transport.queues[index].suppress_notify(memory, base, true, &mut dirty)?;
                }
                while let Some(chain) = self.chain(&transport.queues[index], memory, base, false)? {
                    let bytes = chain.read(memory)?;
                    Self::validate_header(&bytes)?;
                    let previous = transport.queues[index].used_idx;
                    transport.queues[index].complete_chain(memory, &chain, 0, &mut dirty);
                    // Publish the completed prefix before decoding the next request.
                    out.irq |= self.interrupt(transport, memory, base, index, previous)?;
                    if bytes.len() > HEADER {
                        out.frames.push(bytes[HEADER..].to_vec());
                        self.stats.tx_frames += 1;
                    }
                }
                if transport.queues[index].arm_notify(
                    memory,
                    base,
                    event_idx,
                    self.deferred_tx,
                    &mut dirty,
                )? {
                    continue;
                }
                break;
            }
            return Ok(out);
        }
        while let Some(chain) = self.chain(&transport.queues[index], memory, base, false)? {
            let bytes = chain.read(memory)?;
            Self::validate_header(&bytes)?;
            transport.queues[index].complete_chain(memory, &chain, 0, &mut dirty);
            if bytes.len() > HEADER {
                out.frames.push(bytes[HEADER..].to_vec());
            }
            out.irq = true;
            transport.isr |= 1;
        }
        out.irq |= self.drain(transport, memory, base, &mut dirty)?;
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
        if chain.len < HEADER + frame.len() {
            return Err("network receive buffer too small".into());
        }
        let mut packet = vec![0; HEADER];
        packet[10] = 1;
        packet.extend_from_slice(frame);
        let packet_len = packet.len();
        let mut written = 0;
        for segment in &chain.segments {
            let n = segment.len.min(packet_len - written);
            memory[segment.offset..segment.offset + n]
                .copy_from_slice(&packet[written..written + n]);
            dirty(segment.offset, n);
            written += n;
            if written == packet_len {
                break;
            }
        }
        transport.queues[0].complete_chain(memory, &chain, written, dirty);
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
        let event_idx = self.event_idx(transport);
        if self.policy == Policy::Network {
            while !self.backlog.is_empty() && transport.queues[0].pending(memory, base)? {
                if !event_idx {
                    transport.queues[0].suppress_notify(memory, base, true, &mut *dirty)?;
                }
                while let Some(frame) = self.backlog.front().cloned() {
                    let previous = transport.queues[0].used_idx;
                    if !self.put_frame(transport, memory, base, &frame, dirty)? {
                        break;
                    }
                    self.backlog.pop_front();
                    any |= self.interrupt(transport, memory, base, 0, previous)?;
                }
                if !transport.queues[0].arm_notify(memory, base, event_idx, false, &mut *dirty)? {
                    break;
                }
            }
        } else {
            while let Some(frame) = self.backlog.front().cloned() {
                if !self.put_frame(transport, memory, base, &frame, dirty)? {
                    break;
                }
                self.backlog.pop_front();
                any = true;
                transport.isr |= 1;
            }
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
        if frame.len() > 65536 {
            return Err("network frame too large".into());
        }
        if !transport.queues[0].ready || self.backlog.len() >= BACKLOG {
            if self.policy == Policy::Network {
                self.stats.rx_dropped += 1;
            }
            return Ok(Received::default());
        }
        // Own ingress before attempting DMA so failures retain the same backlog.
        let waiting = !self.backlog.is_empty();
        self.backlog.push_back(frame.to_vec());
        let mut irq = false;
        let previous = transport.queues[0].used_idx;
        if !waiting && self.put_frame(transport, memory, base, frame, &mut dirty)? {
            self.backlog.pop_front();
            if self.policy == Policy::Network {
                irq = self.interrupt(transport, memory, base, 0, previous)?;
                if self.event_idx(transport) {
                    transport.queues[0].arm_notify(memory, base, true, false, &mut dirty)?;
                }
            } else {
                transport.isr |= 1;
                irq = true;
            }
        }
        Ok(Received {
            accepted: true,
            irq,
        })
    }
    fn validate_header(bytes: &[u8]) -> Result<(), String> {
        if bytes.len() < HEADER {
            return Err("short virtio network header".into());
        }
        if bytes[..10].iter().any(|b| *b != 0) {
            return Err("unnegotiated virtio network offload".into());
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "virtio_network_tests.rs"]
mod tests;
