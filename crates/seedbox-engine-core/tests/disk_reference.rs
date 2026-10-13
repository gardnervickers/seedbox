//! Generated operation sequences over a small disk, checked step by step
//! against an independent array model of the stable and volatile layers.
//!
//! The reference keeps one array per layer and never shares code with
//! `disk.rs`: a read returns the newest completed write, a flush makes every
//! completed write stable, a crash keeps each unflushed latest version only
//! when the survival policy picks it, and checkpoints and crash transfers
//! round-trip both layers exactly. For survival probabilities strictly
//! between zero and one the reference mirrors the documented draw contract:
//! one draw per volatile sector, in sector order, from the crashed
//! incarnation's `disk-loss/{machine}/{incarnation}` stream. A mismatch
//! reports the shortest failing operation prefix, which reproduces it.
use seedbox_engine_core::disk::{Disk, DiskError, DiskFaultModel, SECTOR};
use seedbox_engine_core::rng::Rng;

const SECTORS: usize = 8;
const OPS: usize = 500;
const MACHINE: &str = "store";

/// SplitMix64: a small generator local to this test, independent of `Rng`.
struct Gen(u64);
impl Gen {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// Write `count` sectors at `sector`, each filled from `tag`; `extra`
    /// bytes make the transfer partial.
    Write {
        sector: u64,
        count: u64,
        tag: u8,
        extra: usize,
    },
    Read {
        sector: u64,
        count: u64,
        extra: usize,
    },
    Flush,
    /// Crash with the disk's survival policy and boot the successor, either
    /// directly or through the crash-transfer format.
    Crash {
        transfer: bool,
    },
    /// Capture a checkpoint into a slot.
    Capture {
        slot: usize,
    },
    /// Restore a captured slot over a fresh base.
    Restore {
        slot: usize,
    },
}

fn generate(seed: u64) -> Vec<Op> {
    let mut g = Gen(seed);
    let mut tag = 0u8;
    (0..OPS)
        .map(|_| match g.below(100) {
            0..=39 => {
                tag = tag.wrapping_add(1);
                // Mostly in range; sometimes past the end or partial.
                let sector = g.below(SECTORS as u64 + 1);
                let count = 1 + g.below(3);
                let extra = if g.below(20) == 0 {
                    1 + g.below(SECTOR as u64 - 1) as usize
                } else {
                    0
                };
                Op::Write {
                    sector,
                    count,
                    tag,
                    extra,
                }
            }
            40..=59 => Op::Read {
                sector: g.below(SECTORS as u64 + 1),
                count: 1 + g.below(SECTORS as u64),
                extra: if g.below(20) == 0 { 7 } else { 0 },
            },
            60..=74 => Op::Flush,
            75..=84 => Op::Crash {
                transfer: g.below(2) == 0,
            },
            85..=92 => Op::Capture {
                slot: g.below(3) as usize,
            },
            _ => Op::Restore {
                slot: g.below(3) as usize,
            },
        })
        .collect()
}

/// Sector contents that identify the write and the sector inside it.
fn pattern(tag: u8, index: u64) -> [u8; SECTOR] {
    let mut bytes = [0; SECTOR];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = tag ^ (index as u8).wrapping_mul(37) ^ (i as u8).wrapping_mul(11);
    }
    bytes[0] = tag;
    bytes[1] = index as u8;
    bytes
}

fn base_image() -> Vec<u8> {
    (0..SECTORS * SECTOR)
        .map(|i| (i / SECTOR) as u8 | 0x80)
        .collect()
}

/// The independent model: one array per layer over an immutable base.
#[derive(Clone, PartialEq, Debug)]
struct Reference {
    base: Vec<[u8; SECTOR]>,
    stable: [Option<[u8; SECTOR]>; SECTORS],
    volatile: [Option<[u8; SECTOR]>; SECTORS],
}

