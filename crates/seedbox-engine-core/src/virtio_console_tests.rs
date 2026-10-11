use super::*;
use crate::virtio_mmio::{Policy as MmioPolicy, Spec};

struct Guest {
    c: Console,
    t: Transport,
    m: Vec<u8>,
    base: u64,
    next: usize,
    avail: [u16; 4],
    dirty: Vec<(usize, usize)>,
}
impl Guest {
    fn new(policy: Policy) -> Self {
        let base = if policy == Policy::Application {
            0x4000_0000
        } else {
            0
        };
        let mut t = Transport::new(
            Spec {
                device_id: 3,
                features: [2, 1],
            },
            4,
            if policy == Policy::Application {
                MmioPolicy::Application
            } else {
                MmioPolicy::Console
            },
        );
        for (i, q) in t.queues.iter_mut().enumerate() {
            q.num = 8;
            q.ready = true;
            q.desc = base + (i * 4096) as u64;
            q.avail = q.desc + 256;
            q.used = q.desc + 512;
        }
        t.status = 15;
        Self {
            c: Console::new(policy),
            t,
            m: vec![0; 262144],
            base,
            next: 65536,
            avail: [0; 4],
            dirty: Vec::new(),
        }
    }
    fn post(&mut self, q: usize, bytes: &[u8], writable: bool) -> usize {
        let head = self.avail[q] % 8;
        let addr = self.next;
        self.next += bytes.len().max(1);
        self.m[addr..addr + bytes.len()].copy_from_slice(bytes);
        let d = q * 4096 + head as usize * 16;
        self.m[d..d + 8].copy_from_slice(&(self.base + addr as u64).to_le_bytes());
        self.m[d + 8..d + 12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.m[d + 12..d + 14].copy_from_slice(&(if writable { 2u16 } else { 0 }).to_le_bytes());
        let a = q * 4096 + 256;
        self.m[a + 4 + head as usize * 2..a + 6 + head as usize * 2]
            .copy_from_slice(&head.to_le_bytes());
        self.avail[q] += 1;
        self.m[a + 2..a + 4].copy_from_slice(&self.avail[q].to_le_bytes());
        addr
    }
    fn notify(&mut self, q: usize) -> Result<Output> {
        self.c
            .notify(&mut self.t, &mut self.m, self.base, q, |a, n| {
                self.dirty.push((a, n))
            })
    }
    fn receive(&mut self, b: &[u8]) -> Result<bool> {
        self.c
            .receive(&mut self.t, &mut self.m, self.base, b, |a, n| {
                self.dirty.push((a, n))
            })
    }
    fn handshake(&mut self, id: u32, event: u16, value: u16) -> Result<Output> {
        let mut b = id.to_le_bytes().to_vec();
        b.extend(event.to_le_bytes());
        b.extend(value.to_le_bytes());
        self.post(3, &b, false);
        self.notify(3)
    }
    fn open(&mut self) {
        self.handshake(u32::MAX, 0, 1).unwrap();
        self.handshake(0, 3, 1).unwrap();
        self.handshake(0, 6, 1).unwrap();
        self.t.isr = 0;
        self.dirty.clear();
    }
}

#[test]
fn closed_ports_and_per_chain_transmit_limits_match() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        let at = g.post(0, &[0; 3], true);
        assert!(!g.receive(b"abc").unwrap());
        assert_eq!(&g.m[at..at + 3], b"\0\0\0");
        g.post(1, b"hi", false);
        assert_eq!(
            g.notify(1).err().unwrap(),
            "console transmit before port open"
        );
        assert_eq!(g.t.queues[1].used_idx, 0);
        g.open();
        assert_eq!(&g.m[at..at + 3], b"abc");
        assert_eq!(g.notify(1).unwrap().payloads, vec![b"hi".to_vec()]);
        let mut g = Guest::new(policy);
        g.open();
        g.post(1, &vec![1; 40000], false);
        g.post(1, &vec![2; 30000], false);
        assert_eq!(
            g.notify(1)
                .unwrap()
                .payloads
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![40000, 30000]
        );
        assert_eq!(g.c.counters(), (70000, 0));
        assert_eq!(g.t.isr, 1);
    }
}

#[test]
fn late_direction_error_preserves_completed_prefix_and_irq_policy() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        g.open();
        g.post(1, b"first", false);
        g.post(1, b"bad", true);
        assert_eq!(
            g.notify(1).err().unwrap(),
            "unsupported console descriptor flags/direction"
        );
        assert_eq!((g.t.queues[1].last_avail, g.t.queues[1].used_idx), (1, 1));
        assert_eq!(g.t.isr, 1);
        assert_eq!(g.dirty, vec![(4096 + 516, 8), (4096 + 514, 2)]);
    }
}

