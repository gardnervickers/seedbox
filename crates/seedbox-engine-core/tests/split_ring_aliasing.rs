//! Block requests over the shared split ring, checked against an independent
//! sequential oracle (NEXT-100 case 79).
//!
//! The oracle re-implements the documented rules from scratch: each chain is
//! validated from current RAM before any mutation, readable bytes are copied
//! before any write, payload bytes are written in chain order, then the status
//! byte, the used element and the used index, and a later malformed chain does
//! not undo the completed prefix. Every guest-visible byte, the dirty journal
//! order, cursors, the interrupt status and both disk layers must match after
//! every step.
//!
//! Generated sequences run in 8 KiB of RAM: payloads alias descriptors, avail
//! and used entries of requests posted later in the same batch, heads are
//! corrupted and repaired, the device is reset onto fresh rings, and a captured
//! state is restored through the serialized transport, block and disk
//! checkpoints. A mismatch reports the seed and the failing operation prefix.
use seedbox_engine_core::{
    disk::{Disk, SECTOR},
    virtio_block::{Block, Policy as BlockPolicy},
    virtio_mmio::{Effect, Policy, Spec, Transport},
};
use std::collections::BTreeMap;

const RAM: usize = 0x2000;
const SECTORS: u64 = 16;
const SPEC: Spec = Spec {
    device_id: 2,
    features: [1 << 9, 1],
};
const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const T_GET_ID: u32 = 8;
const ID: &[u8; 20] = b"seedbox-disk\0\0\0\0\0\0\0\0";
/// Data buffers live between the two ring layouts.
const DATA: (usize, usize) = (0x200, 0x1e00);