impl Reference {
    fn new() -> Self {
        let image = base_image();
        Self {
            base: image
                .chunks(SECTOR)
                .map(|c| c.try_into().unwrap())
                .collect(),
            stable: [None; SECTORS],
            volatile: [None; SECTORS],
        }
    }
    fn visible(&self, s: usize) -> [u8; SECTOR] {
        self.volatile[s].or(self.stable[s]).unwrap_or(self.base[s])
    }
    fn after_crash(&self) -> [u8; SECTOR * SECTORS] {
        let mut out = [0; SECTOR * SECTORS];
        for s in 0..SECTORS {
            out[s * SECTOR..(s + 1) * SECTOR]
                .copy_from_slice(&self.stable[s].unwrap_or(self.base[s]));
        }
        out
    }
    fn valid(&self, sector: u64, len: usize) -> Result<u64, DiskError> {
        if !len.is_multiple_of(SECTOR) {
            return Err(DiskError::Partial);
        }
        let count = (len / SECTOR) as u64;
        if sector + count > SECTORS as u64 {
            return Err(DiskError::Range);
        }
        Ok(count)
    }
    /// Survivors chosen by the documented draw contract.
    fn crash(
        &mut self,
        probability: f64,
        seed: u64,
        identity: Option<&str>,
        incarnation: u32,
    ) -> u64 {
        let mut stream = (probability != 0.0).then(|| {
            Rng::branch(
                seed,
                identity,
                &format!("disk-loss/{MACHINE}/{incarnation}"),
            )
        });
        let mut lost = 0;
        for s in 0..SECTORS {
            let Some(data) = self.volatile[s].take() else {
                continue;
            };
            let keep = match &mut stream {
                None => false,
                Some(_) if probability >= 1.0 => {
                    stream.as_mut().unwrap().next();
                    true
                }
                Some(rng) => (rng.next() as f64) / 18_446_744_073_709_551_616.0 < probability,
            };
            if keep {
                self.stable[s] = Some(data);
            } else {
                lost += 1;
            }
        }
        lost
    }
}

struct Run {
    probability: f64,
    seed: u64,
    identity: Option<&'static str>,
}

fn fresh(probability: f64) -> Disk {
    Disk::new(Some(base_image()), Some(SECTORS as u64))
        .unwrap()
        .with_fault_model(DiskFaultModel {
            sector_bytes: SECTOR,
            survival_probability: probability,
        })
}

