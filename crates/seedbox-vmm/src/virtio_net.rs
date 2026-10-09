//! KVM network adapter: guest address base, DMA journal and shared device state.
use seedbox_engine_core::virtio_mmio::{Effect, Policy as MmioPolicy, Spec, Transport};
pub use seedbox_engine_core::virtio_network::{
    poll_limit, Notified as WriteOutcome, Received as ReceiveOutcome,
};
use seedbox_engine_core::virtio_network::{Network, Policy, Stats, EVENT_IDX};
#[derive(Clone)]
pub struct VirtioNet {
    transport: Transport,
    model: Network,
    pub dirty: Vec<(usize, usize)>,
}
impl VirtioNet {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            transport: Transport::new(
                Spec {
                    device_id: 1,
                    features: [1 << 5 | EVENT_IDX, 1],
                },
                2,
                MmioPolicy::Network,
            ),
            model: Network::new(mac, Policy::Network),
            dirty: Vec::new(),
        }
    }
    pub fn stats(&self) -> &Stats {
        self.model.stats()
    }
    pub fn set_deferred_tx(&mut self, enabled: bool) {
        self.model.set_deferred_tx(enabled)
    }
    pub fn deferred_tx(&self) -> bool {
        self.model.deferred_tx()
    }
    #[cfg(test)]
    pub fn event_idx(&self) -> bool {
        self.model.event_idx(&self.transport)
    }
    pub fn report(&self) -> serde_json::Value {
        self.model.report(&self.transport)
    }
    #[cfg(test)]
    pub fn pending_tx(&self, m: &[u8]) -> Result<bool, String> {
        self.model.pending_tx(&self.transport, m, 0)
    }
    pub fn read(&mut self, off: u64) -> u32 {
        if off >= 0x100 {
            let mac = self.model.mac();
            let byte = |o: u64| mac.get(o as usize).copied().unwrap_or(0);
            let b = off - 0x100;
            u32::from_le_bytes([byte(b), byte(b + 1), byte(b + 2), byte(b + 3)])
        } else {
            self.transport.read(off)
        }
    }
    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<WriteOutcome, String> {
        if off == 0x50 {
            self.model.doorbell();
        }
        let out = match self.transport.write(off, v)? {
            Effect::None => WriteOutcome::default(),
            Effect::Reset => {
                self.model.reset();
                WriteOutcome::default()
            }
            Effect::Notify(index) => {
                self.model
                    .notify(&mut self.transport, m, 0, index, |a, n| {
                        self.dirty.push((a, n))
                    })?
            }
        };
        self.model
            .after_write(&self.transport, m, 0, off, |a, n| self.dirty.push((a, n)))?;
        Ok(out)
    }
    pub fn poll(&mut self, m: &mut [u8]) -> Result<WriteOutcome, String> {
        self.model
            .poll(&mut self.transport, m, 0, |a, n| self.dirty.push((a, n)))
    }
    pub fn poll_at(
        &mut self,
        m: &mut [u8],
        now: u64,
        boundary: bool,
    ) -> Result<WriteOutcome, String> {
        let before = self.stats().tx_frames;
        let out = self.poll(m)?;
        self.model.pickup_stats(
            &self.transport,
            m,
            0,
            now,
            self.model.stats().tx_frames - before,
            Some(boundary),
        )?;
        Ok(out)
    }
    pub fn write_at(
        &mut self,
        m: &mut [u8],
        off: u64,
        v: u32,
        now: u64,
    ) -> Result<WriteOutcome, String> {
        let before = self.stats().tx_frames;
        let out = self.write(m, off, v)?;
        self.model.pickup_stats(
            &self.transport,
            m,
            0,
            now,
            self.model.stats().tx_frames.saturating_sub(before),
            None,
        )?;
        Ok(out)
    }
    pub fn receive(&mut self, m: &mut [u8], frame: &[u8]) -> Result<ReceiveOutcome, String> {
        self.model
            .receive(&mut self.transport, m, 0, frame, |a, n| {
                self.dirty.push((a, n))
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_core::virtqueue::{need_event as vring_need_event, Queue};
    const QSIZE: u16 = 256;
    const HDR: usize = 12;
    const VIRTIO_RING_F_EVENT_IDX: u32 = EVENT_IDX;
    const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;
    const VRING_USED_F_NO_NOTIFY: u16 = 1;
    fn rd16(m: &[u8], a: u64) -> u16 {
        u16::from_le_bytes(m[a as usize..a as usize + 2].try_into().unwrap())
    }
    fn rd32(m: &[u8], a: u64) -> u32 {
        u32::from_le_bytes(m[a as usize..a as usize + 4].try_into().unwrap())
    }
    fn wr16(m: &mut [u8], a: u64, v: u16) {
        m[a as usize..a as usize + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn wr32(m: &mut [u8], a: u64, v: u32) {
        m[a as usize..a as usize + 4].copy_from_slice(&v.to_le_bytes());
    }

    const DESC: u64 = 0x100;
    const AVAIL: u64 = 0x200;
    const USED: u64 = 0x300;

    struct Guest {
        net: VirtioNet,
        m: Vec<u8>,
        queue: usize,
    }

    impl Guest {
        fn new(queue: usize) -> Self {
            let mut g = Self {
                net: VirtioNet::new([2, 0, 0, 0, 0, 1]),
                m: vec![0; 4096],
                queue,
            };
            for (off, value) in [
                (0x030, queue as u32),
                (0x038, 2),
                (0x080, DESC as u32),
                (0x090, AVAIL as u32),
                (0x0a0, USED as u32),
                (0x044, 1),
            ] {
                g.net.write(&mut g.m, off, value).unwrap();
            }
            g
        }
        fn post(&mut self, bytes: &[u8]) {
            let idx = rd16(&self.m, AVAIL + 2);
            let head = idx % 2;
            let d = DESC + 16 * head as u64;
            let a = 0x400 + 128 * head as u64;
            self.m[d as usize..d as usize + 8].copy_from_slice(&a.to_le_bytes());
            let len = if self.queue == 0 {
                128
            } else {
                bytes.len() as u32
            };
            wr32(&mut self.m, d + 8, len);
            wr16(&mut self.m, d + 12, if self.queue == 0 { 2 } else { 0 });
            self.m[a as usize..a as usize + bytes.len()].copy_from_slice(bytes);
            wr16(&mut self.m, AVAIL + 4 + 2 * head as u64, head);
            wr16(&mut self.m, AVAIL + 2, idx.wrapping_add(1));
        }
        fn notify(&mut self) -> WriteOutcome {
            self.net
                .write(&mut self.m, 0x050, self.queue as u32)
                .unwrap()
        }
    }

    #[test]
    fn tx_interrupt_uses_current_avail_flags_and_keeps_completions() {
        // Only NO_INTERRUPT matters; toggling it after an ACK must be sampled
        // again at the next notification, rather than cached on queue setup.
        for flags in [0, VRING_AVAIL_F_NO_INTERRUPT, 2, 3] {
            let mut g = Guest::new(1);
            for current in [flags, flags ^ VRING_AVAIL_F_NO_INTERRUPT] {
                g.net.write(&mut g.m, 0x064, 1).unwrap();
                wr16(&mut g.m, AVAIL, current);
                let mut packet = vec![0; HDR];
                packet.extend_from_slice(b"frame");
                g.post(&packet);
                let out = g.notify();
                let irq = current & VRING_AVAIL_F_NO_INTERRUPT == 0;
                assert_eq!(out.irq, irq);
                assert_eq!(g.net.read(0x060), u32::from(irq));
                assert_eq!(out.frames, [b"frame".to_vec()]);
                assert_eq!(rd16(&g.m, USED + 2) as u64, g.net.stats().tx_frames);
                assert_eq!(rd16(&g.m, USED), 0);
            }
        }
    }

    #[test]
    fn rx_interrupt_suppression_preserves_direct_and_backlogged_acceptance() {
        for backlogged in [false, true] {
            for flags in [0, VRING_AVAIL_F_NO_INTERRUPT, 2, 3] {
                let mut g = Guest::new(0);
                for current in [flags, flags ^ VRING_AVAIL_F_NO_INTERRUPT] {
                    g.net.write(&mut g.m, 0x064, 1).unwrap();
                    wr16(&mut g.m, AVAIL, current);
                    let irq = current & VRING_AVAIL_F_NO_INTERRUPT == 0;
                    if backlogged {
                        assert_eq!(
                            g.net.receive(&mut g.m, b"frame").unwrap(),
                            ReceiveOutcome {
                                accepted: true,
                                irq: false
                            }
                        );
                        g.post(&[]);
                        assert_eq!(g.notify().irq, irq);
                    } else {
                        g.post(&[]);
                        assert_eq!(
                            g.net.receive(&mut g.m, b"frame").unwrap(),
                            ReceiveOutcome {
                                accepted: true,
                                irq
                            }
                        );
                    }
                    assert_eq!(g.net.read(0x060), u32::from(irq));
                    assert_eq!(rd16(&g.m, USED + 2) as u64, g.net.stats().rx_frames);
                    assert_eq!(g.net.stats().rx_dropped, 0);
                    assert!(g.net.model.projection()["backlog"]
                        .as_array()
                        .unwrap()
                        .is_empty());
                    assert_eq!(rd16(&g.m, USED), 0);
                }
            }
        }
    }

    #[test]
    fn legacy_tx_drain_observes_suppression_and_rechecks_after_clearing_it() {
        let mut g = Guest::new(1);
        wr16(&mut g.m, USED, 2); // Preserve unrelated flag bits.
        let mut first = vec![0; HDR];
        first.extend_from_slice(b"first");
        g.post(&first);
        let memory = g.m.as_mut_ptr();
        let mut observed_flags = Vec::new();
        let mut injected = false;
        let out = g
            .net
            .model
            .notify(&mut g.net.transport, &mut g.m, 0, 1, |address, len| {
                if address == USED as usize && len == 2 {
                    // The callback runs immediately after the guest-visible flag write.
                    let flags = unsafe {
                        u16::from_le_bytes([
                            *memory.add(USED as usize),
                            *memory.add(USED as usize + 1),
                        ])
                    };
                    observed_flags.push(flags);
                    if flags == 2 && !injected {
                        // Publish a fresh request exactly as the drain clears NO_NOTIFY,
                        // before arm_notify rechecks avail->idx.
                        let d = (DESC + 16) as usize;
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                0x480u64.to_le_bytes().as_ptr(),
                                memory.add(d),
                                8,
                            );
                            std::ptr::copy_nonoverlapping(
                                (HDR as u32 + 5).to_le_bytes().as_ptr(),
                                memory.add(d + 8),
                                4,
                            );
                            std::ptr::copy_nonoverlapping(
                                0u16.to_le_bytes().as_ptr(),
                                memory.add(d + 12),
                                2,
                            );
                            std::ptr::copy_nonoverlapping(
                                1u16.to_le_bytes().as_ptr(),
                                memory.add((AVAIL + 6) as usize),
                                2,
                            );
                            std::ptr::copy_nonoverlapping(
                                2u16.to_le_bytes().as_ptr(),
                                memory.add((AVAIL + 2) as usize),
                                2,
                            );
                            std::ptr::copy_nonoverlapping(b"second".as_ptr(), memory.add(0x48c), 6);
                        }
                        injected = true;
                    }
                }
                g.net.dirty.push((address, len));
            })
            .unwrap();
        assert_eq!(out.frames, [b"first".to_vec(), b"secon".to_vec()]);
        assert!(out.irq);
        assert!(injected);
        assert_eq!(observed_flags, [3, 2, 3, 2]);
        assert_eq!(rd16(&g.m, USED), 2);
        assert_eq!(rd16(&g.m, USED + 2), 2);
        assert_eq!(g.net.stats().tx_frames, 2);
        assert_eq!(
            g.net
                .dirty
                .iter()
                .filter(|&&(a, n)| a == USED as usize && n == 2)
                .count(),
            4
        );
    }

    #[test]
    fn kvm_rx_truncates_to_posted_buffer_and_completes_the_prefix() {
        for (capacity, expected_len) in [(0, 0), (11, 11), (12, 12), (1530, 1530), (1630, 1612)] {
            for (flags, expected_irq) in [(0, true), (VRING_AVAIL_F_NO_INTERRUPT, false)] {
                let mut g = Guest::new(0);
                g.post(&[]);
                wr32(&mut g.m, DESC + 8, capacity as u32);
                wr16(&mut g.m, AVAIL, flags);
                let frame = vec![0x5a; 1600];
                let received = g.net.receive(&mut g.m, &frame).unwrap();

                assert_eq!(
                    received,
                    ReceiveOutcome {
                        accepted: true,
                        irq: expected_irq
                    }
                );
                assert_eq!(rd32(&g.m, USED + 8), expected_len as u32);
                assert_eq!(rd16(&g.m, USED + 2), 1);
                let mut packet = vec![0; HDR];
                packet[10] = 1;
                packet.extend_from_slice(&frame);
                assert_eq!(&g.m[0x400..0x400 + expected_len], &packet[..expected_len]);
                assert_eq!(
                    g.net.dirty,
                    [
                        (0x400, expected_len),
                        (USED as usize + 4, 8),
                        (USED as usize + 2, 2)
                    ]
                );
                assert_eq!(g.net.stats().rx_frames, 1);
                assert!(g.net.model.projection()["backlog"]
                    .as_array()
                    .unwrap()
                    .is_empty());
                assert_eq!(g.net.read(0x060), u32::from(expected_irq));
            }
        }
    }

    #[test]
    fn rx_drain_clears_and_journals_no_notify_when_frames_or_buffers_run_out() {
        for (frames, buffers) in [(2, 1), (1, 2)] {
            let mut g = Guest::new(0);
            wr16(&mut g.m, USED, 2);
            for frame in [b"first".as_slice(), b"second".as_slice()]
                .into_iter()
                .take(frames)
            {
                assert_eq!(
                    g.net.receive(&mut g.m, frame).unwrap(),
                    ReceiveOutcome {
                        accepted: true,
                        irq: false
                    }
                );
            }
            for _ in 0..buffers {
                g.post(&[]);
            }
            assert!(g.notify().irq);
            assert_eq!(g.net.stats().rx_frames, frames.min(buffers) as u64);
            assert_eq!(
                g.net.model.projection()["backlog"]
                    .as_array()
                    .unwrap()
                    .len(),
                frames.saturating_sub(buffers)
            );
            assert_eq!(
                g.net.transport.queues[0].pending(&g.m, 0).unwrap(),
                buffers > frames
            );
            assert_eq!(&g.m[0x400 + HDR..0x400 + HDR + 5], b"first");
            assert_eq!(rd16(&g.m, USED), 2);
            assert_eq!(g.net.dirty.first(), Some(&(USED as usize, 2)));
            assert_eq!(g.net.dirty.last(), Some(&(USED as usize, 2)));
            assert_eq!(
                g.net
                    .dirty
                    .iter()
                    .filter(|&&(a, _)| a == USED as usize)
                    .count(),
                2
            );
        }
    }

    #[test]
    fn unready_notifications_do_not_access_guest_rings() {
        let mut net = VirtioNet::new([2, 0, 0, 0, 0, 1]);
        for queue in [0, 1] {
            let out = net.write(&mut [], 0x050, queue).unwrap();
            assert!(!out.irq);
            assert!(out.frames.is_empty());
        }
        assert!(net.dirty.is_empty());
    }

    fn negotiate(g: &mut Guest) {
        g.net
            .write(&mut g.m, 0x020, (1 << 5) | VIRTIO_RING_F_EVENT_IDX)
            .unwrap();
        g.net.write(&mut g.m, 0x024, 1).unwrap();
        g.net.write(&mut g.m, 0x020, 1).unwrap();
        g.net.write(&mut g.m, 0x070, 15).unwrap();
    }

    #[test]
    fn event_idx_is_offered_negotiated_and_reset() {
        let mut g = Guest::new(1);
        assert_eq!(g.net.read(0x010), (1 << 5) | VIRTIO_RING_F_EVENT_IDX);
        g.net.write(&mut g.m, 0x014, 1).unwrap();
        assert_eq!(g.net.read(0x010), 1);
        g.net
            .write(&mut g.m, 0x020, VIRTIO_RING_F_EVENT_IDX)
            .unwrap();
        assert!(!g.net.event_idx()); // FEATURES_OK has not been accepted yet.
        negotiate(&mut g);
        assert!(g.net.event_idx());
        wr16(&mut g.m, USED, VRING_USED_F_NO_NOTIFY);
        wr16(&mut g.m, USED + 20, 42);
        g.net.write(&mut g.m, 0x044, 1).unwrap();
        assert_eq!(rd16(&g.m, USED), 0);
        assert_eq!(rd16(&g.m, USED + 20), 0);
        assert!(g.net.dirty.contains(&(USED as usize + 20, 2)));
        g.net.write(&mut g.m, 0x070, 0).unwrap();
        assert!(!g.net.event_idx());
        assert!(!g.net.poll(&mut []).unwrap().irq);
    }

    #[test]
    fn need_event_matches_crossed_entries_including_wrap() {
        for old in [0u16, 1, 32768, 65534, 65535] {
            for count in 0u16..=QSIZE {
                let new = old.wrapping_add(count);
                for offset in 0u16..=QSIZE {
                    assert_eq!(
                        vring_need_event(old.wrapping_add(offset), new, old),
                        offset < count
                    );
                }
                assert!(!vring_need_event(old.wrapping_sub(1), new, old));
            }
        }
    }

    #[test]
    fn avail_event_follows_full_used_ring_and_is_journaled() {
        for num in [1, 2, QSIZE] {
            let mut m = vec![0xa5; 8192];
            let mut q = Queue {
                num,
                ready: true,
                avail: 0x100,
                used: 0x800,
                ..Default::default()
            };
            let a = q.used + 4 + 8 * num as u64;
            let mut dirty = Vec::new();
            for consumed in [65535, 0, 1] {
                q.last_avail = consumed;
                wr16(&mut m, q.avail + 2, consumed);
                assert!(!q
                    .arm_notify(&mut m, 0, true, false, |a, n| dirty.push((a, n)))
                    .unwrap());
                assert_eq!(rd16(&m, a), consumed);
                assert_eq!(dirty.pop(), Some((a as usize, 2)));
                assert_eq!(rd16(&m, a - 2), 0xa5a5);
                assert_eq!(rd16(&m, a + 2), 0xa5a5);
                assert!(!q
                    .arm_notify(&mut m, 0, true, false, |a, n| dirty.push((a, n)))
                    .unwrap());
                assert!(dirty.is_empty());
                wr16(&mut m, q.avail + 2, consumed.wrapping_add(1));
                assert!(q
                    .arm_notify(&mut m, 0, true, false, |a, n| dirty.push((a, n)))
                    .unwrap());
                assert!(vring_need_event(
                    rd16(&m, a),
                    consumed.wrapping_add(1),
                    consumed
                ));
            }
        }
    }

    #[test]
    fn event_idx_interrupts_use_live_used_event_and_ignore_avail_flags() {
        for queue in [0, 1] {
            for backlogged in [false, true] {
                if queue == 1 && backlogged {
                    continue;
                }
                for old in [0u16, 65535] {
                    for (event, irq) in [
                        (old, true),
                        (old.wrapping_add(1), false),
                        (old.wrapping_sub(1), false),
                    ] {
                        let mut g = Guest::new(queue);
                        negotiate(&mut g);
                        g.net.transport.queues[queue].used_idx = old;
                        wr16(&mut g.m, AVAIL, VRING_AVAIL_F_NO_INTERRUPT);
                        wr16(&mut g.m, AVAIL + 4 + 2 * 2, event);
                        let received = if queue == 1 {
                            let mut packet = vec![0; HDR];
                            packet.extend_from_slice(b"frame");
                            g.post(&packet);
                            let out = g.notify();
                            assert_eq!(out.frames, [b"frame".to_vec()]);
                            out.irq
                        } else if backlogged {
                            assert!(g.net.receive(&mut g.m, b"frame").unwrap().accepted);
                            g.post(&[]);
                            g.notify().irq
                        } else {
                            g.post(&[]);
                            let out = g.net.receive(&mut g.m, b"frame").unwrap();
                            assert!(out.accepted);
                            out.irq
                        };
                        assert_eq!(received, irq);
                        assert_eq!(g.net.read(0x060), u32::from(irq));
                        assert_eq!(rd16(&g.m, USED + 2), old.wrapping_add(1));
                        assert_eq!(rd16(&g.m, USED + 4 + 8 * 2), 1);
                        assert_eq!(g.net.stats().suppressed_interrupts, u64::from(!irq));
                        assert_eq!(rd16(&g.m, USED), 0);
                        // Acknowledge, change the threshold and complete again.
                        g.net.write(&mut g.m, 0x064, 1).unwrap();
                        wr16(&mut g.m, AVAIL + 4 + 2 * 2, old.wrapping_add(1));
                        if queue == 1 {
                            g.post(&[0; HDR]);
                            assert!(g.notify().irq);
                        } else {
                            g.post(&[]);
                            assert!(g.net.receive(&mut g.m, b"next").unwrap().irq);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn event_idx_batch_interrupt_covers_every_completed_entry() {
        for queue in [0, 1] {
            for event in [0, 1, 2, 65535] {
                let mut g = Guest::new(queue);
                negotiate(&mut g);
                wr16(&mut g.m, AVAIL + 8, event);
                if queue == 0 {
                    for _ in 0..2 {
                        assert!(g.net.receive(&mut g.m, b"frame").unwrap().accepted);
                    }
                }
                for _ in 0..2 {
                    g.post(&[0; HDR + 5]);
                }
                assert_eq!(g.notify().irq, event < 2);
                assert_eq!(rd16(&g.m, USED + 2), 2);
                assert_eq!(rd16(&g.m, USED + 20), 2);
            }
        }
    }

    #[test]
    fn deterministic_poll_services_tx_and_rx_without_doorbells_only_when_negotiated() {
        for event_idx in [false, true] {
            let mut tx = Guest::new(1);
            let mut rx = Guest::new(0);
            if event_idx {
                negotiate(&mut tx);
                negotiate(&mut rx);
            }
            assert!(rx.net.receive(&mut rx.m, b"frame").unwrap().accepted);
            let mut packet = vec![0; HDR];
            packet.extend_from_slice(b"frame");
            tx.post(&packet);
            rx.post(&[]);
            let out = tx.net.poll(&mut tx.m).unwrap();
            assert_eq!(out.irq, event_idx);
            assert_eq!(out.frames.len(), usize::from(event_idx));
            assert_eq!(rx.net.poll(&mut rx.m).unwrap().irq, event_idx);
            assert_eq!(tx.net.stats().tx_frames, u64::from(event_idx));
            assert_eq!(rx.net.stats().rx_frames, u64::from(event_idx));
            assert_eq!(tx.net.stats().doorbell_exits, 0);
            assert_eq!(rx.net.stats().doorbell_exits, 0);
            assert!(!tx.net.poll(&mut tx.m).unwrap().irq);
            assert!(!rx.net.poll(&mut rx.m).unwrap().irq);
            if !event_idx {
                assert!(tx.notify().irq);
                assert!(rx.notify().irq);
            }
        }
        let mut g = Guest::new(1);
        negotiate(&mut g);
        g.net.transport.queues[1].ready = false;
        assert!(g.net.poll(&mut []).unwrap().frames.is_empty());
        assert!(!g.net.poll(&mut []).unwrap().irq);
    }
    #[test]
    fn deferred_tx_suppresses_lone_and_full_ring_kicks_across_wrap() {
        for event_idx in [false, true] {
            let mut g = Guest::new(1);
            if event_idx {
                negotiate(&mut g);
            }
            g.net.set_deferred_tx(true);
            // Test boot-time arming as well as re-arming after drains.
            g.net.write(&mut g.m, 0x044, 1).unwrap();
            for start in [0, u16::MAX - 1, u16::MAX] {
                g.net.transport.queues[1].last_avail = start;
                g.net.transport.queues[1].used_idx = start;
                wr16(&mut g.m, AVAIL + 2, start);
                g.net.poll_at(&mut g.m, 100, false).unwrap();
                let mut packet = vec![0; HDR];
                packet.extend_from_slice(b"frame");
                for n in 1..=2 {
                    g.post(&packet);
                    if event_idx {
                        let event = rd16(&g.m, USED + 4 + 8 * 2);
                        assert!(!vring_need_event(event, start.wrapping_add(n), start));
                    } else {
                        assert_ne!(rd16(&g.m, USED) & VRING_USED_F_NO_NOTIFY, 0);
                    }
                }
                let out = g.net.poll_at(&mut g.m, 250, true).unwrap();
                assert_eq!(out.frames, [b"frame".to_vec(), b"frame".to_vec()]);
                assert_eq!(rd16(&g.m, USED + 2), start.wrapping_add(2));
                assert!(!g.net.pending_tx(&g.m).unwrap());
                if event_idx {
                    assert_eq!(rd16(&g.m, USED + 4 + 8 * 2), start.wrapping_add(4));
                } else {
                    assert_ne!(rd16(&g.m, USED) & VRING_USED_F_NO_NOTIFY, 0);
                }
            }
            assert_eq!(g.net.stats().doorbell_exits, 0);
            assert_eq!(g.net.stats().tx_polled_at_boundary, 6);
            assert_eq!(g.net.stats().tx_pickup_samples, 6);
            assert_eq!(g.net.stats().tx_pickup_delay_upper_ns_sum, 900);
            assert_eq!(g.net.stats().tx_pickup_delay_upper_ns_max, 150);
            g.net.write(&mut g.m, 0x070, 0).unwrap();
            assert!(
                g.net.deferred_tx(),
                "device reset must retain the run's TX mode"
            );
        }
    }

    #[test]
    fn deferred_lone_tx_stays_suppressed_after_each_empty_poll() {
        for event_idx in [false, true] {
            let mut g = Guest::new(1);
            if event_idx {
                negotiate(&mut g);
            }
            g.net.set_deferred_tx(true);
            for n in 0..10 {
                g.net.poll_at(&mut g.m, n * 100, false).unwrap();
                let old = rd16(&g.m, AVAIL + 2);
                let mut packet = vec![0; HDR];
                packet.extend_from_slice(b"frame");
                g.post(&packet);
                if event_idx {
                    assert!(!vring_need_event(
                        rd16(&g.m, USED + 4 + 8 * 2),
                        old.wrapping_add(1),
                        old
                    ));
                } else {
                    assert_ne!(rd16(&g.m, USED) & VRING_USED_F_NO_NOTIFY, 0);
                }
                assert_eq!(
                    g.net.poll_at(&mut g.m, n * 100 + 50, true).unwrap().frames,
                    [b"frame".to_vec()]
                );
            }
            assert_eq!(g.net.stats().doorbell_exits, 0);
            assert_eq!(g.net.stats().tx_polled_at_boundary, 10);
            assert_eq!(g.net.stats().tx_pickup_delay_upper_ns_sum, 500);
            assert_eq!(g.net.stats().tx_pickup_delay_upper_ns_max, 50);
        }
    }

    #[test]
    fn deferred_legacy_tx_does_not_change_rx_polling_or_notification_flags() {
        let mut g = Guest::new(0);
        g.net.set_deferred_tx(true);
        assert!(g.net.receive(&mut g.m, b"frame").unwrap().accepted);
        g.post(&[]);
        assert!(!g.net.poll_at(&mut g.m, 100, true).unwrap().irq);
        assert_eq!(g.net.stats().rx_frames, 0);
        assert_eq!(rd16(&g.m, USED), 0);
        assert!(g.notify().irq);
        assert_eq!(g.net.stats().rx_frames, 1);
        assert_eq!(rd16(&g.m, USED), 0);
    }

    #[test]
    fn pickup_diagnostics_do_not_change_off_mode_dma_frames_or_interrupts() {
        for event_idx in [false, true] {
            let mut g = Guest::new(1);
            if event_idx {
                negotiate(&mut g);
            }
            for (off, value) in [(0x050, 1), (0x064, 1), (0x050, 1), (0x070, 0)] {
                let mut packet = vec![0; HDR];
                packet.extend_from_slice(b"frame");
                g.post(&packet);
                let mut raw = g.net.clone();
                let mut memory = g.m.clone();
                let expected = raw.write(&mut memory, off, value).unwrap();
                let observed = g.net.write_at(&mut g.m, off, value, 100).unwrap();
                assert_eq!(
                    (observed.irq, observed.frames),
                    (expected.irq, expected.frames)
                );
                assert_eq!(g.m, memory);
                assert_eq!(g.net.dirty, raw.dirty);
                assert_eq!(g.net.transport.isr, raw.transport.isr);
                assert_eq!(g.net.stats().tx_frames, raw.stats().tx_frames);
            }
        }
    }

    #[test]
    fn deferred_poll_bound_is_semantic_and_off_keeps_the_original_limit() {
        assert_eq!(poll_limit(100, 1_000_000, false), 1_000_000);
        assert_eq!(
            poll_limit(100, 1_000_000, true),
            100 + seedbox_engine_core::virtio_network::TX_POLL_NS
        );
        assert_eq!(poll_limit(100, 500, true), 500);
        assert_eq!(poll_limit(u64::MAX - 1, u64::MAX, true), u64::MAX);
    }
}