/// SplitMix64, local to this test.
#[derive(Clone)]
struct Rng(u64);
impl Rng {
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
    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

fn rd16(m: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(m[at..at + 2].try_into().unwrap())
}
fn rd32(m: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(m[at..at + 4].try_into().unwrap())
}
fn rd64(m: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(m[at..at + 8].try_into().unwrap())
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    num: u16,
    desc: usize,
    avail: usize,
    used: usize,
}
const LAYOUTS: [Layout; 2] = [
    Layout {
        num: 8,
        desc: 0x0,
        avail: 0x80,
        used: 0xc0,
    },
    Layout {
        num: 4,
        desc: 0x1e00,
        avail: 0x1e40,
        used: 0x1e80,
    },
];
impl Layout {
    fn rings(&self) -> [(usize, usize); 3] {
        let n = self.num as usize;
        [
            (self.desc, 16 * n),
            (self.avail, 6 + 2 * n),
            (self.used, 6 + 8 * n),
        ]
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// A guest store.
    Write(usize, Vec<u8>),
    /// QUEUE_NOTIFY for the request queue.
    Notify,
    /// Acknowledge the interrupt through InterruptACK.
    Ack,
    /// Reset through Status and negotiate fresh rings at this layout.
    Reset(usize),
    Capture,
    Restore,
}

// ------------------------------------------------------------ device side

struct Device {
    transport: Transport,
    block: Block,
    memory: Vec<u8>,
    dirty: Vec<(usize, usize)>,
}

#[derive(Clone)]
struct DeviceCapture {
    transport: String,
    block: String,
    disk: Vec<u8>,
    memory: Vec<u8>,
}

fn fresh_disk() -> Disk {
    Disk::new(None, Some(SECTORS)).unwrap()
}

impl Device {
    fn new(policy: BlockPolicy) -> Self {
        Self {
            transport: Transport::new(SPEC, 1, mmio(policy)),
            block: Block::new(fresh_disk(), policy),
            memory: vec![0; RAM],
            dirty: Vec::new(),
        }
    }
    fn mmio(&mut self, offset: u64, value: u32) -> Result<(), String> {
        match self.transport.write(offset, value)? {
            Effect::Notify(0) => {
                let base = 0;
                let dirty = &mut self.dirty;
                self.block
                    .notify(&mut self.transport, &mut self.memory, base, 0, |a, n| {
                        dirty.push((a, n))
                    })
                    .map(|_| ())
            }
            // Adapters retain the block model and its disk across a reset.
            Effect::Reset | Effect::None => Ok(()),
            Effect::Notify(_) => unreachable!("one queue"),
        }
    }
    fn negotiate(&mut self, l: Layout) {
        for (offset, value) in [
            (0x70, 0),
            (0x70, 1),
            (0x70, 3),
            (0x24, 1),
            (0x20, 1),
            (0x24, 0),
            (0x20, 0),
            (0x70, 0xb),
            (0x30, 0),
            (0x38, l.num as u32),
            (0x80, l.desc as u32),
            (0x84, 0),
            (0x90, l.avail as u32),
            (0x94, 0),
            (0xa0, l.used as u32),
            (0xa4, 0),
            (0x44, 1),
            (0x70, 0xf),
        ] {
            self.mmio(offset, value).unwrap();
        }
    }
    fn capture(&self) -> DeviceCapture {
        let mut disk = Vec::new();
        self.block.disk().write_checkpoint(&mut disk).unwrap();
        DeviceCapture {
            transport: serde_json::to_string(&self.transport.snapshot()).unwrap(),
            block: serde_json::to_string(&self.block.snapshot().unwrap()).unwrap(),
            disk,
            memory: self.memory.clone(),
        }
    }
    fn restore(&mut self, saved: &DeviceCapture, policy: BlockPolicy) {
        let disk = Disk::read_checkpoint(fresh_disk(), saved.disk.as_slice()).unwrap();
        self.block = Block::new(disk, policy)
            .restore_snapshot(serde_json::from_str(&saved.block).unwrap())
            .unwrap();
        self.transport = Transport::new(SPEC, 1, mmio(policy))
            .restore_snapshot(serde_json::from_str(&saved.transport).unwrap())
            .unwrap();
        self.memory = saved.memory.clone();
    }
}

fn mmio(policy: BlockPolicy) -> Policy {
    match policy {
        BlockPolicy::Application => Policy::Application,
        BlockPolicy::Block => Policy::Block,
    }
}

// ------------------------------------------------------------ oracle side

/// A sequential reading of the split-ring and block rules, sharing no code.
#[derive(Clone)]
struct Oracle {
    memory: Vec<u8>,
    layout: Option<Layout>,
    last_avail: u16,
    used_idx: u16,
    isr: u32,
    stable: BTreeMap<u64, Vec<u8>>,
    volatile: BTreeMap<u64, Vec<u8>>,
    dirty: Vec<(usize, usize)>,
    /// Payload stores into a ring or descriptor table during the last notify.
    ring_payload: usize,
}

struct Segment {
    at: usize,
    len: usize,
    writable: bool,
}

impl Oracle {
    fn new() -> Self {
        Self {
            memory: vec![0; RAM],
            layout: None,
            last_avail: 0,
            used_idx: 0,
            isr: 0,
            stable: BTreeMap::new(),
            volatile: BTreeMap::new(),
            dirty: Vec::new(),
            ring_payload: 0,
        }
    }
    fn sector(&self, s: u64) -> Vec<u8> {
        self.volatile
            .get(&s)
            .or_else(|| self.stable.get(&s))
            .cloned()
            .unwrap_or_else(|| vec![0; SECTOR])
    }
    fn range(sector: u64, len: usize) -> Option<u64> {
        if !len.is_multiple_of(SECTOR) {
            return None;
        }
        let n = (len / SECTOR) as u64;
        sector
            .checked_add(n)
            .filter(|end| *end <= SECTORS)
            .map(|_| n)
    }
    fn store(&mut self, at: usize, bytes: &[u8]) {
        self.memory[at..at + bytes.len()].copy_from_slice(bytes);
        self.dirty.push((at, bytes.len()));
    }
    /// One chain, or None when nothing is pending; Err leaves it unconsumed.
    fn chain(&self, l: Layout) -> Result<Option<(u16, Vec<Segment>)>, ()> {
        let m = &self.memory;
        let avail_idx = rd16(m, l.avail + 2);
        let pending = avail_idx.wrapping_sub(self.last_avail);
        if pending > l.num {
            return Err(());
        }
        if pending == 0 {
            return Ok(None);
        }
        let head = rd16(m, l.avail + 4 + 2 * (self.last_avail % l.num) as usize);
        let mut index = head;
        let mut visited = vec![false; l.num as usize];
        let mut segments: Vec<Segment> = Vec::new();
        let mut total = 0u64;
        loop {
            if index >= l.num || visited[index as usize] {
                return Err(());
            }
            visited[index as usize] = true;
            let d = l.desc + 16 * index as usize;
            let (address, len, flags, next) =
                (rd64(m, d), rd32(m, d + 8), rd16(m, d + 12), rd16(m, d + 14));
            if flags & !3 != 0 {
                return Err(());
            }
            let writable = flags & 2 != 0;
            if !writable && segments.iter().any(|s| s.writable) {
                return Err(());
            }
            total += len as u64;
            if total > 4 << 20 || address.saturating_add(len as u64) > RAM as u64 {
                return Err(());
            }
            segments.push(Segment {
                at: address as usize,
                len: len as usize,
                writable,
            });
            if flags & 1 == 0 {
                return Ok(Some((head, segments)));
            }
            index = next;
        }
    }
    fn notify(&mut self) -> Result<(), ()> {
        let Some(l) = self.layout else {
            return Ok(());
        };
        while let Some((head, segments)) = self.chain(l)? {
            let readable: Vec<u8> = segments
                .iter()
                .filter(|s| !s.writable)
                .flat_map(|s| self.memory[s.at..s.at + s.len].to_vec())
                .collect();
            // Writable byte positions in chain order; the last one is status.
            let places: Vec<usize> = segments
                .iter()
                .filter(|s| s.writable)
                .flat_map(|s| s.at..s.at + s.len)
                .collect();
            if readable.len() < 16 || places.is_empty() {
                return Err(());
            }
            let kind = u32::from_le_bytes(readable[..4].try_into().unwrap());
            let sector = u64::from_le_bytes(readable[8..16].try_into().unwrap());
            let room = places.len() - 1;
            let (status, data) = match kind {
                T_IN => match Self::range(sector, room) {
                    Some(n) => (0, (0..n).flat_map(|i| self.sector(sector + i)).collect()),
                    None => (1, Vec::new()),
                },
                T_OUT => {
                    let input = &readable[16..];
                    match Self::range(sector, input.len()) {
                        Some(_) => {
                            for (i, chunk) in input.chunks(SECTOR).enumerate() {
                                self.volatile.insert(sector + i as u64, chunk.to_vec());
                            }
                            (0, Vec::new())
                        }
                        None => (1, Vec::new()),
                    }
                }
                T_FLUSH => {
                    let moved = std::mem::take(&mut self.volatile);
                    self.stable.extend(moved);
                    (0, Vec::new())
                }
                T_GET_ID => (0, ID[..room.min(ID.len())].to_vec()),
                _ => (2, Vec::new()),
            };
            // Payload: one journal entry per writable descriptor that receives bytes.
            let mut done = 0;
            for s in segments.iter().filter(|s| s.writable) {
                let n = s.len.min(data.len() - done);
                if n > 0 {
                    let bytes = data[done..done + n].to_vec();
                    let hits = |&(r, len): &(usize, usize)| s.at < r + len && r < s.at + n;
                    self.ring_payload += usize::from(l.rings().iter().any(hits));
                    self.store(s.at, &bytes);
                    done += n;
                }
            }
            self.store(places[room], &[status]);
            let entry = l.used + 4 + 8 * (self.used_idx % l.num) as usize;
            let mut element = (head as u32).to_le_bytes().to_vec();
            element.extend_from_slice(&(done as u32 + 1).to_le_bytes());
            self.memory[entry..entry + 8].copy_from_slice(&element);
            self.used_idx = self.used_idx.wrapping_add(1);
            let idx = self.used_idx.to_le_bytes();
            self.memory[l.used + 2..l.used + 4].copy_from_slice(&idx);
            self.dirty.push((entry, 8));
            self.dirty.push((l.used + 2, 2));
            self.last_avail = self.last_avail.wrapping_add(1);
            self.isr |= 1;
        }
        Ok(())
    }
}

// ------------------------------------------------------------ guest driver

/// The generating driver: owns only its own bookkeeping, never reads RAM.
#[derive(Clone)]
struct Driver {
    layout: Layout,
    next_desc: u16,
    avail: u16,
    data: usize,
}

struct Request {
    kind: u32,
    readable: Vec<u8>,
    writable: usize,
}

impl Driver {
    fn new(layout: Layout) -> Self {
        Self {
            layout,
            next_desc: 0,
            avail: 0,
            data: DATA.0,
        }
    }
    fn alloc(&mut self, len: usize) -> usize {
        if self.data + len > DATA.1 {
            self.data = DATA.0; // Wrap: new buffers alias old payloads.
        }
        let at = self.data;
        self.data += len.max(1);
        at
    }
    fn request(rng: &mut Rng) -> Request {
        let pick = rng.below(100);
        let kind = match pick {
            0..=39 => T_IN,
            40..=69 => T_OUT,
            70..=79 => T_FLUSH,
            80..=89 => T_GET_ID,
            _ => 3 + rng.below(3) as u32 * 2, // Unsupported: 3, 5, 7.
        };
        let sector = if rng.chance(10) {
            SECTORS - rng.below(2)
        } else {
            rng.below(SECTORS)
        };
        let mut sectors = rng.below(3) as usize * SECTOR;
        if rng.chance(5) {
            sectors += 100; // Invalid length: IOERR.
        }
        let mut readable = vec![0; 16];
        readable[..4].copy_from_slice(&kind.to_le_bytes());
        readable[4..8].copy_from_slice(&(rng.next() as u32).to_le_bytes());
        readable[8..16].copy_from_slice(&sector.to_le_bytes());
        let mut writable = 1;
        match kind {
            T_OUT => readable.extend((0..sectors).map(|_| rng.next() as u8)),
            T_IN => writable += sectors,
            T_GET_ID => writable += rng.below(40) as usize,
            T_FLUSH if rng.chance(30) => readable.extend([7; 16]),
            _ => {}
        }
        Request {
            kind,
            readable,
            writable,
        }
    }
    fn cuts(rng: &mut Rng, len: usize, extra: u64) -> Vec<usize> {
        let mut cuts = vec![0, len];
        for _ in 0..rng.below(extra + 1) {
            cuts.push(rng.below(len as u64 + 1) as usize);
        }
        cuts.sort();
        cuts
    }
    /// Post one request using at most `budget` (>= 2) descriptors; returns
    /// the head slot and the number of descriptors used.
    fn post(&mut self, rng: &mut Rng, ops: &mut Vec<Op>, alias: bool, budget: u16) -> (u16, u16) {
        let request = Self::request(rng);
        let l = self.layout;
        let mut spans = Vec::new();
        let spare = u64::from(budget - 2);
        let extra_readable = rng.below(spare.min(2) + 1);
        let readable = Self::cuts(rng, request.readable.len(), extra_readable);
        for pair in readable.windows(2) {
            let bytes = request.readable[pair[0]..pair[1]].to_vec();
            let at = self.alloc(bytes.len());
            ops.push(Op::Write(at, bytes.clone()));
            spans.push((at, bytes.len(), 0u16));
        }
        let writable = Self::cuts(
            rng,
            request.writable,
            (spare - (readable.len() as u64 - 2)).min(2),
        );
        let count = readable.len() - 1 + writable.len() - 1;
        let head = self.next_desc;
        let future = (head + count as u16) % l.num;
        for pair in writable.windows(2) {
            let len = pair[1] - pair[0];
            let at = if alias && request.kind == T_IN && len > 0 && rng.chance(40) {
                // Land read payload on rings or on the next request's descriptors.
                let target = match rng.below(4) {
                    0 => l.desc + 16 * future as usize,
                    1 => l.avail + 4 + 2 * ((self.avail + 1) % l.num) as usize,
                    2 => l.used + 4 + 8 * rng.below(l.num as u64) as usize,
                    _ => self.data,
                };
                target.min(RAM - len)
            } else {
                self.alloc(len)
            };
            spans.push((at, len, 2));
        }
        for (i, &(at, len, flags)) in spans.iter().enumerate() {
            let slot = (head + i as u16) % l.num;
            let last = i + 1 == spans.len();
            let next = (slot + 1) % l.num;
            let mut d = (at as u64).to_le_bytes().to_vec();
            d.extend_from_slice(&(len as u32).to_le_bytes());
            d.extend_from_slice(&(flags | u16::from(!last)).to_le_bytes());
            d.extend_from_slice(&next.to_le_bytes());
            ops.push(Op::Write(l.desc + 16 * slot as usize, d));
        }
        self.next_desc = (head + spans.len() as u16) % l.num;
        let entry = l.avail + 4 + 2 * (self.avail % l.num) as usize;
        ops.push(Op::Write(entry, head.to_le_bytes().to_vec()));
        self.avail = self.avail.wrapping_add(1);
        (head, spans.len() as u16)
    }
    fn publish(&self, ops: &mut Vec<Op>) {
        ops.push(Op::Write(
            self.layout.avail + 2,
            self.avail.to_le_bytes().to_vec(),
        ));
    }
    /// Corrupt the head descriptor of a posted chain; returns the repair store.
    fn corrupt(&self, rng: &mut Rng, ops: &mut Vec<Op>, head: u16, original: &[u8]) -> Op {
        let at = self.layout.desc + 16 * head as usize;
        let mut bad = original.to_vec();
        match rng.below(4) {
            0 => bad[12] |= 4, // INDIRECT: unsupported.
            1 => bad[..8].copy_from_slice(&0xffff_0000u64.to_le_bytes()), // Outside RAM.
            2 => {
                bad[12] |= 1; // NEXT to an index past the ring.
                bad[14..16].copy_from_slice(&(self.layout.num + 3).to_le_bytes());
            }
            _ => bad[8..12].copy_from_slice(&(RAM as u32).to_le_bytes()), // Span overrun.
        }
        ops.push(Op::Write(at, bad));
        Op::Write(at, original.to_vec())
    }
}

// ------------------------------------------------------------ the run

struct Run {
    policy: BlockPolicy,
    device: Device,
    oracle: Oracle,
    saved: Option<(DeviceCapture, Oracle)>,
    coverage: Coverage,
}

#[derive(Default, Debug)]
struct Coverage {
    notifications: usize,
    failed: usize,
    ring_payload: usize,
    resets: usize,
    restores: usize,
}

impl Run {
    fn new(policy: BlockPolicy) -> Self {
        Self {
            policy,
            device: Device::new(policy),
            oracle: Oracle::new(),
            saved: None,
            coverage: Coverage::default(),
        }
    }
    fn apply(&mut self, op: &Op) -> Result<(), String> {
        let (device, oracle) = (&mut self.device, &mut self.oracle);
        device.dirty.clear();
        oracle.dirty.clear();
        match op {
            Op::Write(at, bytes) => {
                device.memory[*at..at + bytes.len()].copy_from_slice(bytes);
                oracle.memory[*at..at + bytes.len()].copy_from_slice(bytes);
            }
            Op::Notify => {
                let got = device.mmio(0x50, 0).is_ok();
                let want = oracle.notify().is_ok();
                self.coverage.notifications += 1;
                self.coverage.failed += usize::from(!want);
                self.coverage.ring_payload += std::mem::take(&mut oracle.ring_payload);
                if got != want {
                    return Err(format!("notify ok={got}, oracle ok={want}"));
                }
            }
            Op::Ack => {
                device.mmio(0x64, 1).unwrap();
                oracle.isr &= !1;
            }
            Op::Reset(layout) => {
                self.coverage.resets += 1;
                device.negotiate(LAYOUTS[*layout]);
                oracle.layout = Some(LAYOUTS[*layout]);
                oracle.last_avail = 0;
                oracle.used_idx = 0;
                oracle.isr = 0;
            }
            Op::Capture => self.saved = Some((device.capture(), oracle.clone())),
            Op::Restore => {
                self.coverage.restores += 1;
                let (capture, saved) = self.saved.clone().expect("captured");
                device.restore(&capture, self.policy);
                *oracle = saved;
            }
        }
        self.compare()
    }
    fn compare(&self) -> Result<(), String> {
        let (d, o) = (&self.device, &self.oracle);
        if let Some(at) = (0..RAM).find(|&i| d.memory[i] != o.memory[i]) {
            return Err(format!(
                "RAM differs at {at:#x}: {:#x} != oracle {:#x}",
                d.memory[at], o.memory[at]
            ));
        }
        if d.dirty != o.dirty {
            return Err(format!("journal {:?} != oracle {:?}", d.dirty, o.dirty));
        }
        let q = &d.transport.queues[0];
        if (q.last_avail, q.used_idx, d.transport.isr) != (o.last_avail, o.used_idx, o.isr) {
            return Err(format!(
                "cursors/isr ({}, {}, {}) != oracle ({}, {}, {})",
                q.last_avail, q.used_idx, d.transport.isr, o.last_avail, o.used_idx, o.isr
            ));
        }
        let disk = d.block.disk();
        if (disk.stable_sectors(), disk.volatile_sectors())
            != (o.stable.len() as u64, o.volatile.len() as u64)
        {
            return Err("disk layer sizes differ".into());
        }
        for s in 0..SECTORS {
            let mut bytes = vec![0; SECTOR];
            disk.read(s, &mut bytes).unwrap();
            if bytes != o.sector(s) {
                return Err(format!("sector {s} differs"));
            }
        }
        // A crash keeps exactly the stable layer under the default policy.
        let (crashed, lost) = disk.crashed();
        if lost != o.volatile.len() as u64 {
            return Err("crash loses a different volatile set".into());
        }
        for s in 0..SECTORS {
            let mut bytes = vec![0; SECTOR];
            crashed.read(s, &mut bytes).unwrap();
            let want = o.stable.get(&s).cloned().unwrap_or_else(|| vec![0; SECTOR]);
            if bytes != want {
                return Err(format!("crashed sector {s} differs"));
            }
        }
        Ok(())
    }
}

/// One generated sequence of about 60 operations.
type Outcome = (Vec<Op>, Result<(), (usize, String)>, Coverage);
fn sequence(seed: u64, policy: BlockPolicy) -> Outcome {
    let mut rng = Rng(seed);
    let mut run = Run::new(policy);
    let mut ops = Vec::new();
    let mut driver = Driver::new(LAYOUTS[0]);
    let mut saved_driver = None;
    let mut next = vec![Op::Reset(0)];
    let mut applied = 0;
    let step = |run: &mut Run, ops: &mut Vec<Op>, batch: &mut Vec<Op>| {
        for op in batch.drain(..) {
            ops.push(op.clone());
            if let Err(error) = run.apply(&op) {
                return Err((ops.len() - 1, error));
            }
        }
        Ok(())
    };
    while applied < 60 {
        if let Err(failure) = step(&mut run, &mut ops, &mut next) {
            return (ops, Err(failure), run.coverage);
        }
        applied += 1;
        match rng.below(100) {
            0..=59 => {
                // A batch of requests, then one notification.
                let alias = rng.chance(50);
                let mut repair = None;
                // Requests in one batch never share descriptor slots.
                let mut budget = driver.layout.num;
                for _ in 0..1 + rng.below(3) {
                    if budget < 2 {
                        break;
                    }
                    let mut stores = Vec::new();
                    let (head, used) = driver.post(&mut rng, &mut stores, alias, budget);
                    budget -= used;
                    let at = driver.layout.desc + 16 * head as usize;
                    let original = stores
                        .iter()
                        .rev()
                        .find_map(|op| match op {
                            Op::Write(a, bytes) if *a == at => Some(bytes.clone()),
                            _ => None,
                        })
                        .unwrap();
                    next.extend(stores);
                    if repair.is_none() && rng.chance(10) {
                        repair = Some(driver.corrupt(&mut rng, &mut next, head, &original));
                    }
                }
                driver.publish(&mut next);
                next.push(Op::Notify);
                if let Some(repair) = repair {
                    // The failed head stays pending until the driver repairs it.
                    next.push(repair);
                    next.push(Op::Notify);
                }
            }
            60..=69 => next.push(Op::Notify), // Spurious or retried notification.
            70..=79 => next.push(Op::Ack),
            80..=86 => {
                let layout = rng.below(2) as usize;
                for (at, len) in LAYOUTS[layout].rings() {
                    next.push(Op::Write(at, vec![0; len]));
                }
                next.push(Op::Reset(layout));
                driver = Driver::new(LAYOUTS[layout]);
            }
            87..=93 => {
                next.push(Op::Capture);
                saved_driver = Some(driver.clone());
            }
            _ => {
                if let Some(saved) = &saved_driver {
                    next.push(Op::Restore);
                    driver = saved.clone();
                }
            }
        }
    }
    let result = step(&mut run, &mut ops, &mut next);
    (ops, result, run.coverage)
}

#[test]
fn generated_aliasing_sequences_match_the_sequential_oracle() {
    let mut total = Coverage::default();
    for policy in [BlockPolicy::Application, BlockPolicy::Block] {
        for seed in 0..250u64 {
            let (ops, result, coverage) =
                sequence(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1, policy);
            if let Err((at, error)) = result {
                let prefix: Vec<String> = ops[..=at].iter().map(|op| format!("{op:?}")).collect();
                panic!(
                    "seed {seed} step {at}: {error}\nfailing prefix:\n{}",
                    prefix.join("\n")
                );
            }
            total.notifications += coverage.notifications;
            total.failed += coverage.failed;
            total.ring_payload += coverage.ring_payload;
            total.resets += coverage.resets;
            total.restores += coverage.restores;
        }
    }
    // The generator must reach failed heads, ring aliases, resets and restores.
    eprintln!("coverage: {total:?}");
    assert!(total.notifications > 10_000, "{total:?}");
    assert!(total.failed > 300, "{total:?}");
    assert!(total.ring_payload > 300, "{total:?}");
    assert!(total.resets > 1_000, "{total:?}");
    assert!(total.restores > 100, "{total:?}");
}

// ------------------------------------------------------------ split exhaustion

/// Post one chain of `(at, len, writable)` spans at the start of a fresh ring.
fn single(
    policy: BlockPolicy,
    disk: &Disk,
    stores: &[(usize, Vec<u8>)],
    spans: &[(usize, usize, bool)],
) -> Device {
    let mut device = Device::new(policy);
    device.block = Block::new(disk.clone(), policy);
    let l = LAYOUTS[0];
    device.negotiate(l);
    for (at, bytes) in stores {
        device.memory[*at..at + bytes.len()].copy_from_slice(bytes);
    }
    for (i, &(at, len, writable)) in spans.iter().enumerate() {
        let d = l.desc + 16 * i;
        let m = &mut device.memory;
        m[d..d + 8].copy_from_slice(&(at as u64).to_le_bytes());
        m[d + 8..d + 12].copy_from_slice(&(len as u32).to_le_bytes());
        let flags = (if writable { 2u16 } else { 0 }) | u16::from(i + 1 < spans.len());
        m[d + 12..d + 14].copy_from_slice(&flags.to_le_bytes());
        m[d + 14..d + 16].copy_from_slice(&(i as u16 + 1).to_le_bytes());
    }
    device.memory[l.avail + 2..l.avail + 4].copy_from_slice(&1u16.to_le_bytes());
    device.mmio(0x50, 0).unwrap();
    device
}

/// Splits of `len` bytes at up to two cut points (equal cuts leave empty descriptors).
fn splits(len: usize, candidates: &[usize]) -> Vec<Vec<usize>> {
    let c: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&c| c > 0 && c < len)
        .collect();
    let mut out = vec![vec![0, len]];
    for (i, &a) in c.iter().enumerate() {
        out.push(vec![0, a, len]);
        for &b in &c[i..] {
            out.push(vec![0, a, b, len]);
        }
    }
    out
}

/// Lay out pieces from just before a page boundary, contiguously or with gaps.
fn place(pieces: &[usize], shift: usize, gap: usize) -> Vec<usize> {
    let mut at = 0x1000 - shift;
    pieces
        .iter()
        .map(|len| {
            let here = at;
            at += len + gap;
            here
        })
        .collect()
}

#[test]
fn every_header_and_payload_split_matches_the_contiguous_request() {
    let header_cuts = [1, 4, 8, 15, 16, 17, 16 + 511, 16 + 512, 16 + 513];
    let data_cuts = [1, 511, 512, 513, 1023, 1024];
    let shifts = [0, 1, 16, 511, 512, 513];
    let payload: Vec<u8> = (0..1024u32).map(|i| (i * 7 + i / 512) as u8).collect();
    let mut checked = 0;
    for policy in [BlockPolicy::Application, BlockPolicy::Block] {
        for kind in [T_OUT, T_IN] {
            let mut disk = fresh_disk();
            disk.write(3, &payload.iter().map(|b| !b).collect::<Vec<_>>())
                .unwrap();
            let mut header = vec![0; 16];
            header[..4].copy_from_slice(&kind.to_le_bytes());
            header[8..].copy_from_slice(&3u64.to_le_bytes());
            let readable: Vec<u8> = if kind == T_OUT {
                [header.clone(), payload.clone()].concat()
            } else {
                header.clone()
            };
            let writable = if kind == T_IN { 1025 } else { 1 };
            // Reference: one readable and one writable descriptor.
            let reference = single(
                policy,
                &disk,
                &[(0x400, readable.clone()), (0xa00, vec![0xee; writable])],
                &[(0x400, readable.len(), false), (0xa00, writable, true)],
            );
            let want_data = reference.memory[0xa00..0xa00 + writable].to_vec();
            let want_used = reference.memory[LAYOUTS[0].used + 4..LAYOUTS[0].used + 12].to_vec();
            let want_disk = reference.block.disk().digest();
            assert_eq!(want_data[writable - 1], 0, "reference status");
            for r in splits(readable.len(), &header_cuts) {
                for w in splits(writable, &data_cuts) {
                    for &shift in &shifts {
                        for gap in [0, 40] {
                            let mut pieces: Vec<usize> =
                                r.windows(2).map(|p| p[1] - p[0]).collect();
                            let readable_count = pieces.len();
                            pieces.extend(w.windows(2).map(|p| p[1] - p[0]));
                            let at = place(&pieces, shift, gap);
                            let mut stores = Vec::new();
                            let mut spans = Vec::new();
                            for (i, (&start, len)) in at.iter().zip(&pieces).enumerate() {
                                let writable = i >= readable_count;
                                let bytes = if writable {
                                    vec![0xee; *len]
                                } else {
                                    readable[r[i]..r[i + 1]].to_vec()
                                };
                                stores.push((start, bytes));
                                spans.push((start, *len, writable));
                            }
                            let device = single(policy, &disk, &stores, &spans);
                            let got: Vec<u8> = spans
                                .iter()
                                .filter(|s| s.2)
                                .flat_map(|&(a, n, _)| device.memory[a..a + n].to_vec())
                                .collect();
                            let context = format!(
                                "kind {kind} readable {r:?} writable {w:?} shift {shift} gap {gap}"
                            );
                            assert_eq!(got, want_data, "{context}");
                            assert_eq!(
                                &device.memory[LAYOUTS[0].used + 4..LAYOUTS[0].used + 12],
                                &want_used[..],
                                "{context}"
                            );
                            assert_eq!(device.block.disk().digest(), want_disk, "{context}");
                            assert_eq!(device.transport.isr, 1, "{context}");
                            // Journal: payload pieces that receive bytes, status, used entry, index.
                            let mut journal = Vec::new();
                            let mut left = if kind == T_IN { 1024 } else { 0 };
                            for &(a, n, writable) in &spans {
                                if writable && left > 0 && n > 0 {
                                    journal.push((a, n.min(left)));
                                    left -= n.min(left);
                                }
                            }
                            let status = spans
                                .iter()
                                .rfind(|s| s.2 && s.1 > 0)
                                .map(|&(a, n, _)| a + n - 1)
                                .unwrap();
                            journal.push((status, 1));
                            journal.push((LAYOUTS[0].used + 4, 8));
                            journal.push((LAYOUTS[0].used + 2, 2));
                            assert_eq!(device.dirty, journal, "{context}");
                            checked += 1;
                        }
                    }
                }
            }
        }
    }
    assert!(checked > 3000, "{checked} splits");
}
