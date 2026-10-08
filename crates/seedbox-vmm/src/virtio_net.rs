//! virtio-net over virtio-mmio (version 2), split virtqueues, no offloads.
//! Queue 0 receives, queue 1 transmits. All DMA happens on the world thread
//! while the guest is stopped.

use std::collections::VecDeque;

const HDR: usize = 12; // virtio_net_hdr with num_buffers (VERSION_1)
const QSIZE: u16 = 256;
const BACKLOG: usize = 256;
const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;
const VRING_USED_F_NO_NOTIFY: u16 = 1;

#[derive(Default, Clone)]
struct Queue {
    num: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail: u16,
    used_idx: u16,
}

/// Receiver acceptance and IRQ notification are independent. A frame queued
/// in the bounded device backlog is accepted without an immediate interrupt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ReceiveOutcome {
    pub accepted: bool,
    pub irq: bool,
}

pub struct WriteOutcome {
    pub irq: bool,
    pub frames: Vec<Vec<u8>>,
}

#[derive(Clone)]
pub struct VirtioNet {
    mac: [u8; 6],
    dev_sel: u32,
    drv_sel: u32,
    drv_features: [u32; 2],
    queue_sel: u32,
    q: [Queue; 2],
    status: u32,
    isr: u32,
    backlog: VecDeque<Vec<u8>>,
    pub tx_frames: u64,
    pub rx_frames: u64,
    pub rx_dropped: u64,
    /// Guest memory ranges (start, len) written by the device since last taken.
    pub dirty: Vec<(usize, usize)>,
}

fn rd16(m: &[u8], a: u64) -> u16 {
    let a = a as usize;
    u16::from_le_bytes(m[a..a + 2].try_into().unwrap())
}
fn rd32(m: &[u8], a: u64) -> u32 {
    let a = a as usize;
    u32::from_le_bytes(m[a..a + 4].try_into().unwrap())
}
fn rd64(m: &[u8], a: u64) -> u64 {
    let a = a as usize;
    u64::from_le_bytes(m[a..a + 8].try_into().unwrap())
}
fn wr16(m: &mut [u8], a: u64, v: u16) {
    let a = a as usize;
    m[a..a + 2].copy_from_slice(&v.to_le_bytes());
}
fn wr32(m: &mut [u8], a: u64, v: u32) {
    let a = a as usize;
    m[a..a + 4].copy_from_slice(&v.to_le_bytes());
}

struct Desc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

impl Queue {
    fn desc(&self, m: &[u8], i: u16) -> Desc {
        let a = self.desc + 16 * i as u64;
        Desc {
            addr: rd64(m, a),
            len: rd32(m, a + 8),
            flags: rd16(m, a + 12),
            next: rd16(m, a + 14),
        }
    }
    fn pending(&self, m: &[u8]) -> bool {
        self.ready && rd16(m, self.avail + 2) != self.last_avail
    }
    fn interrupt(&self, m: &[u8]) -> bool {
        rd16(m, self.avail) & VRING_AVAIL_F_NO_INTERRUPT == 0
    }
    fn suppress_notify(&self, m: &mut [u8], dirty: &mut Vec<(usize, usize)>, suppress: bool) {
        let flags = rd16(m, self.used);
        let next = if suppress { flags | VRING_USED_F_NO_NOTIFY } else { flags & !VRING_USED_F_NO_NOTIFY };
        if next != flags {
            wr16(m, self.used, next);
            dirty.push((self.used as usize, 2));
        }
    }
    fn finish_drain(&self, m: &mut [u8], dirty: &mut Vec<(usize, usize)>) -> bool {
        // Re-enable kicks before re-reading avail->idx. The guest is stopped,
        // so both operations occur at this deterministic device-service point.
        self.suppress_notify(m, dirty, false);
        self.pending(m)
    }
    fn pop(&mut self, m: &[u8]) -> Option<u16> {
        if !self.pending(m) {
            return None;
        }
        let head = rd16(m, self.avail + 4 + 2 * (self.last_avail % self.num) as u64);
        self.last_avail = self.last_avail.wrapping_add(1);
        Some(head)
    }
    fn push_used(&mut self, m: &mut [u8], dirty: &mut Vec<(usize, usize)>, head: u16, len: u32) {
        let e = self.used + 4 + 8 * (self.used_idx % self.num) as u64;
        wr32(m, e, head as u32);
        wr32(m, e + 4, len);
        self.used_idx = self.used_idx.wrapping_add(1);
        wr16(m, self.used + 2, self.used_idx);
        dirty.push((e as usize, 8));
        dirty.push((self.used as usize + 2, 2));
    }
}

