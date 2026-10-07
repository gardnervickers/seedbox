//! A machine's retained disk: an immutable base image under a stable layer
//! and a volatile layer, in 512-byte sectors.
//!
//! A completed write lands in the volatile layer, where reads see it at once.
//! A completed FLUSH moves every write completed before it into the stable
//! layer before it is acknowledged. A crash drops the whole volatile layer
//! (drop-all-unflushed) and keeps the stable one: that is what the next
//! incarnation of the machine boots with. No torn writes, no seeded surviving
//! subsets. This is durability against a simulated machine crash, not host
//! power loss: nothing here is written to host storage.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::rc::Rc;

pub const SECTOR: usize = 512;
/// Version of these semantics (layers, flush coverage, crash loss policy),
/// reported with the disk. Changing them changes runs.
pub const MODEL: &str = "flush-stable/drop-all-unflushed/v1";

type Sector = Box<[u8; SECTOR]>;

#[derive(Clone)]
pub struct Disk {
    /// None: an all-zero base of `capacity` sectors.
    base: Option<Rc<[u8]>>,
    capacity: u64,
    /// SHA-256 of the image; None for an all-zero base.
    base_sha256: Option<String>,
    stable: BTreeMap<u64, Sector>,
    volatile: BTreeMap<u64, Sector>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    /// The request reaches past the end of the disk.
    Range,
    /// The transfer is not a whole number of sectors.
    Partial,
}

