//! A machine's retained disk: an immutable base image under a stable layer
//! and a volatile layer, in 512-byte sectors.
//!
//! A completed write lands in the volatile layer, where reads see it at once.
//! A completed FLUSH moves every write completed before it into the stable
//! layer before it is acknowledged. A crash drops the whole volatile layer
//! by default. The opt-in torn-write policy keeps a seeded subset of its latest
//! sectors instead. Both keep stable data unless a surviving sector overwrites
//! it. This is durability against a simulated machine crash, not host
//! power loss: nothing here is written to host storage.

mod checkpoint;

use crate::rng::Rng;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::sync::Arc;

pub const SECTOR: usize = 512;

pub use seedbox_engine_protocol::disk::DiskFaultModel;

/// [`fault_model`] for a world document's `disk_loss` held as JSON.
pub fn parse_disk_fault_model(value: &Value) -> Result<DiskFaultModel, &'static str> {
    let model =
        Option::<DiskFaultModel>::deserialize(value).map_err(|_| "unknown disk_loss parameters")?;
    fault_model(model)
}

/// A world's crash-loss policy: absent drops every unflushed sector.
pub fn fault_model(model: Option<DiskFaultModel>) -> Result<DiskFaultModel, &'static str> {
    let model = model.unwrap_or(DiskFaultModel {
        sector_bytes: SECTOR,
        survival_probability: 0.0,
    });
    validate_disk_fault_model(model)?;
    Ok(model)
}

fn validate_disk_fault_model(model: DiskFaultModel) -> Result<(), &'static str> {
    if model.sector_bytes == SECTOR
        && model.survival_probability.is_finite()
        && (0.0..=1.0).contains(&model.survival_probability)
    {
        Ok(())
    } else {
        Err("unknown disk_loss parameters")
    }
}

fn update_fault_model_digest(model: DiskFaultModel, h: &mut Sha256) {
    h.update((model.sector_bytes as u64).to_le_bytes());
    h.update(model.survival_probability.to_bits().to_le_bytes());
}

fn seeded_survival_branch(
    model: DiskFaultModel,
    seed: u64,
    identity: Option<&str>,
    machine: &str,
    incarnation: u32,
) -> Option<Rng> {
    if model.survival_probability == 0.0 {
        None
    } else {
        debug_assert_eq!(model.sector_bytes, SECTOR);
        Some(Rng::branch(
            seed,
            identity,
            &format!("disk-loss/{machine}/{incarnation}"),
        ))
    }
}

fn sector_survives(model: DiskFaultModel, rng: &mut Rng) -> bool {
    let draw = rng.next() as f64 / (u64::MAX as f64 + 1.0);
    draw < model.survival_probability
}

type Sector = Box<[u8; SECTOR]>;