impl VirtioNet {
    pub fn new(mac: [u8; 6]) -> Self {
        Self {
            mac,
            dev_sel: 0,
            drv_sel: 0,
            drv_features: [0; 2],
            queue_sel: 0,
            q: Default::default(),
            status: 0,
            isr: 0,
            backlog: VecDeque::new(),
            tx_frames: 0,
            rx_frames: 0,
            rx_dropped: 0,
            dirty: Vec::new(),
        }
    }

    fn config(&self, off: u64) -> u8 {
        match off {
            0..=5 => self.mac[off as usize],
            _ => 0,
        }
    }

    pub fn read(&mut self, off: u64) -> u32 {
        let qs = self.queue_sel as usize;
        match off {
            0x000 => 0x7472_6976,
            0x004 => 2,
            0x008 => 1,
            0x00c => 0x5345_4544,
            0x010 => match self.dev_sel {
                0 => 1 << 5, // VIRTIO_NET_F_MAC
                1 => 1,      // VIRTIO_F_VERSION_1
                _ => 0,
            },
            0x034 => {
                if qs < 2 {
                    QSIZE as u32
                } else {
                    0
                }
            }
            0x044 => {
                if qs < 2 {
                    self.q[qs].ready as u32
                } else {
                    0
                }
            }
            0x060 => self.isr,
            0x070 => self.status,
            0x0fc => 0,
            o if o >= 0x100 => {
                let b = o - 0x100;
                u32::from_le_bytes([
                    self.config(b),
                    self.config(b + 1),
                    self.config(b + 2),
                    self.config(b + 3),
                ])
            }
            _ => 0,
        }
    }

    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> WriteOutcome {
        let mut out = WriteOutcome {
            irq: false,
            frames: Vec::new(),
        };
        let qs = (self.queue_sel as usize).min(1);
        match off {
            0x014 => self.dev_sel = v,
            0x020 => {
                if self.drv_sel < 2 {
                    self.drv_features[self.drv_sel as usize] = v;
                }
            }
            0x024 => self.drv_sel = v,
            0x030 => self.queue_sel = v,
            0x038 => self.q[qs].num = (v as u16).min(QSIZE).max(1),
            0x044 => {
                let q = &mut self.q[qs];
                q.ready = v == 1;
                q.last_avail = 0;
                q.used_idx = 0;
            }
            0x050 => match v {
                1 => {
                    while self.q[1].pending(m) {
                        self.q[1].suppress_notify(m, &mut self.dirty, true);
                        while let Some(head) = self.q[1].pop(m) {
                            let mut bytes = Vec::new();
                            let mut i = head;
                            loop {
                                let d = self.q[1].desc(m, i);
                                let a = d.addr as usize;
                                bytes.extend_from_slice(&m[a..a + d.len as usize]);
                                if d.flags & 1 == 0 {
                                    break;
                                }
                                i = d.next;
                            }
                            self.q[1].push_used(m, &mut self.dirty, head, 0);
                            if bytes.len() > HDR {
                                out.frames.push(bytes[HDR..].to_vec());
                                self.tx_frames += 1;
                            }
                            out.irq = true;
                        }
                        if !self.q[1].finish_drain(m, &mut self.dirty) {
                            break;
                        }
                    }
                    out.irq = out.irq && self.q[1].interrupt(m);
                }
                0 => out.irq = self.drain(m),
                _ => {}
            },
            0x064 => self.isr &= !v,
            0x070 => {
                self.status = v;
                if v == 0 {
                    let dirty = std::mem::take(&mut self.dirty);
                    *self = Self::new(self.mac);
                    self.dirty = dirty;
                }
            }
            0x080 => self.q[qs].desc = (self.q[qs].desc & !0xffff_ffff) | v as u64,
            0x084 => self.q[qs].desc = (self.q[qs].desc & 0xffff_ffff) | (v as u64) << 32,
            0x090 => self.q[qs].avail = (self.q[qs].avail & !0xffff_ffff) | v as u64,
            0x094 => self.q[qs].avail = (self.q[qs].avail & 0xffff_ffff) | (v as u64) << 32,
            0x0a0 => self.q[qs].used = (self.q[qs].used & !0xffff_ffff) | v as u64,
            0x0a4 => self.q[qs].used = (self.q[qs].used & 0xffff_ffff) | (v as u64) << 32,
            _ => {}
        }
        if out.irq {
            self.isr |= 1;
        }
        out
    }