/// Execute `ops`, checking the disk against the reference after every
/// operation. Returns the index of the first divergent operation.
fn execute(run: &Run, ops: &[Op], stats: &mut Stats) -> Result<(), (usize, String)> {
    let mut disk = fresh(run.probability);
    let mut model = Reference::new();
    let mut incarnation = 0u32;
    let mut slots: [Option<(Vec<u8>, Reference, String)>; 3] = [None, None, None];
    for (step, op) in ops.iter().enumerate() {
        let fail = |message: String| Err((step, message));
        match *op {
            Op::Write {
                sector,
                count,
                tag,
                extra,
            } => {
                let mut data = Vec::new();
                for i in 0..count {
                    data.extend_from_slice(&pattern(tag, i));
                }
                data.extend(std::iter::repeat_n(0xee, extra));
                let expected = model.valid(sector, data.len());
                let before = disk.digest();
                let actual = disk.write(sector, &data);
                if actual.map(|_| ()) != expected.map(|_| ()) {
                    return fail(format!("write returned {actual:?}, reference {expected:?}"));
                }
                if expected.is_err() {
                    if disk.digest() != before {
                        return fail("a rejected write changed the disk".into());
                    }
                } else {
                    for i in 0..count {
                        model.volatile[(sector + i) as usize] = Some(pattern(tag, i));
                    }
                }
            }
            Op::Read {
                sector,
                count,
                extra,
            } => {
                let mut buf = vec![0x55; count as usize * SECTOR + extra];
                let expected = model.valid(sector, buf.len());
                let actual = disk.read(sector, &mut buf);
                if actual != expected.map(|_| ()) {
                    return fail(format!("read returned {actual:?}, reference {expected:?}"));
                }
                if actual.is_ok() {
                    for i in 0..count as usize {
                        if buf[i * SECTOR..(i + 1) * SECTOR] != model.visible(sector as usize + i) {
                            return fail(format!("sector {} differs", sector as usize + i));
                        }
                    }
                } else if buf.iter().any(|&b| b != 0x55) {
                    return fail("a rejected read wrote the buffer".into());
                }
            }
            Op::Flush => {
                let expected = model.volatile.iter().filter(|s| s.is_some()).count() as u64;
                for s in 0..SECTORS {
                    if let Some(data) = model.volatile[s].take() {
                        model.stable[s] = Some(data);
                    }
                }
                let moved = disk.flush();
                if moved != expected {
                    return fail(format!("flush moved {moved}, reference {expected}"));
                }
            }
            Op::Crash { transfer } => {
                let candidates = model.volatile.iter().filter(|s| s.is_some()).count() as u64;
                let expected = model.crash(run.probability, run.seed, run.identity, incarnation);
                let source = disk.digest();
                let (next, lost) =
                    disk.crashed_branch(run.seed, run.identity, MACHINE, incarnation);
                if disk.digest() != source {
                    return fail("crash changed its source disk".into());
                }
                if lost != expected {
                    return fail(format!("crash lost {lost}, reference {expected}"));
                }
                if lost > 0 && lost < candidates {
                    stats.partial_crashes += 1;
                }
                stats.kept += candidates - lost;
                stats.lost += lost;
                // Repeating the crash from the same inputs picks the same survivors.
                if disk
                    .crashed_branch(run.seed, run.identity, MACHINE, incarnation)
                    .0
                    .digest()
                    != next.digest()
                {
                    return fail("the same crash inputs chose different survivors".into());
                }
                disk = if transfer {
                    let mut bytes = Vec::new();
                    if let Err(e) = next.write_crash_transfer(&mut bytes) {
                        return fail(e);
                    }
                    match Disk::read_crash_transfer(fresh(run.probability), bytes.as_slice()) {
                        Ok(restored) => restored,
                        Err(e) => return fail(e),
                    }
                } else {
                    next
                };
                incarnation += 1;
                if disk.volatile_sectors() != 0 {
                    return fail("a successor booted with volatile sectors".into());
                }
                let mut all = [0; SECTOR * SECTORS];
                disk.read(0, &mut all).unwrap();
                if all != model.after_crash() {
                    return fail("successor contents differ".into());
                }
            }
            Op::Capture { slot } => {
                let mut bytes = Vec::new();
                if let Err(e) = disk.write_checkpoint(&mut bytes) {
                    return fail(e);
                }
                slots[slot] = Some((bytes, model.clone(), disk.digest()));
            }
            Op::Restore { slot } => {
                let Some((bytes, captured, digest)) = &slots[slot] else {
                    continue;
                };
                disk = match Disk::read_checkpoint(fresh(run.probability), bytes.as_slice()) {
                    Ok(restored) => restored,
                    Err(e) => return fail(e),
                };
                if disk.digest() != *digest {
                    return fail("restored digest differs from the captured disk".into());
                }
                model = captured.clone();
                stats.restores += 1;
            }
        }
        let stable = model.stable.iter().filter(|s| s.is_some()).count() as u64;
        let volatile = model.volatile.iter().filter(|s| s.is_some()).count() as u64;
        if (disk.stable_sectors(), disk.volatile_sectors()) != (stable, volatile) {
            return fail(format!(
                "layers hold {}/{} sectors, reference {stable}/{volatile}",
                disk.stable_sectors(),
                disk.volatile_sectors()
            ));
        }
        let mut all = [0; SECTOR * SECTORS];
        disk.read(0, &mut all).unwrap();
        for s in 0..SECTORS {
            if all[s * SECTOR..(s + 1) * SECTOR] != model.visible(s) {
                return fail(format!("visible sector {s} differs"));
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct Stats {
    kept: u64,
    lost: u64,
    partial_crashes: u64,
    restores: u64,
}

#[test]
fn generated_disk_histories_match_the_reference_model() {
    let mut totals = [(); 3].map(|_| Stats::default());
    for seed in [1u64, 2, 3, 0x5eed, 0xd15c, 1009, 77_777, u64::MAX] {
        let ops = generate(seed);
        for (index, probability) in [0.0, 1.0, 0.5].into_iter().enumerate() {
            for identity in [None, Some("branch-a")] {
                let run = Run {
                    probability,
                    seed,
                    identity,
                };
                if let Err((step, message)) = execute(&run, &ops, &mut totals[index]) {
                    panic!(
                        "seed {seed} probability {probability} identity {identity:?}: \
                         step {step}: {message}\nfailing prefix ({} operations):\n{:#?}",
                        step + 1,
                        &ops[..=step]
                    );
                }
            }
        }
    }
    let [none, all, half] = totals;
    assert_eq!(none.kept, 0, "probability 0 kept a volatile sector");
    assert!(none.lost > 0 && none.restores > 0);
    assert_eq!(all.lost, 0, "probability 1 lost a volatile sector");
    assert!(all.kept > 0);
    assert!(
        half.partial_crashes > 0 && half.kept > 0 && half.lost > 0,
        "probability 0.5 never tore a crash"
    );
}
