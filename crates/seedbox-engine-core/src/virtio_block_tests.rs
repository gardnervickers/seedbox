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
        self.notify_at(0)
    }
    fn notify_at(&mut self, now: u64) -> Result<Notified> {
        self.b
            .notify(&mut self.t, &mut self.m, self.base, now, |a, n| {
                self.dirty.push((a, n))
            })
    }
    fn complete_at(&mut self, now: u64) -> Result<Notified> {
        self.b.complete_due(&mut self.t, &mut self.m, now, |a, n| {
            self.dirty.push((a, n))
        })
    }
    fn used_idx(&self) -> u16 {
        u16::from_le_bytes(self.m[8194..8196].try_into().unwrap())
    }
}

#[test]
fn injected_latency_completes_in_order_at_due_times_with_deferred_effects() {
    let mut g = Guest::new(Policy::Block);
    let rng = crate::rng::Rng::new(7, "disk-latency/test");
    let latency = InjectedLatency::new(seedbox_control::DiskOp::Write, 5_000, 2_000, rng).unwrap();
    assert!(g.b.set_latency(Some(latency)).is_none());
    let (_, write_status) = g.request(1, 1, &[9; 512], false);
    let (_, flush_status) = g.request(4, 0, &[], false);
    let out = g.notify_at(100).unwrap();
    // Nothing is due yet: no effect, status, used entry or interrupt.
    assert_eq!((out.completed, out.irq, g.t.isr), (0, false, 0));
    assert_eq!(
        (g.m[write_status], g.m[flush_status], g.used_idx()),
        (0xff, 0xff, 0)
    );
    assert_eq!(g.b.disk().volatile_sectors(), 0);
    let dues = g.b.pending_due();
    assert_eq!(dues.len(), 2);
    assert!((5_100..=7_100).contains(&dues[0]));
    // The flush is not delayed itself, but never overtakes the write.
    assert_eq!(dues[1], dues[0]);
    // Checkpoints keep pending requests and the jitter stream.
    let saved = serde_json::to_string(&g.b.snapshot().unwrap()).unwrap();
    assert!(saved.contains("pending"));
    let out = g.complete_at(dues[0] - 1).unwrap();
    assert_eq!(out.completed, 0);
    let out = g.complete_at(dues[0]).unwrap();
    assert_eq!((out.completed, out.irq), (2, true));
    assert_eq!(
        (g.m[write_status], g.m[flush_status], g.used_idx()),
        (0, 0, 2)
    );
    assert_eq!(g.b.disk().stable_sectors(), 1);
    assert_eq!(g.b.next_due(), None);
    // Healing reports what it did; later requests complete at once again.
    let counts = g.b.set_latency(None).unwrap();
    assert_eq!((counts.delayed, counts.deepest), (1, 2));
    g.request(0, 1, &[0; 512], true);
    assert_eq!(g.notify_at(dues[0] + 1).unwrap().completed, 1);
}