#[derive(Clone)]
pub struct Disk {
    /// None: an all-zero base of `capacity` sectors.
    base: Option<Arc<[u8]>>,
    capacity: u64,
    /// SHA-256 of the image; None for an all-zero base.
    base_sha256: Option<String>,
    stable: BTreeMap<u64, Sector>,
    volatile: BTreeMap<u64, Sector>,
    fault_model: DiskFaultModel,
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
                    return Err(format!(
                        "disk image of {} bytes is not a whole number of {SECTOR}-byte sectors",
                        bytes.len()
                    ));
                }
                let sectors = (bytes.len() / SECTOR) as u64;
                if want.is_some_and(|w| w != sectors) {
                    return Err(format!(
                        "disk image has {sectors} sectors, config says {}",
                        want.unwrap()
                    ));
                }
                (Some(Arc::<[u8]>::from(bytes)), sectors)
            }
            (None, Some(sectors)) if sectors > 0 => (None, sectors),
            (None, _) => return Err("disk needs an image or a size".into()),
        };
        let base_sha256 = base.as_ref().map(crate::sha256_hex);
        Ok(Self {
            base,
            capacity,
            base_sha256,
            stable: BTreeMap::new(),
            volatile: BTreeMap::new(),
            fault_model: DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.0,
            },
        })
    }

    pub fn with_fault_model(mut self, fault_model: DiskFaultModel) -> Self {
        self.fault_model = fault_model;
        self
    }

    pub fn fault_model(&self) -> DiskFaultModel {
        self.fault_model
    }

    /// Each crash has its own stream; no draws from timing, entropy or links.
    /// Incarnation is the one that crashed, not the successor. Recreating the
    /// stream here also makes snapshot/fork replay independent of earlier runs.
    pub fn crashed_seeded(&self, seed: u64, machine: &str, incarnation: u32) -> (Disk, u64) {
        self.crashed_branch(seed, None, machine, incarnation)
    }

    /// A runtime branch keys future loss on its full identity. Continuations
    /// retain the ordinary seedbox-v1 stream. Selection is atomic at crash;
    /// there is no partially consumed survival RNG to capture.
    pub fn crashed_branch(
        &self,
        seed: u64,
        identity: Option<&str>,
        machine: &str,
        incarnation: u32,
    ) -> (Disk, u64) {
        let Some(mut rng) =
            seeded_survival_branch(self.fault_model, seed, identity, machine, incarnation)
        else {
            return self.crashed();
        };
        self.crashed_keeping(|_, _| sector_survives(self.fault_model, &mut rng))
    }

    /// Stream a crash survivor overlay without rebasing its immutable image.
    /// Only a disk whose crash policy has already been applied can be exported.
    pub fn write_crash_transfer(&self, mut out: impl Write) -> Result<(), String> {
        if !self.volatile.is_empty() {
            return Err("crash transfer contains volatile sectors".into());
        }
        validate_disk_fault_model(self.fault_model).map_err(str::to_owned)?;
        let mut write = |bytes: &[u8]| {
            out.write_all(bytes)
                .map_err(|e| format!("write crash transfer: {e}"))
        };
        write(b"SBXDISK1")?;
        write(&self.capacity.to_le_bytes())?;
        write(
            &self
                .fault_model
                .survival_probability
                .to_bits()
                .to_le_bytes(),
        )?;
        write(&[u8::from(self.base_sha256.is_some())])?;
        write(
            self.base_sha256
                .as_deref()
                .unwrap_or(&"0".repeat(64))
                .as_bytes(),
        )?;
        write(&(self.stable.len() as u64).to_le_bytes())?;
        for (sector, data) in &self.stable {
            write(&sector.to_le_bytes())?;
            write(&data[..])?;
        }
        write(self.digest().as_bytes())?;
        out.flush()
            .map_err(|e| format!("flush crash transfer: {e}"))
    }

    /// Validate a survivor overlay against an unmodified configured base.
    /// The returned disk preserves the source digest and future crash behavior.
    /// Malformed input never changes the supplied base or publishes partial data.
    pub fn read_crash_transfer(mut base: Disk, mut input: impl Read) -> Result<Disk, String> {
        fn read<const N: usize>(input: &mut impl Read) -> Result<[u8; N], String> {
            let mut bytes = [0; N];
            input
                .read_exact(&mut bytes)
                .map_err(|e| format!("read crash transfer: {e}"))?;
            Ok(bytes)
        }
        if !base.stable.is_empty() || !base.volatile.is_empty() {
            return Err("crash transfer destination is not a fresh base".into());
        }
        if read::<8>(&mut input)? != *b"SBXDISK1" {
            return Err("invalid crash transfer version".into());
        }
        let capacity = u64::from_le_bytes(read(&mut input)?);
        let survival = u64::from_le_bytes(read(&mut input)?);
        let has_base = read::<1>(&mut input)?[0];
        let base_hash = read::<64>(&mut input)?;
        if capacity != base.capacity
            || survival != base.fault_model.survival_probability.to_bits()
            || has_base != u8::from(base.base_sha256.is_some())
            || base_hash.as_slice()
                != base
                    .base_sha256
                    .as_deref()
                    .unwrap_or(&"0".repeat(64))
                    .as_bytes()
        {
            return Err("crash transfer base, capacity or fault model mismatch".into());
        }
        validate_disk_fault_model(base.fault_model).map_err(str::to_owned)?;
        let count = u64::from_le_bytes(read(&mut input)?);
        if count > capacity {
            return Err("crash transfer sector count exceeds capacity".into());
        }
        let mut previous = None;
        for _ in 0..count {
            let sector = u64::from_le_bytes(read(&mut input)?);
            if sector >= capacity || previous.is_some_and(|p| sector <= p) {
                return Err("crash transfer sectors are unordered or out of range".into());
            }
            base.stable
                .insert(sector, Box::new(read::<SECTOR>(&mut input)?));
            previous = Some(sector);
        }
        let digest = read::<64>(&mut input)?;
        if digest.as_slice() != base.digest().as_bytes() {
            return Err("crash transfer digest mismatch".into());
        }
        let mut trailing = [0];
        if input
            .read(&mut trailing)
            .map_err(|e| format!("read crash transfer trailer: {e}"))?
            != 0
        {
            return Err("crash transfer has trailing data".into());
        }
        Ok(base)
    }

    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    pub fn base_sha256(&self) -> Option<&str> {
        self.base_sha256.as_deref()
    }

    fn check(&self, sector: u64, len: usize) -> Result<u64, DiskError> {
        if !len.is_multiple_of(SECTOR) {
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
            self.volatile
                .insert(sector + i as u64, Box::new(chunk.try_into().unwrap()));
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
    /// Only the latest unflushed version of each sector is a candidate.
    /// The seeded policy decides here, from its own RNG domain. Returns the
    /// next incarnation's disk and the number of volatile sectors lost.
    pub fn crashed_keeping(&self, mut survives: impl FnMut(u64, &[u8]) -> bool) -> (Disk, u64) {
        let mut next = Disk {
            volatile: BTreeMap::new(),
            ..self.clone()
        };
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
    /// with equal digests read the same now and, for equal crash inputs, after a crash.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        update_fault_model_digest(self.fault_model, &mut h);
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
        hex::encode(h.finalize())
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
        assert_eq!(
            [0, 1, 5].map(|s| at(&after, s)),
            [b'A', b'B', 0],
            "sector 1 kept, 0 and 5 lost"
        );
        assert_eq!(after.volatile_sectors(), 0);
        assert_eq!(
            d.crashed_keeping(|_, _| false).0.digest(),
            d.crashed().0.digest()
        );
    }

    #[test]
    fn disabled_policy_preserves_the_original_digest_and_crash_bytes() {
        let mut d = Disk::new(None, Some(8)).unwrap();
        d.write(0, &fill(b'A', 2)).unwrap();
        d.flush();
        d.write(0, &fill(b'B', 4)).unwrap();
        let before = d.digest();
        let baseline = d.crashed();
        for seed in [0, 1009, 1010, u64::MAX] {
            let (after, lost) = d
                .clone()
                .with_fault_model(DiskFaultModel {
                    sector_bytes: SECTOR,
                    survival_probability: 0.0,
                })
                .crashed_seeded(seed, "server", 7);
            assert_eq!(after.digest(), baseline.0.digest());
            assert_eq!(lost, baseline.1);
            assert_eq!(
                after.fault_model(),
                DiskFaultModel {
                    sector_bytes: SECTOR,
                    survival_probability: 0.0
                }
            );
            assert_eq!(
                (0..8).map(|s| at(&after, s)).collect::<Vec<_>>(),
                vec![b'A', b'A', 0, 0, 0, 0, 0, 0]
            );
        }
        assert_eq!(
            d.digest(),
            before,
            "crash selection leaves the source intact"
        );
    }

    #[test]
    fn seeded_survival_is_per_sector_repeatable_and_domain_separated() {
        let mut d = Disk::new(None, Some(32))
            .unwrap()
            .with_fault_model(DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5,
            });
        // One write spanning 32 sectors must be able to tear within the write.
        d.write(0, &fill(b'V', 32)).unwrap();
        let before = d.digest();
        let crash = |seed, name, incarnation| d.crashed_seeded(seed, name, incarnation);
        let (after, lost) = crash(1009, "server", 0);
        let survivors: Vec<_> = (0..32).filter(|&s| at(&after, s) == b'V').collect();
        assert_eq!(
            survivors,
            vec![0, 3, 4, 5, 11, 13, 14, 15, 23, 25, 26, 29, 30]
        );
        assert_eq!(lost, 32 - survivors.len() as u64);
        assert_eq!(after.volatile_sectors(), 0);
        assert_eq!(
            after.fault_model(),
            DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5
            }
        );
        assert_eq!(after.digest(), crash(1009, "server", 0).0.digest());
        assert_ne!(after.digest(), crash(1010, "server", 0).0.digest());
        assert_ne!(after.digest(), crash(1009, "other", 0).0.digest());
        assert_ne!(after.digest(), crash(1009, "server", 1).0.digest());
        assert_eq!(d.digest(), before);
        assert_eq!(
            after.crashed_seeded(1010, "server", 1).0.digest(),
            after.digest(),
            "survivors are stable across later crashes"
        );
        assert_eq!(
            d.clone().crashed_seeded(1009, "server", 0).0.digest(),
            after.digest(),
            "a fork retains the policy and layers"
        );
    }

    #[test]
    fn checkpoint_restore_keeps_survival_inputs_and_branches_future_loss() {
        let mut d = Disk::new(None, Some(64))
            .unwrap()
            .with_fault_model(DiskFaultModel {
                sector_bytes: 512,
                survival_probability: 0.5,
            });
        d.write(0, &fill(b'A', 64)).unwrap();
        d.flush();
        d.write(0, &fill(b'B', 64)).unwrap();
        let saved = d.clone();
        let parent = d.crashed_seeded(7, "server", 3);
        let identities = [
            crate::checkpoint::branch_identity("parent", 100, 42),
            crate::checkpoint::branch_identity("parent", 200, 42),
        ];
        let mut branches = Vec::new();
        for id in [&identities[0], &identities[1], &identities[0]] {
            d = saved.clone();
            assert_eq!(
                d.fault_model(),
                DiskFaultModel {
                    sector_bytes: 512,
                    survival_probability: 0.5
                }
            );
            let (after, lost) = d.crashed_branch(42, Some(id), "server", 3);
            let mut rng = Rng::branch(42, Some(id), "disk-loss/server/3");
            for sector in 0..64 {
                assert_eq!(
                    at(&after, sector),
                    if sector_survives(d.fault_model, &mut rng) {
                        b'B'
                    } else {
                        b'A'
                    }
                );
            }
            assert!(lost > 0 && lost < 64);
            assert_eq!(after.volatile_sectors(), 0);
            branches.push((after.digest(), lost));
        }
        assert_eq!(branches[0], branches[2]);
        assert_ne!(branches[0].0, branches[1].0);
        assert_ne!(branches[0].0, parent.0.digest());
        let restored = saved.crashed_branch(7, None, "server", 3);
        assert_eq!(
            (restored.0.digest(), restored.1),
            (parent.0.digest(), parent.1)
        );
        // A down snapshot has already sampled survival: later branches cannot
        // change those stable survivors without another write.
        assert_eq!(
            parent
                .0
                .crashed_branch(42, Some(&identities[0]), "server", 4)
                .0
                .digest(),
            parent.0.digest()
        );
    }

    #[test]
    fn seeded_loss_only_considers_unflushed_latest_versions() {
        let mut d = Disk::new(Some(fill(b'.', 64)), None)
            .unwrap()
            .with_fault_model(DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5,
            });
        d.write(0, &fill(b'A', 48)).unwrap();
        d.flush();
        d.write(16, &fill(b'B', 32)).unwrap();
        d.write(16, &fill(b'C', 32)).unwrap();
        let (after, lost) = d.crashed_seeded(1009, "server", 0);
        assert!(lost > 0 && lost < 32);
        for s in 0..64 {
            match s {
                0..=15 => assert_eq!(at(&after, s), b'A'),
                16..=47 => assert!(matches!(at(&after, s), b'A' | b'C')),
                _ => assert_eq!(at(&after, s), b'.'),
            }
        }
        let mut shifted = Disk::new(None, Some(32))
            .unwrap()
            .with_fault_model(DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5,
            });
        shifted.write(0, &fill(b'C', 32)).unwrap();
        let shifted = shifted.crashed_seeded(1009, "server", 0).0;
        for s in 0..32 {
            assert_eq!(
                at(&after, s + 16) == b'C',
                at(&shifted, s) == b'C',
                "stable sectors consume no draws"
            );
        }
        d.flush();
        for seed in [0, 1009, 1010] {
            let (after, lost) = d.crashed_seeded(seed, "server", 0);
            assert_eq!(lost, 0);
            assert_eq!(
                after.digest(),
                d.digest(),
                "completed FLUSH covers every earlier write"
            );
        }
    }

    #[test]
    fn every_survivor_subset_satisfies_the_crash_contract() {
        use Op::*;
        let history = [
            W(0, 1),
            W(1, 2),
            W(2, 3),
            F,
            W(0, 4),
            W(2, 5),
            W(2, 6),
            W(5, 7),
        ];
        let stable = oracle(&history);
        let mut latest = BTreeMap::new();
        let mut d = Disk::new(None, Some(8))
            .unwrap()
            .with_fault_model(DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5,
            });
        for op in history {
            match op {
                W(s, b) => {
                    d.write(s, &fill(b, 1)).unwrap();
                    latest.insert(s, b);
                }
                F => {
                    d.flush();
                    latest.clear();
                }
            }
        }
        let sectors: Vec<_> = latest.keys().copied().collect();
        // Exhaust every subset, including later surviving while earlier is lost.
        for mask in 0..1u64 << sectors.len() {
            let keep = |s| mask & (1 << sectors.iter().position(|&v| v == s).unwrap()) != 0;
            let (after, lost) = d.crashed_keeping(|s, _| keep(s));
            let mut want = stable.clone();
            for (&s, &b) in &latest {
                if keep(s) {
                    want[s as usize] = b;
                }
            }
            assert_eq!((0..8).map(|s| at(&after, s)).collect::<Vec<_>>(), want);
            assert_eq!(lost, sectors.len() as u64 - mask.count_ones() as u64);
            assert_eq!(after.volatile_sectors(), 0);
        }
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
        assert_eq!(
            [0, 1, 2, 3, 7].map(|s| at(&d, s)),
            [b'a', b'b', b'c', b'.', b'.']
        );
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
    enum Op {
        W(u64, u8),
        F,
    }

    /// What a crash after `history` must leave: each sector's last write
    /// before the last completed flush, and nothing written after it.
    fn oracle(history: &[Op]) -> Vec<u8> {
        let last_flush = history
            .iter()
            .rposition(|op| matches!(op, Op::F))
            .map_or(0, |i| i + 1);
        let mut want = vec![0u8; 8];
        for op in &history[..last_flush] {
            if let Op::W(s, b) = *op {
                want[s as usize] = b;
            }
        }
        want
    }

    #[derive(Clone, Copy, Debug)]
    enum Model {
        Real,
        EarlyFlushAck,
        VolatileAsWritethrough,
        StableErasure,
    }

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
            Model::StableErasure => Disk {
                stable: BTreeMap::new(),
                ..d.crashed().0
            },
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
        for model in [
            Model::EarlyFlushAck,
            Model::VolatileAsWritethrough,
            Model::StableErasure,
        ] {
            assert!(
                histories.iter().any(|h| crash_after(model, h) != oracle(h)),
                "{model:?} went undetected"
            );
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
        assert_ne!(
            run(1).digest(),
            run(2).digest(),
            "same bytes, different durability"
        );
        assert_eq!(run(1).crashed().0.digest(), run(1).crashed().0.digest());
        let zeros = Disk::new(None, Some(8)).unwrap();
        assert_eq!(zeros.base_sha256(), None);
        assert_ne!(
            zeros.digest(),
            Disk::new(Some(vec![0; 8 * SECTOR]), None).unwrap().digest(),
            "an image is not the zero base"
        );
    }
}

#[cfg(test)]
#[path = "disk_transfer_tests.rs"]
mod transfer_tests;

#[cfg(test)]
mod policy_tests {
    use super::*;
    #[test]
    fn disk_fault_model_is_parameterized_without_a_wire_name() {
        let model = parse_disk_fault_model(&serde_json::json!({
            "sector_bytes": 512,
            "survival_probability": 0.5,
        }))
        .unwrap();
        assert_eq!(
            model,
            DiskFaultModel {
                sector_bytes: SECTOR,
                survival_probability: 0.5
            }
        );
    }
}