    fn put_frame(&mut self, m: &mut [u8], frame: &[u8]) -> bool {
        let Some(head) = self.q[0].pop(m) else {
            return false;
        };
        let mut data = vec![0u8; HDR];
        data[10] = 1; // num_buffers
        data.extend_from_slice(frame);
        let mut written = 0usize;
        let mut i = head;
        loop {
            let d = self.q[0].desc(m, i);
            if d.flags & 2 != 0 {
                let n = (d.len as usize).min(data.len() - written);
                let a = d.addr as usize;
                m[a..a + n].copy_from_slice(&data[written..written + n]);
                self.dirty.push((a, n));
                written += n;
            }
            if d.flags & 1 == 0 || written == data.len() {
                break;
            }
            i = d.next;
        }
        self.q[0].push_used(m, &mut self.dirty, head, written as u32);
        self.rx_frames += 1;
        true
    }

    fn drain(&mut self, m: &mut [u8]) -> bool {
        if self.backlog.is_empty() || !self.q[0].pending(m) {
            return false;
        }
        let mut any = false;
        loop {
            self.q[0].suppress_notify(m, &mut self.dirty, true);
            while let Some(f) = self.backlog.front() {
                let f = f.clone();
                if !self.put_frame(m, &f) {
                    break;
                }
                self.backlog.pop_front();
                any = true;
            }
            let pending = self.q[0].finish_drain(m, &mut self.dirty);
            if self.backlog.is_empty() || !pending {
                break;
            }
        }
        let irq = any && self.q[0].interrupt(m);
        if irq {
            self.isr |= 1;
        }
        irq
    }

    /// Deliver an inbound frame now, reporting acceptance separately from IRQ.
    pub fn receive(&mut self, m: &mut [u8], frame: &[u8]) -> ReceiveOutcome {
        if !self.q[0].ready {
            self.rx_dropped += 1;
            return ReceiveOutcome::default();
        }
        if !self.backlog.is_empty() || !self.put_frame(m, frame) {
            if self.backlog.len() >= BACKLOG {
                self.rx_dropped += 1;
                return ReceiveOutcome::default();
            }
            self.backlog.push_back(frame.to_vec());
            return ReceiveOutcome {
                accepted: true,
                irq: false,
            };
        }
        let irq = self.q[0].interrupt(m);
        if irq {
            self.isr |= 1;
        }
        ReceiveOutcome {
            accepted: true,
            irq,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let mut g = Self { net: VirtioNet::new([2, 0, 0, 0, 0, 1]), m: vec![0; 4096], queue };
            for (off, value) in [(0x030, queue as u32), (0x038, 2), (0x080, DESC as u32), (0x090, AVAIL as u32), (0x0a0, USED as u32), (0x044, 1)] {
                g.net.write(&mut g.m, off, value);
            }
            g
        }
        fn post(&mut self, bytes: &[u8]) {
            let idx = rd16(&self.m, AVAIL + 2);
            let head = idx % 2;
            let d = DESC + 16 * head as u64;
            let a = 0x400 + 128 * head as u64;
            self.m[d as usize..d as usize + 8].copy_from_slice(&a.to_le_bytes());
            let len = if self.queue == 0 { 128 } else { bytes.len() as u32 };
            wr32(&mut self.m, d + 8, len);
            wr16(&mut self.m, d + 12, if self.queue == 0 { 2 } else { 0 });
            self.m[a as usize..a as usize + bytes.len()].copy_from_slice(bytes);
            wr16(&mut self.m, AVAIL + 4 + 2 * head as u64, head);
            wr16(&mut self.m, AVAIL + 2, idx.wrapping_add(1));
        }
        fn notify(&mut self) -> WriteOutcome {
            self.net.write(&mut self.m, 0x050, self.queue as u32)
        }
    }