#[test]
fn counter_overflow_rejects_before_dma() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        g.open();
        g.c.rx_dma_bytes = u64::MAX;
        let addr = g.post(0, &[0; 2], true);
        assert_eq!(
            g.receive(b"xy").err().unwrap(),
            "control DMA byte counter overflow"
        );
        assert_eq!(&g.m[addr..addr + 2], &[0; 2]);
        assert_eq!((g.t.queues[0].last_avail, g.t.queues[0].used_idx), (0, 0));
        assert_eq!(g.c.rx.iter().copied().collect::<Vec<_>>(), b"xy");
        assert_eq!(g.c.rx_dma_bytes, u64::MAX);
        assert_eq!(g.t.isr, 0);
        assert!(g.dirty.is_empty());
        g.c.tx_bytes = u64::MAX;
        g.post(1, b"z", false);
        assert_eq!(
            g.notify(1).err().unwrap(),
            "control transmit byte counter overflow"
        );
        assert_eq!(g.t.queues[1].used_idx, 0);
    }
}

#[test]
fn clone_restore_repeats_handshake_backlog_dma_and_journal() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        g.open();
        let saved = (g.c.clone(), g.t.clone(), g.m.clone(), g.next, g.avail);
        let perform = |g: &mut Guest| {
            g.post(2, &[0; 8], true);
            g.post(2, &[0; 15], true);
            g.post(2, &[0; 8], true);
            g.notify(2).unwrap();
            g.post(0, &[0; 2], true);
            g.receive(b"abcd").unwrap();
            (
                g.c.projection(),
                serde_json::to_value(g.t.projection()).unwrap(),
                g.m.clone(),
                g.dirty.clone(),
            )
        };
        let expected = perform(&mut g);
        g.c = saved.0;
        g.t = saved.1;
        g.m = saved.2;
        g.next = saved.3;
        g.avail = saved.4;
        g.dirty.clear();
        assert_eq!(perform(&mut g), expected);
        assert_eq!(g.c.pending_rx_bytes(), 2);
    }
}

#[test]
fn repeated_handshake_and_receive_size_boundaries_are_transactional() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        g.open();
        let before = g.c.projection();
        assert_eq!(
            g.handshake(u32::MAX, 0, 1).err().unwrap(),
            "invalid console control handshake"
        );
        assert_eq!(g.c.projection(), before);
        g.receive(&vec![0; LIMIT]).unwrap();
        let before = g.c.projection();
        assert_eq!(
            g.receive(&[1]).err().unwrap(),
            "console receive backlog full"
        );
        assert_eq!(g.c.projection(), before);
        let mut tx = Guest::new(policy);
        tx.open();
        tx.post(1, &vec![0; LIMIT], false);
        assert_eq!(tx.notify(1).unwrap().payloads[0].len(), LIMIT);
        let mut tx = Guest::new(policy);
        tx.open();
        tx.post(1, &vec![0; LIMIT + 1], false);
        assert_eq!(
            tx.notify(1).err().unwrap(),
            "console descriptor chain too large"
        );
        assert_eq!(tx.t.queues[1].used_idx, 0);
    }
}

#[test]
fn late_receive_and_control_errors_preserve_the_completed_prefix() {
    for policy in [Policy::Application, Policy::Console] {
        let mut g = Guest::new(policy);
        g.open();
        let at = g.post(0, &[0; 2], true);
        g.post(0, &[0; 2], false);
        assert!(g.receive(b"abcd").is_err());
        assert_eq!(&g.m[at..at + 2], b"ab");
        assert_eq!(g.c.rx.iter().copied().collect::<Vec<_>>(), b"cd");
        assert_eq!((g.t.queues[0].last_avail, g.t.queues[0].used_idx), (1, 1));
        assert_eq!(g.t.isr, 1);
        assert_eq!(g.dirty, vec![(at, 2), (516, 8), (514, 2)]);

        let mut g = Guest::new(policy);
        g.handshake(u32::MAX, 0, 1).unwrap();
        g.t.isr = 0;
        g.dirty.clear();
        let at = g.post(2, &[0; 8], true);
        g.post(2, &[0; 7], true);
        assert!(g.handshake(0, 3, 1).is_err());
        assert_eq!(&g.m[at..at + 8], &[0, 0, 0, 0, 1, 0, 1, 0]);
        assert_eq!(g.c.control_rx.len(), 2);
        assert_eq!((g.t.queues[2].last_avail, g.t.queues[2].used_idx), (1, 1));
        assert_eq!(g.t.isr, 1);
        assert_eq!(
            g.dirty,
            vec![
                (3 * 4096 + 524, 8),
                (3 * 4096 + 514, 2),
                (at, 8),
                (2 * 4096 + 516, 8),
                (2 * 4096 + 514, 2)
            ]
        );
    }
}
