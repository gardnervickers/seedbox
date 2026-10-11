//! Block MMIO adapter; the shared model owns requests, disk effects and DMA.
use crate::disk::Disk;
const F_FLUSH: u32 = 1 << 9;
const F_VERSION_1: u32 = 1;
pub use seedbox_engine_core::virtio_block::Notified;
use seedbox_engine_core::virtio_block::{Block, Policy as BlockPolicy, Stats};
use seedbox_engine_core::virtio_mmio::{Effect, Policy as MmioPolicy, Spec, Transport};
#[derive(Clone)]
pub struct VirtioBlk {
    model: Block,
    transport: Transport,
    pub dirty: Vec<(usize, usize)>,
}
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    model: seedbox_engine_core::virtio_block::Snapshot,
    transport: seedbox_engine_core::virtio_mmio::Snapshot,
    dirty: Vec<(usize, usize)>,
}

impl VirtioBlk {
    pub(crate) fn snapshot(&self) -> Snapshot {
        Snapshot {
            model: self.model.snapshot(),
            transport: self.transport.snapshot(),
            dirty: self.dirty.clone(),
        }
    }
    pub(crate) fn from_snapshot(snapshot: Snapshot, disk: Disk) -> Result<Self, String> {
        let device = Self::new(disk);
        Ok(Self {
            model: device.model.restore_snapshot(snapshot.model)?,
            transport: device.transport.restore_snapshot(snapshot.transport)?,
            dirty: snapshot.dirty,
        })
    }