#[test]
fn a_rebuilt_device_loses_pending_requests_but_keeps_its_latency() {
    let mut g = Guest::new(Policy::Block);
    let rng = crate::rng::Rng::new(1, "disk-latency/test");
    let latency = InjectedLatency::new(seedbox_control::DiskOp::Any, 1_000, 0, rng).unwrap();
    g.b.set_latency(Some(latency));
    g.request(1, 0, &[3; 512], false);
    g.notify_at(10).unwrap();
    assert_eq!(g.b.next_due(), Some(1_010));
    // A reset (or a crash) rebuilds the device from its disk.
    let latency = g.b.take_latency();
    let mut rebuilt = Block::new(g.b.clone().into_disk(), Policy::Block);
    rebuilt.set_latency(latency);
    assert_eq!(rebuilt.next_due(), None);
    assert_eq!(
        rebuilt.disk().volatile_sectors() + rebuilt.disk().stable_sectors(),
        0
    );
    assert!(rebuilt.take_latency().is_some());
}
#[test]
fn successful_disk_operations_and_snapshot_restore_keep_layers_and_stats() {
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        g.request(1, 1, &[7; 512], false);
        g.notify().unwrap();
        let metadata = serde_json::to_vec(&g.b.snapshot().unwrap()).unwrap();
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
fn descriptor_profiles_accept_modern_framing_and_report_io_errors() {
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
        g.notify().unwrap();
        assert_eq!(g.m[s], 0);
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
            "disk request without a header or status byte"
        );
        let mut g = Guest::new(policy);
        let before = g.b.disk().digest();
        let (_, s) = g.request(1, 1, &[7; 513], false);
        g.notify().unwrap();
        assert_eq!(g.m[s], 1);
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
        assert_eq!(g.t.isr, 1);
        assert_eq!(g.dirty, vec![(s, 1), (8196, 8), (8194, 2)]);
        let mut g = Guest::new(policy);
        g.b.disk_mut().write(1, &[5; 512]).unwrap();
        let (_, s) = g.request(4, 0, &[1; 16], false);
        assert_eq!(g.notify().unwrap().flushed, 1);
        assert_eq!(g.b.disk().stable_sectors(), 1);
        assert_eq!(g.m[s], 0);
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
#[test]
fn injected_errors_fail_matching_requests_without_disk_effects() {
    use seedbox_control::DiskOp;
    let errors = |op, ppm| InjectedErrors::new(op, ppm, crate::rng::Rng::new(1, "test")).unwrap();
    for policy in [Policy::Application, Policy::Block] {
        let mut g = Guest::new(policy);
        // A durable sector to read back, then writes fail and leave no trace.
        g.request(1, 1, &[7; 512], false);
        g.request(4, 0, &[], false);
        g.notify().unwrap();
        let before = g.b.disk().digest();
        assert!(g
            .b
            .set_errors(Some(errors(DiskOp::Write, 1_000_000)))
            .is_none());
        let (_, write) = g.request(1, 2, &[9; 512], false);
        let (data, read) = g.request(0, 1, &[0; 512], true);
        g.notify().unwrap();
        assert_eq!((g.m[write], g.m[read]), (1, 0));
        assert_eq!(&g.m[data..data + 512], &[7; 512]);
        assert_eq!(g.b.disk().digest(), before);
        let counts =
            g.b.set_errors(Some(errors(DiskOp::Flush, 1_000_000)))
                .unwrap();
        assert_eq!(
            counts,
            ErrorCounts {
                matched: 1,
                failed: 1
            }
        );
        // A failed flush makes nothing durable.
        g.request(1, 3, &[4; 512], false);
        let (_, flush) = g.request(4, 0, &[], false);
        g.notify().unwrap();
        assert_eq!(g.m[flush], 1);
        assert_eq!(g.b.disk().volatile_sectors(), 1);
        // Reads fail without writing data into guest memory.
        g.b.set_errors(Some(errors(DiskOp::Read, 1_000_000)));
        let (data, read) = g.request(0, 1, &[0xee; 512], true);
        g.notify().unwrap();
        assert_eq!(g.m[read], 1);
        assert_eq!(&g.m[data..data + 512], &[0xee; 512]);
        // Healing restores normal service; GET_ID never matches.
        g.b.set_errors(Some(errors(DiskOp::Any, 1_000_000)));
        let (_, id) = g.request(8, 0, &[0; 20], true);
        g.notify().unwrap();
        assert_eq!(g.m[id], 0);
        assert_eq!(g.b.set_errors(None), Some(ErrorCounts::default()));
        let (_, flush) = g.request(4, 0, &[], false);
        g.notify().unwrap();
        assert_eq!(g.m[flush], 0);
        assert_eq!(g.b.disk().volatile_sectors(), 0);
    }
}
#[test]
fn injected_corruption_garbles_one_sector_and_reports_success() {
    use seedbox_control::{DiskEffect, DiskOp};
    let corrupt = |op| {
        InjectedErrors::new(op, 1_000_000, crate::rng::Rng::new(1, "test"))
            .unwrap()
            .with_effect(DiskEffect::Corrupt)
    };
    let differing = |data: &[u8], fill: u8| {
        data.chunks(512)
            .map(|sector| sector.iter().any(|b| *b != fill))
            .collect::<Vec<_>>()
    };
    let mut g = Guest::new(Policy::Block);
    g.request(1, 0, &[7; 1024], false);
    g.notify().unwrap();
    // A corrupted write succeeds and stores one garbled sector of the two.
    g.b.set_errors(Some(corrupt(DiskOp::Write)));
    let (_, write) = g.request(1, 2, &[9; 1024], false);
    g.notify().unwrap();
    assert_eq!(g.m[write], 0);
    let mut stored = vec![0; 1024];
    g.b.disk().read(2, &mut stored).unwrap();
    assert_eq!(
        differing(&stored, 9).iter().filter(|d| **d).count(),
        1,
        "{stored:?}"
    );
    assert_eq!(
        g.b.set_errors(None),
        Some(ErrorCounts {
            matched: 1,
            failed: 1
        })
    );
    // A corrupted read returns one garbled sector and leaves the disk right.
    let before = g.b.disk().digest();
    g.b.set_errors(Some(corrupt(DiskOp::Any)));
    let (data, read) = g.request(0, 0, &[0; 1024], true);
    g.notify().unwrap();
    assert_eq!(g.m[read], 0);
    assert_eq!(
        differing(&g.m[data..data + 1024], 7)
            .iter()
            .filter(|d| **d)
            .count(),
        1
    );
    assert_eq!(g.b.disk().digest(), before);
    // Flushes are never selected: they succeed and draw nothing.
    let (_, flush) = g.request(4, 0, &[], false);
    g.notify().unwrap();
    assert_eq!(g.m[flush], 0);
    assert_eq!(g.b.disk().volatile_sectors(), 0);
    assert_eq!(
        g.b.set_errors(None),
        Some(ErrorCounts {
            matched: 1,
            failed: 1
        })
    );
}
#[test]
fn partial_injected_errors_are_seeded_and_survive_snapshots() {
    use seedbox_control::DiskOp;
    let run = |snapshot_after: Option<usize>| {
        let mut g = Guest::new(Policy::Block);
        let rng = crate::rng::Rng::new(5, "disk-error/n1/0/1");
        g.b.set_errors(Some(
            InjectedErrors::new(DiskOp::Write, 500_000, rng).unwrap(),
        ));
        let mut statuses = Vec::new();
        // Three descriptors each: stay within the 64-entry test queue.
        for i in 0..20 {
            if snapshot_after == Some(i) {
                let snapshot: Snapshot =
                    serde_json::from_slice(&serde_json::to_vec(&g.b.snapshot().unwrap()).unwrap())
                        .unwrap();
                let disk = g.b.clone().into_disk();
                g.b = Block::new(disk, Policy::Block)
                    .restore_snapshot(snapshot)
                    .unwrap();
            }
            let (_, s) = g.request(1, i as u64 % 8, &[i as u8; 512], false);
            g.notify().unwrap();
            statuses.push(g.m[s]);
        }
        (statuses, g.b.set_errors(None).unwrap())
    };
    let (statuses, counts) = run(None);
    assert_eq!(counts.matched, 20);
    assert!(counts.failed > 2 && counts.failed < 18, "{counts:?}");
    assert_eq!(
        statuses.iter().filter(|s| **s == 1).count() as u64,
        counts.failed
    );
    assert_eq!(run(Some(9)), (statuses, counts));
    assert!(InjectedErrors::new(DiskOp::Any, 0, crate::rng::Rng::new(1, "x")).is_err());
}
#[test]
fn a_hold_completes_earlier_requests_and_leaves_the_selected_one_untouched() {
    use seedbox_control::DiskOp;
    let mut g = Guest::new(Policy::Block);
    let hold = Hold {
        trigger_id: 7,
        op: DiskOp::Write,
        nth: 2,
        seen: 0,
    };
    g.b.set_hold(Some(hold)).unwrap();
    // The count survives a checkpoint taken before the hold.
    let (_, first) = g.request(1, 1, &[1; 512], false);
    assert_eq!(g.notify().unwrap().held, None);
    let saved = g.b.snapshot().unwrap();
    let disk = g.b.clone().into_disk();
    g.b = Block::new(disk, Policy::Block)
        .restore_snapshot(saved)
        .unwrap();
    // One notification: a read, the held write, and a write after it.
    let (_, read) = g.request(0, 1, &[0; 512], true);
    let (_, held) = g.request(1, 2, &[2; 512], false);
    let (_, after) = g.request(1, 3, &[3; 512], false);
    let used_before = u16::from_le_bytes([g.m[8194], g.m[8195]]);
    let notified = g.notify().unwrap();
    assert_eq!((notified.held, notified.completed), (Some(7), 1));
    assert_eq!(
        (g.m[first], g.m[read], g.m[held], g.m[after]),
        (0, 0, 0xff, 0xff)
    );
    assert_eq!(u16::from_le_bytes([g.m[8194], g.m[8195]]), used_before + 1);
    let mut sector = [0; 512];
    g.b.disk().read(2, &mut sector).unwrap();
    assert_eq!(sector, [0; 512], "the held write has no disk effect");
    assert_eq!(g.b.held(), Some(7));
    assert!(g.b.snapshot().is_err());
    assert!(g.notify().is_err());
    assert!(g.b.set_hold(None).is_err());
}

#[test]
fn a_stopped_queue_drops_pending_requests_and_a_reused_chain_is_rejected() {
    let latency = |seed| {
        let rng = crate::rng::Rng::new(seed, "disk-latency/test");
        InjectedLatency::new(seedbox_control::DiskOp::Any, 1_000, 0, rng).unwrap()
    };
    // The driver stops the queue: the request is dropped without any DMA.
    let mut g = Guest::new(Policy::Block);
    g.b.set_latency(Some(latency(1)));
    let (_, status) = g.request(1, 0, &[5; 512], false);
    g.notify_at(0).unwrap();
    g.t.queues[0].ready = false;
    let out = g.complete_at(1_000).unwrap();
    assert_eq!((out.completed, g.m[status], g.used_idx()), (0, 0xff, 0));
    assert_eq!(
        g.b.disk().volatile_sectors() + g.b.disk().stable_sectors(),
        0
    );
    assert_eq!(g.b.next_due(), None);

    // Disabling then re-enabling the same layout still loses the request,
    // and the reused head is no longer pending.
    let mut g = Guest::new(Policy::Block);
    g.b.set_latency(Some(latency(3)));
    let (_, status) = g.request(1, 0, &[5; 512], false);
    g.notify_at(0).unwrap();
    g.t.queues[0].ready = false;
    g.b.queue_changed(&g.t);
    g.t.queues[0].ready = true;
    g.b.queue_changed(&g.t);
    assert_eq!(g.b.next_due(), None);
    let out = g.complete_at(1_000).unwrap();
    assert_eq!((out.completed, g.m[status], g.used_idx()), (0, 0xff, 0));

    // Publishing a chain the device still holds is a driver error.
    let mut g = Guest::new(Policy::Block);
    g.b.set_latency(Some(latency(2)));
    g.request(1, 0, &[5; 512], false);
    g.notify_at(0).unwrap();
    let a = 4096 + 4 + g.avail as usize * 2;
    g.m[a..a + 2].copy_from_slice(&0u16.to_le_bytes());
    g.avail += 1;
    g.m[4098..4100].copy_from_slice(&g.avail.to_le_bytes());
    assert!(g
        .notify_at(10)
        .unwrap_err()
        .contains("reused while pending"));
}