    #[test]
    fn tx_interrupt_uses_current_avail_flags_and_keeps_completions() {
        // Only NO_INTERRUPT matters; toggling it after an ACK must be sampled
        // again at the next notification, rather than cached on queue setup.
        for flags in [0, VRING_AVAIL_F_NO_INTERRUPT, 2, 3] {
            let mut g = Guest::new(1);
            for current in [flags, flags ^ VRING_AVAIL_F_NO_INTERRUPT] {
                g.net.write(&mut g.m, 0x064, 1);
                wr16(&mut g.m, AVAIL, current);
                let mut packet = vec![0; HDR];
                packet.extend_from_slice(b"frame");
                g.post(&packet);
                let out = g.notify();
                let irq = current & VRING_AVAIL_F_NO_INTERRUPT == 0;
                assert_eq!(out.irq, irq);
                assert_eq!(g.net.read(0x060), u32::from(irq));
                assert_eq!(out.frames, [b"frame".to_vec()]);
                assert_eq!(rd16(&g.m, USED + 2) as u64, g.net.tx_frames);
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
                    g.net.write(&mut g.m, 0x064, 1);
                    wr16(&mut g.m, AVAIL, current);
                    let irq = current & VRING_AVAIL_F_NO_INTERRUPT == 0;
                    if backlogged {
                        assert_eq!(g.net.receive(&mut g.m, b"frame"), ReceiveOutcome { accepted: true, irq: false });
                        g.post(&[]);
                        assert_eq!(g.notify().irq, irq);
                    } else {
                        g.post(&[]);
                        assert_eq!(g.net.receive(&mut g.m, b"frame"), ReceiveOutcome { accepted: true, irq });
                    }
                    assert_eq!(g.net.read(0x060), u32::from(irq));
                    assert_eq!(rd16(&g.m, USED + 2) as u64, g.net.rx_frames);
                    assert_eq!(g.net.rx_dropped, 0);
                    assert!(g.net.backlog.is_empty());
                    assert_eq!(rd16(&g.m, USED), 0);
                }
            }
        }
    }

    #[test]
    fn no_notify_is_set_during_drain_and_cleared_before_a_fresh_pending_check() {
        let mut g = Guest::new(1);
        wr16(&mut g.m, USED, 2); // Preserve unrelated flag bits.
        g.post(&[0; HDR]);
        g.net.q[1].suppress_notify(&mut g.m, &mut g.net.dirty, true);
        assert_eq!(rd16(&g.m, USED), 2 | VRING_USED_F_NO_NOTIFY);
        assert_eq!(g.net.q[1].pop(&g.m), Some(0));
        g.net.q[1].push_used(&mut g.m, &mut g.net.dirty, 0, 0);
        assert_eq!(rd16(&g.m, USED), 2 | VRING_USED_F_NO_NOTIFY);
        assert_eq!(g.net.q[1].pop(&g.m), None);
        // Model a newly published head between the empty observation and
        // re-enabling notifications, without introducing host concurrency.
        g.post(&[0; HDR]);
        assert!(g.net.q[1].finish_drain(&mut g.m, &mut g.net.dirty));
        assert_eq!(rd16(&g.m, USED), 2);
        assert_eq!(g.net.dirty, [(USED as usize, 2), (USED as usize + 4, 8), (USED as usize + 2, 2), (USED as usize, 2)]);
        g.net.q[1].suppress_notify(&mut g.m, &mut g.net.dirty, true);
        assert_eq!(g.net.q[1].pop(&g.m), Some(1));
        assert!(!g.net.q[1].finish_drain(&mut g.m, &mut g.net.dirty));
        assert_eq!(rd16(&g.m, USED), 2);
    }

    #[test]
    fn rx_drain_clears_and_journals_no_notify_when_frames_or_buffers_run_out() {
        for (frames, buffers) in [(2, 1), (1, 2)] {
            let mut g = Guest::new(0);
            wr16(&mut g.m, USED, 2);
            for frame in [b"first".as_slice(), b"second".as_slice()].into_iter().take(frames) {
                assert_eq!(g.net.receive(&mut g.m, frame), ReceiveOutcome { accepted: true, irq: false });
            }
            for _ in 0..buffers { g.post(&[]); }
            assert!(g.notify().irq);
            assert_eq!(g.net.rx_frames, frames.min(buffers) as u64);
            assert_eq!(g.net.backlog.len(), frames.saturating_sub(buffers));
            assert_eq!(g.net.q[0].pending(&g.m), buffers > frames);
            assert_eq!(&g.m[0x400 + HDR..0x400 + HDR + 5], b"first");
            assert_eq!(rd16(&g.m, USED), 2);
            assert_eq!(g.net.dirty.first(), Some(&(USED as usize, 2)));
            assert_eq!(g.net.dirty.last(), Some(&(USED as usize, 2)));
            assert_eq!(g.net.dirty.iter().filter(|&&(a, _)| a == USED as usize).count(), 2);
        }
    }

    #[test]
    fn unready_notifications_do_not_access_guest_rings() {
        let mut net = VirtioNet::new([2, 0, 0, 0, 0, 1]);
        for queue in [0, 1] {
            let out = net.write(&mut [], 0x050, queue);
            assert!(!out.irq);
            assert!(out.frames.is_empty());
        }
        assert!(net.dirty.is_empty());
    }
}