    pub fn new(disk: Disk) -> Self {
        Self {
            model: Block::new(disk, BlockPolicy::Block),
            transport: Transport::new(
                Spec {
                    device_id: 2,
                    features: [F_FLUSH, F_VERSION_1],
                },
                1,
                MmioPolicy::Block,
            ),
            dirty: Vec::new(),
        }
    }
    pub fn disk(&self) -> &Disk {
        self.model.disk()
    }
    #[cfg(test)]
    pub fn disk_mut(&mut self) -> &mut Disk {
        self.model.disk_mut()
    }
    pub fn stats(&self) -> &Stats {
        self.model.stats()
    }
    fn config(&self, off: u64) -> u8 {
        match off {
            0..=7 => self.disk().capacity().to_le_bytes()[off as usize],
            _ => 0,
        }
    }
    pub fn read(&mut self, off: u64) -> u32 {
        if off >= 0x100 {
            let b = off - 0x100;
            u32::from_le_bytes([
                self.config(b),
                self.config(b + 1),
                self.config(b + 2),
                self.config(b + 3),
            ])
        } else {
            self.transport.read(off)
        }
    }
    pub fn write(&mut self, m: &mut [u8], off: u64, v: u32) -> Result<Notified, String> {
        match self.transport.write(off, v)? {
            Effect::Notify(_) => {
                let dirty = &mut self.dirty;
                self.model
                    .notify(&mut self.transport, m, 0, |a, n| dirty.push((a, n)))
            }
            // The transport reset has completed; disk, stats and dirty journal survive.
            Effect::Reset | Effect::None => Ok(Notified::default()),
        }
    }
}
#[cfg(test)]
use crate::disk::SECTOR;
#[cfg(test)]
const T_IN: u32 = 0;
#[cfg(test)]
const T_OUT: u32 = 1;
#[cfg(test)]
const T_FLUSH: u32 = 4;
#[cfg(test)]
const T_GET_ID: u32 = 8;
#[cfg(test)]
const S_OK: u8 = 0;
#[cfg(test)]
const S_IOERR: u8 = 1;
#[cfg(test)]
const S_UNSUPP: u8 = 2;
#[cfg(test)]
fn rd16(m: &[u8], a: usize) -> u16 {
    u16::from_le_bytes(m[a..a + 2].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: u64 = 0x1000;
    const AVAIL: u64 = 0x2000;
    const USED: u64 = 0x3000;

    struct Guest {
        m: Vec<u8>,
        dev: VirtioBlk,
        next_desc: u16,
        avail_idx: u16,
        next_buf: usize,
    }

    impl Guest {
        fn new(sectors: u64) -> Self {
            let mut g = Guest {
                m: vec![0; 1 << 20],
                dev: VirtioBlk::new(Disk::new(None, Some(sectors)).unwrap()),
                next_desc: 0,
                avail_idx: 0,
                next_buf: 0x10000,
            };
            for (off, v) in [
                (0x030, 0),
                (0x038, 16),
                (0x080, DESC as u32),
                (0x090, AVAIL as u32),
                (0x0a0, USED as u32),
                (0x044, 1),
            ] {
                g.dev.write(&mut g.m, off, v).unwrap();
            }
            g
        }
        fn buf(&mut self, bytes: &[u8]) -> u64 {
            let a = self.next_buf;
            self.m[a..a + bytes.len()].copy_from_slice(bytes);
            self.next_buf += bytes.len().next_multiple_of(64);
            a as u64
        }
        /// Post a chain of (address, len, writable) and return its head.
        fn post(&mut self, segs: &[(u64, u32, bool)]) -> u16 {
            let head = self.next_desc;
            for (k, &(addr, len, w)) in segs.iter().enumerate() {
                let i = self.next_desc % 16;
                self.next_desc += 1;
                let d = DESC as usize + 16 * i as usize;
                let more = k + 1 < segs.len();
                self.m[d..d + 8].copy_from_slice(&addr.to_le_bytes());
                self.m[d + 8..d + 12].copy_from_slice(&len.to_le_bytes());
                self.m[d + 12..d + 14]
                    .copy_from_slice(&((w as u16) << 1 | more as u16).to_le_bytes());
                self.m[d + 14..d + 16].copy_from_slice(&((i + 1) % 16).to_le_bytes());
            }
            let slot = AVAIL as usize + 4 + 2 * (self.avail_idx % 16) as usize;
            self.m[slot..slot + 2].copy_from_slice(&(head % 16).to_le_bytes());
            self.avail_idx += 1;
            self.m[AVAIL as usize + 2..AVAIL as usize + 4]
                .copy_from_slice(&self.avail_idx.to_le_bytes());
            head % 16
        }
        fn header(&mut self, kind: u32, sector: u64) -> u64 {
            let mut h = kind.to_le_bytes().to_vec();
            h.extend([0; 4]);
            h.extend(sector.to_le_bytes());
            self.buf(&h)
        }
        fn write(&mut self, sector: u64, data: &[u8]) -> u64 {
            let (h, d, s) = (
                self.header(T_OUT, sector),
                self.buf(data),
                self.buf(&[0xff]),
            );
            self.post(&[(h, 16, false), (d, data.len() as u32, false), (s, 1, true)]);
            s
        }
        fn read(&mut self, sector: u64, len: usize) -> (u64, u64) {
            let (h, d, s) = (
                self.header(T_IN, sector),
                self.buf(&vec![0xee; len]),
                self.buf(&[0xff]),
            );
            self.post(&[(h, 16, false), (d, len as u32, true), (s, 1, true)]);
            (d, s)
        }
        fn flush(&mut self) -> u64 {
            let (h, s) = (self.header(T_FLUSH, 0), self.buf(&[0xff]));
            self.post(&[(h, 16, false), (s, 1, true)]);
            s
        }
        fn notify(&mut self) -> Notified {
            self.dev.write(&mut self.m, 0x050, 0).unwrap()
        }
        fn used_idx(&self) -> u16 {
            rd16(&self.m, USED as usize + 2)
        }
        fn at(&self, a: u64) -> u8 {
            self.m[a as usize]
        }
    }

    #[test]
    fn negotiates_writeback_with_flush_and_no_fua() {
        let mut g = Guest::new(64);
        assert_eq!(g.dev.read(0x008), 2);
        assert_eq!(g.dev.read(0x010), F_FLUSH);
        g.dev.write(&mut g.m, 0x014, 1).unwrap();
        assert_eq!(g.dev.read(0x010), F_VERSION_1);
        assert_eq!((g.dev.read(0x100), g.dev.read(0x104)), (64, 0));
    }

    #[test]
    fn write_flush_overwrite_then_crash_recovers_the_flushed_data() {
        let mut g = Guest::new(64);
        let a = g.write(3, &[b'A'; SECTOR]);
        let f = g.flush();
        let b = g.write(3, &[b'B'; SECTOR]);
        let n = g.notify();
        assert_eq!((n.completed, n.flushed, n.irq), (3, 1, true));
        assert_eq!(
            n.log,
            [(T_OUT, 3, S_OK), (T_FLUSH, 0, S_OK), (T_OUT, 3, S_OK)]
        );
        assert_eq!([g.at(a), g.at(f), g.at(b)], [S_OK; 3]);
        assert_eq!(g.used_idx(), 3);
        // Read-your-writes includes volatile data.
        let (d, s) = g.read(3, SECTOR);
        g.notify();
        assert_eq!((g.at(d), g.at(s)), (b'B', S_OK));
        let (disk, lost) = g.dev.disk().crashed();
        let mut buf = [0u8; SECTOR];
        disk.read(3, &mut buf).unwrap();
        assert_eq!((buf[0], lost), (b'A', 1));
    }

    #[test]
    fn crash_before_the_flush_is_notified_loses_the_write() {
        let mut g = Guest::new(64);
        g.write(0, &[1; SECTOR]);
        g.notify();
        g.flush(); // posted, never notified: the crash comes first
        let (disk, lost) = g.dev.disk().crashed();
        assert_eq!((disk.stable_sectors(), lost), (0, 1));
        g.notify();
        let (disk, lost) = g.dev.disk().crashed();
        assert_eq!((disk.stable_sectors(), lost), (1, 0));
    }

    #[test]
    fn bad_requests_complete_once_with_an_error_and_change_nothing() {
        let mut g = Guest::new(8);
        let before = g.dev.disk().digest();
        let past_end = g.write(8, &[1; SECTOR]);
        let partial = g.write(0, &[1; 100]);
        let (h, s) = (g.header(99, 0), g.buf(&[0xff]));
        g.post(&[(h, 16, false), (s, 1, true)]);
        let (_, read_past) = g.read(7, 2 * SECTOR);
        assert_eq!(g.notify().completed, 4);
        assert_eq!(
            [g.at(past_end), g.at(partial), g.at(s), g.at(read_past)],
            [S_IOERR, S_IOERR, S_UNSUPP, S_IOERR]
        );
        assert_eq!(g.dev.disk().digest(), before);
        assert_eq!(g.dev.stats().errors, 4);
        // Nothing left to complete: a second notify is a no-op.
        assert_eq!(g.notify(), Notified::default());
        assert_eq!(g.used_idx(), 4);
    }

    #[test]
    fn malformed_rings_fail_the_run_instead_of_guessing() {
        let mut g = Guest::new(8);
        let s = g.buf(&[0]);
        g.post(&[(s, 1, true)]); // no header
        assert!(g.dev.write(&mut g.m, 0x050, 0).is_err());
        let mut g = Guest::new(8);
        let h = g.header(T_OUT, 0);
        g.post(&[(h, 16, false), (0xffff_ffff, 512, false)]);
        assert!(g
            .dev
            .write(&mut g.m, 0x050, 0)
            .unwrap_err()
            .contains("outside guest RAM"));
    }

    #[test]
    fn a_device_reset_keeps_the_disk_and_restarts_the_queue() {
        let mut g = Guest::new(8);
        g.write(1, &[7; SECTOR]);
        g.notify();
        g.dev.write(&mut g.m, 0x070, 0).unwrap();
        assert!(!g.dev.transport.queues[0].ready);
        assert_eq!(g.dev.disk().volatile_sectors(), 1);
        assert_eq!(g.dev.stats().writes, 1);
    }

    #[test]
    fn get_id_names_the_disk() {
        let mut g = Guest::new(8);
        let (h, d, s) = (g.header(T_GET_ID, 0), g.buf(&[0; 20]), g.buf(&[0xff]));
        g.post(&[(h, 16, false), (d, 20, true), (s, 1, true)]);
        g.notify();
        assert_eq!(&g.m[d as usize..d as usize + 12], b"seedbox-disk");
        assert_eq!(g.at(s), S_OK);
    }

    #[test]
    fn cloned_device_repeats_dma_and_preserves_journal_across_reset() {
        let mut g = Guest::new(8);
        let first_status = g.write(1, &[7; SECTOR]);
        g.notify();
        let saved = (
            g.dev.clone(),
            g.m.clone(),
            g.next_desc,
            g.avail_idx,
            g.next_buf,
        );
        let (data, status) = g.read(1, SECTOR);
        let expected = g.notify();
        let memory = g.m.clone();
        let dirty = vec![
            (first_status as usize, 1),
            (USED as usize + 4, 8),
            (USED as usize + 2, 2),
            (data as usize, SECTOR),
            (status as usize, 1),
            (USED as usize + 12, 8),
            (USED as usize + 2, 2),
        ];
        assert_eq!(g.dev.dirty, dirty);
        g.dev = saved.0;
        g.m = saved.1;
        g.next_desc = saved.2;
        g.avail_idx = saved.3;
        g.next_buf = saved.4;
        assert_eq!(g.read(1, SECTOR), (data, status));
        assert_eq!(g.notify(), expected);
        assert_eq!(g.m, memory);
        assert_eq!(g.dev.dirty, dirty);
        let stats = g.dev.stats().clone();
        let disk = g.dev.disk().digest();
        g.dev.write(&mut g.m, 0x070, 0).unwrap();
        assert_eq!(g.dev.dirty, dirty);
        assert_eq!(*g.dev.stats(), stats);
        assert_eq!(g.dev.disk().digest(), disk);
        assert_eq!(g.dev.read(0x044), 0);
    }

    #[test]
    fn mutable_disk_access_and_snapshot_keep_stable_and_volatile_layers() {
        let mut dev = VirtioBlk::new(Disk::new(None, Some(8)).unwrap());
        dev.disk_mut().write(0, &[1; SECTOR]).unwrap();
        dev.disk_mut().flush();
        let saved = dev.clone();
        dev.disk_mut().write(0, &[2; SECTOR]).unwrap();
        assert_eq!(dev.disk().volatile_sectors(), 1);
        let mut m = vec![0; 4096];
        dev.write(&mut m, 0x70, 0).unwrap();
        assert_eq!(dev.disk().volatile_sectors(), 1);
        dev = saved;
        let mut bytes = [0; SECTOR];
        dev.disk().read(0, &mut bytes).unwrap();
        assert_eq!(bytes, [1; SECTOR]);
        assert_eq!(dev.disk().volatile_sectors(), 0);
    }
}