impl Disk {
    /// A disk over `image`, or over zeros when there is none. An image must be
    /// a whole number of sectors and, if `capacity` is given, exactly that big.
    pub fn new(image: Option<Vec<u8>>, capacity: Option<u64>) -> Result<Self, String> {
        let (base, capacity) = match (image, capacity) {
            (Some(bytes), want) => {
                if bytes.len() % SECTOR != 0 || bytes.is_empty() {
                    return Err(format!("disk image of {} bytes is not a whole number of {SECTOR}-byte sectors", bytes.len()));
                }
                let sectors = (bytes.len() / SECTOR) as u64;
                if want.is_some_and(|w| w != sectors) {
                    return Err(format!("disk image has {sectors} sectors, config says {}", want.unwrap()));
                }
                (Some(Rc::<[u8]>::from(bytes)), sectors)
            }
            (None, Some(sectors)) if sectors > 0 => (None, sectors),
            (None, _) => return Err("disk needs an image or a size".into()),
        };
        let base_sha256 = base.as_ref().map(|bytes| format!("{:x}", Sha256::digest(bytes)));
        Ok(Self { base, capacity, base_sha256, stable: BTreeMap::new(), volatile: BTreeMap::new() })
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn base_sha256(&self) -> Option<&str> {
        self.base_sha256.as_deref()
    }

    fn check(&self, sector: u64, len: usize) -> Result<u64, DiskError> {
        if len % SECTOR != 0 {
            return Err(DiskError::Partial);
        }
        let n = (len / SECTOR) as u64;
        match sector.checked_add(n) {
            Some(end) if end <= self.capacity => Ok(n),
            _ => Err(DiskError::Range),
        }
    }

    fn sector(&self, s: u64) -> &[u8] {
        if let Some(v) = self.volatile.get(&s).or_else(|| self.stable.get(&s)) {
            return &v[..];
        }
        match &self.base {
            Some(b) => &b[s as usize * SECTOR..(s as usize + 1) * SECTOR],
            None => &[0u8; SECTOR],
        }
    }

    /// Read whole sectors from `sector` into `buf`: the newest completed
    /// write, flushed or not.
    pub fn read(&self, sector: u64, buf: &mut [u8]) -> Result<(), DiskError> {
        self.check(sector, buf.len())?;
        for (i, out) in buf.chunks_mut(SECTOR).enumerate() {
            out.copy_from_slice(self.sector(sector + i as u64));
        }
        Ok(())
    }

    /// Complete a write of whole sectors. It is volatile until a flush.
    pub fn write(&mut self, sector: u64, data: &[u8]) -> Result<(), DiskError> {
        self.check(sector, data.len())?;
        for (i, chunk) in data.chunks(SECTOR).enumerate() {
            self.volatile.insert(sector + i as u64, Box::new(chunk.try_into().unwrap()));
        }
        Ok(())
    }

    /// Complete a FLUSH: every write completed so far becomes stable. Returns
    /// how many sectors moved.
    pub fn flush(&mut self) -> u64 {
        let n = self.volatile.len() as u64;
        self.stable.append(&mut self.volatile);
        n
    }

    /// The disk the next incarnation boots with after a crash: the stable
    /// layer only. Returns it with the number of volatile sectors lost.
    pub fn crashed(&self) -> (Disk, u64) {
        self.crashed_keeping(|_, _| false)
    }

    /// A crash that keeps the volatile sectors `survives` picks, in sector
    /// order, as if the device had written them back before power was lost.
    /// The current model keeps none. A later loss policy (seeded partial
    /// persistence, post-V0) decides here, from its own RNG domain. Returns the
    /// next incarnation's disk and the number of volatile sectors lost.
    pub fn crashed_keeping(&self, mut survives: impl FnMut(u64, &[u8]) -> bool) -> (Disk, u64) {
        let mut next = Disk { volatile: BTreeMap::new(), ..self.clone() };
        let mut lost = 0;
        for (&s, data) in &self.volatile {
            if survives(s, &data[..]) {
                next.stable.insert(s, data.clone());
            } else {
                lost += 1;
            }
        }
        (next, lost)
    }

    pub fn stable_sectors(&self) -> u64 {
        self.stable.len() as u64
    }

    pub fn volatile_sectors(&self) -> u64 {
        self.volatile.len() as u64
    }

    /// Hash of the base image and both layers, sector by sector. Two disks
    /// with equal digests read the same now and after a crash.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        h.update(MODEL.as_bytes());
        h.update(self.capacity.to_le_bytes());
        h.update(self.base_sha256.as_deref().unwrap_or("zeros").as_bytes());
        for (tag, layer) in [(b's', &self.stable), (b'v', &self.volatile)] {
            h.update([tag]);
            h.update((layer.len() as u64).to_le_bytes());
            for (s, data) in layer {
                h.update(s.to_le_bytes());
                h.update(&data[..]);
            }
        }
        format!("{:x}", h.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fill(b: u8, sectors: usize) -> Vec<u8> {
        vec![b; sectors * SECTOR]
    }
    fn at(d: &Disk, s: u64) -> u8 {
        let mut buf = [0u8; SECTOR];
        d.read(s, &mut buf).unwrap();
        assert!(buf.iter().all(|&x| x == buf[0]), "sector {s} is torn");
        buf[0]
    }

    #[test]
    fn a_flushed_write_survives_a_crash_and_a_later_unflushed_overwrite_does_not() {
        let mut d = Disk::new(None, Some(16)).unwrap();
        d.write(0, &fill(b'A', 1)).unwrap();
        // Crash before the flush completes: A was never stable.
        assert_eq!(at(&d.crashed().0, 0), 0);
        assert_eq!(d.flush(), 1);
        d.write(0, &fill(b'B', 1)).unwrap();
        assert_eq!(at(&d, 0), b'B', "reads see volatile data");
        let (after, lost) = d.crashed();
        assert_eq!((at(&after, 0), lost), (b'A', 1));
        assert_eq!((after.stable_sectors(), after.volatile_sectors()), (1, 0));
    }

    #[test]
    fn a_flush_covers_every_write_before_it_and_none_after() {
        let mut d = Disk::new(None, Some(16)).unwrap();
        d.write(1, &fill(b'1', 2)).unwrap();
        d.write(5, &fill(b'2', 1)).unwrap();
        d.write(1, &fill(b'3', 1)).unwrap(); // same-sector overwrite before the flush
        assert_eq!(d.flush(), 3);
        d.write(2, &fill(b'4', 1)).unwrap();
        d.write(9, &fill(b'5', 1)).unwrap();
        let (after, lost) = d.crashed();
        assert_eq!(lost, 2);
        assert_eq!([1, 2, 5, 9].map(|s| at(&after, s)), [b'3', b'1', b'2', 0]);
    }

    #[test]
    fn a_crash_may_keep_chosen_volatile_sectors_over_stable_ones() {
        let mut d = Disk::new(None, Some(8)).unwrap();
        d.write(0, &fill(b'A', 2)).unwrap();
        d.flush();
        d.write(0, &fill(b'B', 2)).unwrap();
        d.write(5, &fill(b'C', 1)).unwrap();
        let (after, lost) = d.crashed_keeping(|s, data| s == 1 && data[0] == b'B');
        assert_eq!(lost, 2);
        assert_eq!([0, 1, 5].map(|s| at(&after, s)), [b'A', b'B', 0], "sector 1 kept, 0 and 5 lost");
        assert_eq!(after.volatile_sectors(), 0);
        assert_eq!(d.crashed_keeping(|_, _| false).0.digest(), d.crashed().0.digest());
    }

    #[test]
    fn repeated_crashes_keep_stable_data_and_lose_only_new_volatile_data() {
        let mut d = Disk::new(Some(fill(b'.', 8)), None).unwrap();
        for round in 0..3u8 {
            d.write(round as u64, &fill(b'a' + round, 1)).unwrap();
            d.flush();
            d.write(7, &fill(b'x', 1)).unwrap();
            d = d.crashed().0;
        }
        assert_eq!([0, 1, 2, 3, 7].map(|s| at(&d, s)), [b'a', b'b', b'c', b'.', b'.']);
    }

    #[test]
    fn out_of_range_and_partial_requests_change_nothing() {
        let mut d = Disk::new(None, Some(4)).unwrap();
        let before = d.digest();
        assert_eq!(d.write(3, &fill(1, 2)), Err(DiskError::Range));
        assert_eq!(d.write(u64::MAX, &fill(1, 1)), Err(DiskError::Range));
        assert_eq!(d.write(0, &[1; 100]), Err(DiskError::Partial));
        assert_eq!(d.read(4, &mut [0; SECTOR]), Err(DiskError::Range));
        assert_eq!(d.digest(), before);
        assert!(Disk::new(Some(vec![0; 100]), None).is_err());
        assert!(Disk::new(Some(vec![0; 1024]), Some(3)).is_err());
        assert!(Disk::new(None, None).is_err());
    }

    #[derive(Clone, Copy)]
    enum Op { W(u64, u8), F }

    /// What a crash after `history` must leave: each sector's last write
    /// before the last completed flush, and nothing written after it.
    fn oracle(history: &[Op]) -> Vec<u8> {
        let last_flush = history.iter().rposition(|op| matches!(op, Op::F)).map_or(0, |i| i + 1);
        let mut want = vec![0u8; 8];
        for op in &history[..last_flush] {
            if let Op::W(s, b) = *op {
                want[s as usize] = b;
            }
        }
        want
    }

    #[derive(Clone, Copy, Debug)]
    enum Model { Real, EarlyFlushAck, VolatileAsWritethrough, StableErasure }

    fn crash_after(model: Model, history: &[Op]) -> Vec<u8> {
        let mut d = Disk::new(None, Some(8)).unwrap();
        for op in history {
            match (*op, model) {
                (Op::W(s, b), _) => d.write(s, &fill(b, 1)).unwrap(),
                (Op::F, Model::EarlyFlushAck) => {} // acknowledged, never made stable
                (Op::F, _) => _ = d.flush(),
            }
        }
        let after = match model {
            Model::VolatileAsWritethrough => d.clone(),
            Model::StableErasure => Disk { stable: BTreeMap::new(), ..d.crashed().0 },
            _ => d.crashed().0,
        };
        (0..8).map(|s| at(&after, s)).collect()
    }

    #[test]
    fn the_crash_oracle_accepts_the_model_and_rejects_each_broken_one() {
        use Op::*;
        let histories: [&[Op]; 5] = [
            &[W(0, 1), F, W(0, 2)],
            &[W(0, 1), W(1, 2), F, W(2, 3), W(0, 4)],
            &[W(3, 1), F, W(3, 2), F, W(3, 3)],
            &[W(5, 9)],
            &[W(1, 1), F, F, W(1, 2), W(1, 3)],
        ];
        for h in histories {
            assert_eq!(crash_after(Model::Real, h), oracle(h), "{:?}", Model::Real);
        }
        for model in [Model::EarlyFlushAck, Model::VolatileAsWritethrough, Model::StableErasure] {
            assert!(histories.iter().any(|h| crash_after(model, h) != oracle(h)), "{model:?} went undetected");
        }
    }

    #[test]
    fn the_digest_separates_stable_from_volatile_and_matches_equal_histories() {
        let run = |flush_at: usize| {
            let mut d = Disk::new(None, Some(8)).unwrap();
            for i in 0..4 {
                d.write(i as u64, &fill(i as u8 + 1, 1)).unwrap();
                if i == flush_at {
                    d.flush();
                }
            }
            d
        };
        assert_eq!(run(1).digest(), run(1).digest());
        assert_ne!(run(1).digest(), run(2).digest(), "same bytes, different durability");
        assert_eq!(run(1).crashed().0.digest(), run(1).crashed().0.digest());
        let zeros = Disk::new(None, Some(8)).unwrap();
        assert_eq!(zeros.base_sha256(), None);
        assert_ne!(zeros.digest(), Disk::new(Some(vec![0; 8 * SECTOR]), None).unwrap().digest(), "an image is not the zero base");
    }
}
