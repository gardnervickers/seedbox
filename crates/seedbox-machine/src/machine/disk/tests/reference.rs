//! Generated operation sequences over a small store, checked step by step
//! against an independent array model of the durable and unflushed layers.
//!
//! The reference keeps one array per layer and shares no code with the store:
//! a read returns the newest completed write, a flush makes every completed
//! write durable, a boot-end loss keeps each latest unflushed sector only when
//! its loss policy picks it, and overlay encodings round-trip both layers
//! exactly over a fresh base. For torn loss the reference re-derives the
//! documented draw contract (`policy.rs`): one draw per latest unflushed
//! sector, in ascending sector order, from the `disk-loss/<name>/<boot>`
//! stream. A mismatch reports the failing operation prefix, which reproduces it.
use super::super::{Error, Store, SECTOR};
use sha2::{Digest, Sha256};

const SECTORS: usize = 8;
const OPERATIONS: usize = 500;
const MACHINE: &str = "store";

/// SplitMix64: a generator local to this test.
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
    /// Write `count` sectors at `sector`, each identifying `tag` and its
    /// index; `extra` bytes make the transfer partial.
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
    /// End the boot under the run's loss policy; the successor starts either
    /// from the returned store or from its encoded overlays over a fresh base.
    Crash {
        encoded: bool,
    },
    /// Encode both overlays into a slot.
    Capture {
        slot: usize,
    },
    /// Replace a fresh base's overlays with a captured slot.
    Restore {
        slot: usize,
    },
}

