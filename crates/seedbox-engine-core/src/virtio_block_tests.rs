use super::*;
use crate::virtio_mmio::{Policy as MmioPolicy, Spec};
struct Guest {
    b: Block,
    t: Transport,
    m: Vec<u8>,
    base: u64,
    next: usize,
    desc: u16,
    avail: u16,
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
                device_id: 2,
                features: [512, 1],
            },
            1,
            if policy == Policy::Application {
                MmioPolicy::Application
            } else {
                MmioPolicy::Block
            },
        );
        let q = &mut t.queues[0];
        q.num = 64;
        q.ready = true;
        q.desc = base;
        q.avail = base + 4096;
        q.used = base + 8192;
        Self {
            b: Block::new(Disk::new(None, Some(8)).unwrap(), policy),
            t,
            m: vec![0; 1 << 20],
            base,
            next: 65536,
            desc: 0,
            avail: 0,
            dirty: Vec::new(),
        }
    }
    fn buf(&mut self, b: &[u8]) -> usize {
        let a = self.next;
        self.next += b.len().max(1);
        self.m[a..a + b.len()].copy_from_slice(b);
        a
    }
    fn post(&mut self, spans: &[(usize, usize, bool)]) {
        let head = self.desc;
        for (i, &(addr, len, writable)) in spans.iter().enumerate() {
            let d = self.desc as usize * 16;
            self.m[d..d + 8].copy_from_slice(&(self.base + addr as u64).to_le_bytes());
            self.m[d + 8..d + 12].copy_from_slice(&(len as u32).to_le_bytes());
            let next = i + 1 < spans.len();
            let flags = (if writable { 2u16 } else { 0 }) | u16::from(next);
            self.m[d + 12..d + 14].copy_from_slice(&flags.to_le_bytes());
            self.m[d + 14..d + 16].copy_from_slice(&(self.desc + 1).to_le_bytes());
            self.desc += 1;
        }
        let a = 4096 + 4 + self.avail as usize * 2;
        self.m[a..a + 2].copy_from_slice(&head.to_le_bytes());
        self.avail += 1;
        self.m[4098..4100].copy_from_slice(&self.avail.to_le_bytes());
    }
    fn header(&mut self, kind: u32, sector: u64) -> usize {
        let mut h = vec![0; 16];
        h[..4].copy_from_slice(&kind.to_le_bytes());
        h[8..].copy_from_slice(&sector.to_le_bytes());
        self.buf(&h)
    }
    fn request(&mut self, kind: u32, sector: u64, data: &[u8], writable: bool) -> (usize, usize) {
        let h = self.header(kind, sector);
        let d = self.buf(data);
        let s = self.buf(&[0xff]);
        let mut spans = vec![(h, 16, false)];
        if !data.is_empty() {
            spans.push((d, data.len(), writable));
        }
        spans.push((s, 1, true));
        self.post(&spans);
        (d, s)
    }
    fn notify(&mut self) -> Result<Notified> {
        self.b.notify(&mut self.t, &mut self.m, self.base, |a, n| {
            self.dirty.push((a, n))
        })
    }
}
#[test]
fn successful_disk_operations_and_snapshot_restore_keep_layers_and_stats() {
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        g.request(1, 1, &[7; 512], false);
        g.notify().unwrap();
        let metadata = serde_json::to_vec(&g.b.snapshot()).unwrap();
        let mut disk_bytes = Vec::new();
        g.b.disk().write_checkpoint(&mut disk_bytes).unwrap();
        let saved = (
            g.b.clone(),
            g.t.clone(),
            g.m.clone(),
            g.next,
            g.desc,
            g.avail,
        );
        g.request(4, 0, &[], false);
        g.notify().unwrap();
        assert_eq!(
            (g.b.disk().stable_sectors(), g.b.disk().volatile_sectors()),
            (1, 0)
        );
        let (d, s) = g.request(0, 1, &[0xcc; 512], true);
        g.notify().unwrap();
        assert_eq!(&g.m[d..d + 512], &[7; 512]);
        assert_eq!(g.m[s], 0);
        assert_eq!(
            g.b.stats().reads,
            if policy == Policy::Block { 1 } else { 0 }
        );
        let expected = g.b.disk().digest();
        let stats = g.b.stats().clone();
        // The durable path must preserve the same continuation as a live model.
        let base = Guest::new(policy).b.into_disk();
        let restored_disk = Disk::read_checkpoint(base, disk_bytes.as_slice()).unwrap();
        assert!(Block::new(Guest::new(policy).b.into_disk(), policy)
            .restore_snapshot(serde_json::from_slice(&metadata).unwrap())
            .is_err());
        g.b = Block::new(restored_disk, policy)
            .restore_snapshot(serde_json::from_slice(&metadata).unwrap())
            .unwrap();
        assert_eq!(g.b.disk().digest(), saved.0.disk().digest());
        assert_eq!(g.b.stats(), saved.0.stats());
        g.t = saved.1;
        g.m = saved.2;
        g.next = saved.3;
        g.desc = saved.4;
        g.avail = saved.5;
        g.request(4, 0, &[], false);
        g.notify().unwrap();
        g.request(0, 1, &[0xcc; 512], true);
        g.notify().unwrap();
        assert_eq!(g.b.disk().digest(), expected);
        assert_eq!(*g.b.stats(), stats);
        let (crashed, lost) = g.b.disk().crashed_seeded(3, "guest", 0);
        assert_eq!(lost, 0);
        assert_eq!(crashed.digest(), expected);
    }
}
#[test]
fn descriptor_profiles_keep_split_header_zero_length_and_partial_io_distinct() {
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        let h = g.header(1, 1);
        let d = g.buf(&[9; 512]);
        let s = g.buf(&[0xff]);
        g.post(&[
            (h, 8, false),
            (h + 8, 8, false),
            (d, 512, false),
            (s, 1, true),
        ]);
        if policy == Policy::Application {
            assert_eq!(
                g.notify().unwrap_err(),
                "invalid block header/status descriptors"
            );
            assert_eq!(g.m[s], 0xff);
        } else {
            g.notify().unwrap();
            assert_eq!(g.m[s], 0);
        }
        let mut g = Guest::new(policy);
        let h = g.header(8, 0);
        let s = g.buf(&[0xff]);
        g.post(&[(h, 16, false), (s, 0, true), (s, 1, true)]);
        g.notify().unwrap();
        assert_eq!(g.m[s], 0);
        let mut empty = Guest::new(policy);
        let at = empty.buf(&[]);
        empty.post(&[(at, 0, false)]);
        assert_eq!(
            empty.notify().unwrap_err(),
            if policy == Policy::Application {
                "empty virtio descriptor chain"
            } else {
                "disk request without a header or status byte"
            }
        );
        let mut g = Guest::new(policy);
        let before = g.b.disk().digest();
        let (_, s) = g.request(1, 1, &[7; 513], false);
        if policy == Policy::Application {
            assert_eq!(g.notify().unwrap_err(), "invalid block data descriptors");
            assert_eq!(g.m[s], 0xff);
        } else {
            assert_eq!(g.notify().unwrap().log, vec![(1, 1, 1)]);
            assert_eq!(g.m[s], 1);
        }
        assert_eq!(g.b.disk().digest(), before);
    }
}
#[test]
fn flush_data_and_late_malformed_request_preserve_disk_prefix_and_irq_boundary() {
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        let (_, s) = g.request(1, 1, &[5; 512], false);
        let h = g.header(0, 1);
        g.post(&[(h, 16, false)]);
        assert!(g.notify().is_err());
        assert_eq!(g.m[s], 0);
        assert_eq!(g.b.disk().volatile_sectors(), 1);
        assert_eq!((g.t.queues[0].last_avail, g.t.queues[0].used_idx), (1, 1));
        assert_eq!(g.t.isr, if policy == Policy::Application { 1 } else { 0 });
        assert_eq!(g.dirty, vec![(s, 1), (8196, 8), (8194, 2)]);
        let mut g = Guest::new(policy);
        g.b.disk_mut().write(1, &[5; 512]).unwrap();
        let (_, s) = g.request(4, 0, &[1; 16], false);
        if policy == Policy::Application {
            assert_eq!(
                g.notify().unwrap_err(),
                "block flush includes data descriptors"
            );
            assert_eq!(g.b.disk().volatile_sectors(), 1);
            assert_eq!(g.m[s], 0xff);
        } else {
            assert_eq!(g.notify().unwrap().flushed, 1);
            assert_eq!(g.b.disk().stable_sectors(), 1);
            assert_eq!(g.m[s], 0);
        }
    }
}
#[test]
fn aliased_payload_status_and_failed_read_keep_exact_write_order() {
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        let diskdata: Vec<_> = (0..512).map(|n| (n / 128) as u8).collect();
        g.b.disk_mut().write(1, &diskdata).unwrap();
        let h = g.header(0, 1);
        let d = g.buf(&[0xcc; 512]);
        g.post(&[
            (h, 16, false),
            (d, 256, true),
            (d + 128, 256, true),
            (d + 129, 1, true),
        ]);
        g.notify().unwrap();
        assert_eq!(g.m[d], 0);
        assert_eq!(g.m[d + 128], 2);
        assert_eq!(g.m[d + 129], 0);
        assert_eq!(g.m[d + 256], 3);
        assert_eq!(
            g.dirty,
            vec![(d, 256), (d + 128, 256), (d + 129, 1), (8196, 8), (8194, 2)]
        );
        let (data, status) = g.request(0, 99, &[0xcc; 512], true);
        g.dirty.clear();
        g.notify().unwrap();
        assert_eq!(&g.m[data..data + 512], &[0xcc; 512]);
        assert_eq!(g.m[status], 1);
        assert_eq!(g.dirty, vec![(status, 1), (8204, 8), (8194, 2)]);
    }
}
#[test]
fn get_id_zero_short_room_and_unsupported_requests_keep_used_lengths() {
    for policy in [Policy::Application, Policy::Block] {
        for room in [0, 3, 20, 40] {
            let mut g = Guest::new(policy);
            let (d, s) = g.request(8, 0, &vec![0xcc; room], true);
            g.notify().unwrap();
            let n = room.min(ID.len());
            assert_eq!(&g.m[d..d + n], &ID[..n]);
            assert_eq!(g.m[s], 0);
            assert_eq!(
                u32::from_le_bytes(g.m[8200..8204].try_into().unwrap()),
                (n + 1) as u32
            );
        }
        let mut g = Guest::new(policy);
        let (_, s) = g.request(123, 0, &[3; 13], false);
        g.notify().unwrap();
        assert_eq!(g.m[s], 2);
        assert_eq!(
            g.b.stats().errors,
            if policy == Policy::Block { 1 } else { 0 }
        );
    }
}