fn generate(seed: u64) -> Vec<Op> {
    let mut g = Gen(seed);
    let mut tag = 0u8;
    (0..OPERATIONS)
        .map(|_| match g.below(100) {
            0..=39 => {
                tag = tag.wrapping_add(1);
                // Mostly in range; sometimes past the end or partial.
                let extra = if g.below(20) == 0 {
                    1 + g.below(SECTOR as u64 - 1) as usize
                } else {
                    0
                };
                Op::Write {
                    sector: g.below(SECTORS as u64 + 1),
                    count: 1 + g.below(3),
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
                encoded: g.below(2) == 0,
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

/// The boot-end loss policy of one run.
#[derive(Clone, Copy, Debug)]
enum Loss {
    /// `Store::crash` without torn writes: every unflushed sector is lost.
    All,
    /// `Store::crash` with torn writes under `seed`.
    Torn(u64),
    /// A selection that keeps every unflushed sector.
    Nothing,
}

/// The independent model: one array per layer over an immutable base.
#[derive(Clone, Debug)]
struct Reference {
    base: Vec<[u8; SECTOR]>,
    durable: [Option<[u8; SECTOR]>; SECTORS],
    unflushed: [Option<[u8; SECTOR]>; SECTORS],
}

impl Reference {
    fn new() -> Self {
        Self {
            base: base_image()
                .chunks(SECTOR)
                .map(|c| c.try_into().unwrap())
                .collect(),
            durable: [None; SECTORS],
            unflushed: [None; SECTORS],
        }
    }
    fn visible(&self, s: usize) -> [u8; SECTOR] {
        self.unflushed[s]
            .or(self.durable[s])
            .unwrap_or(self.base[s])
    }
    fn check(&self, sector: u64, length: usize) -> Result<(), Error> {
        if !length.is_multiple_of(SECTOR) {
            return Err(Error::Alignment);
        }
        if sector + (length / SECTOR) as u64 > SECTORS as u64 {
            return Err(Error::Range);
        }
        Ok(())
    }
    fn count(layer: &[Option<[u8; SECTOR]>]) -> u64 {
        layer.iter().filter(|s| s.is_some()).count() as u64
    }
    /// Apply a boot-end loss, choosing survivors by the documented contract.
    fn crash(&mut self, loss: Loss, boot: u64) -> u64 {
        let mut torn = match loss {
            Loss::Torn(seed) => {
                let mut key = Sha256::new();
                key.update(b"seedbox-v1");
                key.update(seed.to_le_bytes());
                key.update(format!("disk-loss/{MACHINE}/{boot}").as_bytes());
                Some((key.finalize(), 0u64))
            }
            _ => None,
        };
        let mut lost = 0;
        for s in 0..SECTORS {
            let Some(data) = self.unflushed[s].take() else {
                continue;
            };
            let keep = match (&mut torn, loss) {
                (Some((key, counter)), _) => {
                    let mut draw = Sha256::new();
                    draw.update(*key);
                    draw.update(counter.to_le_bytes());
                    *counter += 1;
                    let draw = u64::from_le_bytes(draw.finalize()[..8].try_into().unwrap());
                    (draw as f64) / 18_446_744_073_709_551_616.0 < 0.5
                }
                (None, Loss::Nothing) => true,
                (None, _) => false,
            };
            if keep {
                self.durable[s] = Some(data);
            } else {
                lost += 1;
            }
        }
        lost
    }
}

fn fresh() -> Store {
    Store::from_image(base_image()).unwrap()
}

fn crash(store: &Store, loss: Loss, boot: u64) -> (Store, u64) {
    match loss {
        Loss::All => store.crash(None, boot).unwrap(),
        Loss::Torn(seed) => store.crash(Some((seed, MACHINE)), boot).unwrap(),
        Loss::Nothing => store.crashed_keeping(|_, _| true),
    }
}

fn digest(store: &Store) -> String {
    store.state_digest(false).unwrap()
}

#[derive(Default)]
struct Stats {
    kept: u64,
    lost: u64,
    torn_crashes: u64,
    restores: u64,
}

/// Execute `ops`, checking the store against the reference after every
/// operation. Returns the index of the first divergent operation.
fn execute(loss: Loss, ops: &[Op], stats: &mut Stats) -> Result<(), (usize, String)> {
    let mut store = fresh();
    let mut model = Reference::new();
    let mut boot = 0u64;
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
                let expected = model.check(sector, data.len());
                let before = digest(&store);
                let actual = store.write(sector, &data);
                if actual != expected {
                    return fail(format!("write returned {actual:?}, reference {expected:?}"));
                }
                if expected.is_err() {
                    if digest(&store) != before {
                        return fail("a rejected write changed the store".into());
                    }
                } else {
                    for i in 0..count {
                        model.unflushed[(sector + i) as usize] = Some(pattern(tag, i));
                    }
                }
            }
            Op::Read {
                sector,
                count,
                extra,
            } => {
                let mut buffer = vec![0x55; count as usize * SECTOR + extra];
                let expected = model.check(sector, buffer.len());
                let actual = store.read(sector, &mut buffer);
                if actual != expected {
                    return fail(format!("read returned {actual:?}, reference {expected:?}"));
                }
                if actual.is_ok() {
                    for i in 0..count as usize {
                        let s = sector as usize + i;
                        if buffer[i * SECTOR..(i + 1) * SECTOR] != model.visible(s) {
                            return fail(format!("read sector {s} differs"));
                        }
                    }
                } else if buffer.iter().any(|&b| b != 0x55) {
                    return fail("a rejected read wrote the buffer".into());
                }
            }
            Op::Flush => {
                for s in 0..SECTORS {
                    if let Some(data) = model.unflushed[s].take() {
                        model.durable[s] = Some(data);
                    }
                }
                store.flush();
            }
            Op::Crash { encoded } => {
                let candidates = Reference::count(&model.unflushed);
                let expected = model.crash(loss, boot);
                let source = digest(&store);
                let (next, lost) = crash(&store, loss, boot);
                if digest(&store) != source {
                    return fail("a crash changed its source".into());
                }
                if lost != expected {
                    return fail(format!("crash lost {lost}, reference {expected}"));
                }
                if lost > 0 && lost < candidates {
                    stats.torn_crashes += 1;
                }
                stats.kept += candidates - lost;
                stats.lost += lost;
                // The same inputs choose the same survivors.
                if digest(&crash(&store, loss, boot).0) != digest(&next) {
                    return fail("the same crash inputs chose different survivors".into());
                }
                store = if encoded {
                    let mut successor = fresh();
                    if let Err(e) = successor.replace_overlays(&next.encode_overlays()) {
                        return fail(format!("successor overlays: {e:?}"));
                    }
                    successor
                } else {
                    next
                };
                boot += 1;
                if !store.unflushed.is_empty() {
                    return fail("a successor booted with unflushed sectors".into());
                }
            }
            Op::Capture { slot } => {
                slots[slot] = Some((store.encode_overlays(), model.clone(), digest(&store)));
            }
            Op::Restore { slot } => {
                let Some((bytes, captured, expected)) = &slots[slot] else {
                    continue;
                };
                store = fresh();
                if let Err(e) = store.replace_overlays(bytes) {
                    return fail(format!("restore: {e:?}"));
                }
                if digest(&store) != *expected {
                    return fail("restored digest differs from the captured store".into());
                }
                model = captured.clone();
                stats.restores += 1;
            }
        }
        let durable = Reference::count(&model.durable);
        let unflushed = Reference::count(&model.unflushed);
        let layers = (store.durable.len() as u64, store.unflushed.len() as u64);
        if layers != (durable, unflushed) {
            return fail(format!(
                "layers hold {layers:?} sectors, reference {durable}/{unflushed}"
            ));
        }
        let mut all = [0; SECTOR * SECTORS];
        store.read(0, &mut all).unwrap();
        for s in 0..SECTORS {
            if all[s * SECTOR..(s + 1) * SECTOR] != model.visible(s) {
                return fail(format!("visible sector {s} differs"));
            }
        }
    }
    Ok(())
}

#[test]
fn generated_store_histories_match_the_reference_model() {
    let mut totals = [(); 3].map(|_| Stats::default());
    for seed in [1u64, 2, 3, 0x5eed, 0xd15c, 1009, 77_777, u64::MAX] {
        let ops = generate(seed);
        for (index, loss) in [Loss::All, Loss::Nothing, Loss::Torn(seed)]
            .into_iter()
            .enumerate()
        {
            if let Err((step, message)) = execute(loss, &ops, &mut totals[index]) {
                panic!(
                    "seed {seed} loss {loss:?}: step {step}: {message}\n\
                     failing prefix ({} operations):\n{:#?}",
                    step + 1,
                    &ops[..=step]
                );
            }
        }
    }
    let [all, nothing, torn] = totals;
    assert_eq!(all.kept, 0, "loss of everything kept an unflushed sector");
    assert!(all.lost > 0 && all.restores > 0);
    assert_eq!(nothing.lost, 0, "keeping everything lost a sector");
    assert!(nothing.kept > 0);
    assert!(
        torn.torn_crashes > 0 && torn.kept > 0 && torn.lost > 0,
        "torn loss never tore a crash"
    );
}
