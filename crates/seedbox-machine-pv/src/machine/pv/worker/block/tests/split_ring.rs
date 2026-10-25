//! Block requests over the shared split ring, checked against an independent
//! sequential oracle.
//!
//! The oracle re-reads the documented rules (`virtio.rs`, `block.rs` and the
//! store's `request.rs`) and shares no code with them: each chain is
//! validated from current RAM before any effect, a request reads its
//! readable bytes before writing, writes its data in chain order and then
//! its status into the last writable byte, then the used element at the used
//! index it reads from RAM, then that index; a later malformed chain leaves
//! the completed prefix completed. Every guest RAM byte, the queue cursor,
//! the interrupt status and both disk layers must match after every step.
//! The device keeps no DMA journal, so write order is observed only through
//! aliasing: generated payloads land on later requests' descriptors,
//! available entries and the used ring.
//!
//! Generated sequences run in 8 KiB of RAM: heads are corrupted and
//! repaired, the driver resets onto fresh rings, and a captured device is
//! restored through its serialized transport and encoded store overlays. A
//! mismatch reports the seed and the failing operation prefix.
use super::*;
use std::collections::BTreeMap;

const RAM: usize = 0x2000;
const SECTORS: u64 = 16;
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

/// Ring placement as RAM offsets.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Layout {
    size: u16,
    descriptors: usize,
    available: usize,
    used: usize,
}
const LAYOUTS: [Layout; 2] = [
    Layout {
        size: 8,
        descriptors: 0x0,
        available: 0x80,
        used: 0xc0,
    },
    Layout {
        size: 4,
        descriptors: 0x1e00,
        available: 0x1e40,
        used: 0x1e80,
    },
];
impl Layout {
    fn rings(&self) -> [(usize, usize); 3] {
        let n = self.size as usize;
        [
            (self.descriptors, 16 * n),
            (self.available, 6 + 2 * n),
            (self.used, 6 + 8 * n),
        ]
    }
}

#[derive(Clone, Debug)]
enum Op {
    /// A guest store at a RAM offset.
    Write(usize, Vec<u8>),
    /// QUEUE_NOTIFY for the request queue.
    Notify,
    /// Acknowledge the interrupt through INTERRUPT_ACK.
    Ack,
    /// Reset through STATUS and negotiate fresh rings at this layout.
    Reset(usize),
    Capture,
    Restore,
}

// ------------------------------------------------------------ device side

struct Device {
    block: Block,
    ram: Vec<u8>,
}

/// A device as a machine snapshot holds it: the transport as serialized in
/// the capture header, the store as its encoded overlays, and RAM.
#[derive(Clone)]
struct DeviceCapture {
    transport: Vec<u8>,
    overlays: Vec<u8>,
    ram: Vec<u8>,
}

fn fresh_store() -> Store {
    Store::zeroed(SECTORS).unwrap()
}

impl Device {
    fn new() -> Self {
        Self {
            block: Block::new(fresh_store()),
            ram: vec![0; RAM],
        }
    }
    fn mmio(&mut self, offset: u64, value: u32) -> Result<()> {
        self.block.write(&mut self.ram, offset, value)
    }
    fn negotiate(&mut self, l: Layout) {
        let mut w = |offset, value| self.mmio(offset, value).unwrap();
        w(STATUS, 0);
        w(STATUS, ACKNOWLEDGE_DRIVER);
        w(DRIVER_FEATURES_SEL, 0);
        w(DRIVER_FEATURES, FEATURES);
        w(DRIVER_FEATURES_SEL, 1);
        w(DRIVER_FEATURES, FEATURE_VERSION_1_HIGH);
        w(STATUS, ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK);
        w(QUEUE_SEL, 0);
        w(QUEUE_NUM, l.size as u32);
        for (low, high, at) in [
            (QUEUE_DESC_LOW, QUEUE_DESC_HIGH, l.descriptors),
            (QUEUE_AVAIL_LOW, QUEUE_AVAIL_HIGH, l.available),
            (QUEUE_USED_LOW, QUEUE_USED_HIGH, l.used),
        ] {
            let address = BASE + at as u64;
            w(low, address as u32);
            w(high, (address >> 32) as u32);
        }
        w(QUEUE_READY, 1);
        w(
            STATUS,
            ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }
    fn capture(&self) -> DeviceCapture {
        DeviceCapture {
            transport: serde_json::to_vec(self.block.transport()).unwrap(),
            overlays: self.block.store().encode_overlays(),
            ram: self.ram.clone(),
        }
    }
    fn restore(&mut self, saved: &DeviceCapture) {
        let mut store = fresh_store();
        store.replace_overlays(&saved.overlays).unwrap();
        let transport = serde_json::from_slice(&saved.transport).unwrap();
        self.block = Block::restore(transport, store);
        self.ram = saved.ram.clone();
    }
}

// ------------------------------------------------------------ oracle side

/// A sequential reading of the split-ring and block rules, sharing no code.
#[derive(Clone)]
struct Oracle {
    ram: Vec<u8>,
    layout: Option<Layout>,
    cursor: u16,
    interrupt: u32,
    durable: BTreeMap<u64, Vec<u8>>,
    unflushed: BTreeMap<u64, Vec<u8>>,
    /// Payload stores into a ring or descriptor table during the last notify.
    ring_payload: usize,
}

struct Segment {
    at: usize,
    length: usize,
    writable: bool,
}

impl Oracle {
    fn new() -> Self {
        Self {
            ram: vec![0; RAM],
            layout: None,
            cursor: 0,
            interrupt: 0,
            durable: BTreeMap::new(),
            unflushed: BTreeMap::new(),
            ring_payload: 0,
        }
    }
    fn sector(&self, s: u64) -> Vec<u8> {
        self.unflushed
            .get(&s)
            .or_else(|| self.durable.get(&s))
            .cloned()
            .unwrap_or_else(|| vec![0; SECTOR])
    }
    /// Sectors covered by `length` bytes from `sector`, if all fit.
    fn range(sector: u64, length: usize) -> Option<u64> {
        if !length.is_multiple_of(SECTOR) {
            return None;
        }
        let n = (length / SECTOR) as u64;
        sector
            .checked_add(n)
            .filter(|end| *end <= SECTORS)
            .map(|_| n)
    }
    /// The next chain, or None when nothing is pending; Err leaves it unconsumed.
    fn chain(&self, l: Layout) -> std::result::Result<Option<(u16, Vec<Segment>)>, ()> {
        let m = &self.ram;
        let pending = rd16(m, l.available + 2).wrapping_sub(self.cursor);
        if pending > l.size {
            return Err(());
        }
        if pending == 0 {
            return Ok(None);
        }
        let head = rd16(m, l.available + 4 + 2 * (self.cursor % l.size) as usize);
        let mut index = head;
        let mut visited = vec![false; l.size as usize];
        let mut segments: Vec<Segment> = Vec::new();
        loop {
            if index >= l.size || visited[index as usize] {
                return Err(());
            }
            visited[index as usize] = true;
            let d = l.descriptors + 16 * index as usize;
            let (address, length, flags, next) =
                (rd64(m, d), rd32(m, d + 8), rd16(m, d + 12), rd16(m, d + 14));
            if flags & !3 != 0 {
                return Err(());
            }
            let writable = flags & 2 != 0;
            if !writable && segments.iter().any(|s| s.writable) {
                return Err(());
            }
            let at = address.checked_sub(BASE).ok_or(())?;
            if at.saturating_add(length as u64) > RAM as u64 {
                return Err(());
            }
            segments.push(Segment {
                at: at as usize,
                length: length as usize,
                writable,
            });
            if flags & 1 == 0 {
                return Ok(Some((head, segments)));
            }
            index = next;
        }
    }
    fn notify(&mut self) -> std::result::Result<(), ()> {
        let Some(l) = self.layout else {
            return Ok(());
        };
        while let Some((head, segments)) = self.chain(l)? {
            let readable: Vec<u8> = segments
                .iter()
                .filter(|s| !s.writable)
                .flat_map(|s| self.ram[s.at..s.at + s.length].to_vec())
                .collect();
            // Writable byte positions in chain order; the last one is status.
            let places: Vec<usize> = segments
                .iter()
                .filter(|s| s.writable)
                .flat_map(|s| s.at..s.at + s.length)
                .collect();
            if readable.len() < 16 || places.is_empty() {
                return Err(());
            }
            let kind = u32::from_le_bytes(readable[..4].try_into().unwrap());
            let sector = u64::from_le_bytes(readable[8..16].try_into().unwrap());
            let (input, room) = (&readable[16..], places.len() - 1);
            let (status, data): (u8, Vec<u8>) = match kind {
                T_IN if input.is_empty() => match Self::range(sector, room) {
                    Some(n) => (0, (0..n).flat_map(|i| self.sector(sector + i)).collect()),
                    None => (1, Vec::new()),
                },
                T_OUT if room == 0 => match Self::range(sector, input.len()) {
                    Some(_) => {
                        for (i, chunk) in input.chunks(SECTOR).enumerate() {
                            self.unflushed.insert(sector + i as u64, chunk.to_vec());
                        }
                        (0, Vec::new())
                    }
                    None => (1, Vec::new()),
                },
                T_FLUSH if input.is_empty() && room == 0 => {
                    let moved = std::mem::take(&mut self.unflushed);
                    self.durable.extend(moved);
                    (0, Vec::new())
                }
                T_GET_ID if input.is_empty() => (0, ID[..room.min(ID.len())].to_vec()),
                T_IN | T_OUT | T_FLUSH | T_GET_ID => (1, Vec::new()),
                _ => (2, Vec::new()),
            };
            let rings = l.rings();
            for (&at, &byte) in places.iter().zip(&data) {
                let hits =
                    |&(start, length): &(usize, usize)| (start..start + length).contains(&at);
                self.ring_payload += usize::from(rings.iter().any(hits));
                self.ram[at] = byte;
            }
            self.ram[places[room]] = status;
            // The used index is read back from RAM, where a payload may have landed.
            let index = rd16(&self.ram, l.used + 2);
            let entry = l.used + 4 + 8 * (index % l.size) as usize;
            self.ram[entry..entry + 4].copy_from_slice(&(head as u32).to_le_bytes());
            self.ram[entry + 4..entry + 8].copy_from_slice(&(data.len() as u32 + 1).to_le_bytes());
            self.ram[l.used + 2..l.used + 4].copy_from_slice(&index.wrapping_add(1).to_le_bytes());
            self.cursor = self.cursor.wrapping_add(1);
            if rd16(&self.ram, l.available) & 1 == 0 {
                self.interrupt |= 1;
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ guest driver

/// The generating driver: owns only its own bookkeeping, never reads RAM.
#[derive(Clone)]
struct Driver {
    layout: Layout,
    next_descriptor: u16,
    available: u16,
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
            next_descriptor: 0,
            available: 0,
            data: DATA.0,
        }
    }
    fn alloc(&mut self, length: usize) -> usize {
        if self.data + length > DATA.1 {
            self.data = DATA.0; // Wrap: new buffers alias old payloads.
        }
        let at = self.data;
        self.data += length.max(1);
        at
    }
    fn request(rng: &mut Rng) -> Request {
        let kind = match rng.below(100) {
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
            sectors += 100; // Partial data: IOERR.
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
            // Misplaced data on a FLUSH: IOERR.
            T_FLUSH if rng.chance(30) => readable.extend([7; 16]),
            _ => {}
        }
        Request {
            kind,
            readable,
            writable,
        }
    }
    fn cuts(rng: &mut Rng, length: usize, extra: u64) -> Vec<usize> {
        let mut cuts = vec![0, length];
        for _ in 0..rng.below(extra + 1) {
            cuts.push(rng.below(length as u64 + 1) as usize);
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
        let head = self.next_descriptor;
        let future = (head + count as u16) % l.size;
        for pair in writable.windows(2) {
            let length = pair[1] - pair[0];
            let at = if alias && request.kind == T_IN && length > 0 && rng.chance(40) {
                // Land read payload on rings or on the next request's descriptors.
                let target = match rng.below(4) {
                    0 => l.descriptors + 16 * future as usize,
                    1 => l.available + 4 + 2 * ((self.available + 1) % l.size) as usize,
                    2 => l.used + 4 + 8 * rng.below(l.size as u64) as usize,
                    _ => self.data,
                };
                target.min(RAM - length)
            } else {
                self.alloc(length)
            };
            spans.push((at, length, DESCRIPTOR_WRITE));
        }
        for (i, &(at, length, flags)) in spans.iter().enumerate() {
            let slot = (head + i as u16) % l.size;
            let last = i + 1 == spans.len();
            let next = (slot + 1) % l.size;
            let mut d = (BASE + at as u64).to_le_bytes().to_vec();
            d.extend_from_slice(&(length as u32).to_le_bytes());
            d.extend_from_slice(&(flags | (u16::from(!last) * DESCRIPTOR_NEXT)).to_le_bytes());
            d.extend_from_slice(&next.to_le_bytes());
            ops.push(Op::Write(l.descriptors + 16 * slot as usize, d));
        }
        self.next_descriptor = (head + spans.len() as u16) % l.size;
        let entry = l.available + 4 + 2 * (self.available % l.size) as usize;
        ops.push(Op::Write(entry, head.to_le_bytes().to_vec()));
        self.available = self.available.wrapping_add(1);
        (head, spans.len() as u16)
    }
    fn publish(&self, ops: &mut Vec<Op>) {
        ops.push(Op::Write(
            self.layout.available + 2,
            self.available.to_le_bytes().to_vec(),
        ));
    }
    /// Corrupt the head descriptor of a posted chain; returns the repair store.
    fn corrupt(&self, rng: &mut Rng, ops: &mut Vec<Op>, head: u16, original: &[u8]) -> Op {
        let at = self.layout.descriptors + 16 * head as usize;
        let mut bad = original.to_vec();
        match rng.below(5) {
            0 => bad[12] |= 4, // INDIRECT: unsupported.
            // The header turns device-writable: readable data after it, or
            // no header at all.
            4 => bad[12] |= DESCRIPTOR_WRITE as u8,
            1 => bad[..8].copy_from_slice(&0xffff_0000u64.to_le_bytes()), // Outside RAM.
            2 => {
                bad[12] |= 1; // NEXT to an index past the ring.
                bad[14..16].copy_from_slice(&(self.layout.size + 3).to_le_bytes());
            }
            _ => bad[8..12].copy_from_slice(&(RAM as u32).to_le_bytes()), // Span overrun.
        }
        ops.push(Op::Write(at, bad));
        Op::Write(at, original.to_vec())
    }
}

// ------------------------------------------------------------ the run

struct Run {
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
    fn apply(&mut self, op: &Op) -> std::result::Result<(), String> {
        let (device, oracle) = (&mut self.device, &mut self.oracle);
        match op {
            Op::Write(at, bytes) => {
                // A guest store has the same effect on both sides; there is
                // nothing to compare until the device acts.
                device.ram[*at..at + bytes.len()].copy_from_slice(bytes);
                oracle.ram[*at..at + bytes.len()].copy_from_slice(bytes);
                return Ok(());
            }
            Op::Notify => {
                let got = device.mmio(QUEUE_NOTIFY, 0).is_ok();
                let want = oracle.notify().is_ok();
                self.coverage.notifications += 1;
                self.coverage.failed += usize::from(!want);
                self.coverage.ring_payload += std::mem::take(&mut oracle.ring_payload);
                if got != want {
                    return Err(format!("notify ok={got}, oracle ok={want}"));
                }
            }
            Op::Ack => {
                device.mmio(INTERRUPT_ACK, USED_RING_INTERRUPT).unwrap();
                oracle.interrupt &= !USED_RING_INTERRUPT;
            }
            Op::Reset(layout) => {
                self.coverage.resets += 1;
                device.negotiate(LAYOUTS[*layout]);
                oracle.layout = Some(LAYOUTS[*layout]);
                oracle.cursor = 0;
                oracle.interrupt = 0;
            }
            Op::Capture => self.saved = Some((device.capture(), oracle.clone())),
            Op::Restore => {
                self.coverage.restores += 1;
                let (capture, saved) = self.saved.clone().expect("captured");
                device.restore(&capture);
                *oracle = saved;
            }
        }
        self.compare()
    }
    fn compare(&self) -> std::result::Result<(), String> {
        let (d, o) = (&self.device, &self.oracle);
        if let Some(at) = (d.ram != o.ram)
            .then(|| (0..RAM).find(|&i| d.ram[i] != o.ram[i]))
            .flatten()
        {
            return Err(format!(
                "RAM differs at {at:#x}: {:#x} != oracle {:#x}",
                d.ram[at], o.ram[at]
            ));
        }
        let cursor = d.block.transport().queue[0].cursor;
        let interrupt = d.block.interrupt_status();
        if (cursor, interrupt) != (o.cursor, o.interrupt) {
            return Err(format!(
                "cursor/interrupt ({cursor}, {interrupt}) != oracle ({}, {})",
                o.cursor, o.interrupt
            ));
        }
        let store = d.block.store();
        let mut bytes = vec![0; SECTOR];
        for s in 0..SECTORS {
            store.read(s, &mut bytes).unwrap();
            if bytes != o.sector(s) {
                return Err(format!("sector {s} differs"));
            }
        }
        // Losing every unflushed write keeps exactly the durable layer.
        let (lost, count) = store.crashed_keeping(|_, _| false);
        if count != o.unflushed.len() as u64 {
            return Err("a power loss loses a different unflushed set".into());
        }
        for s in 0..SECTORS {
            lost.read(s, &mut bytes).unwrap();
            let want = o
                .durable
                .get(&s)
                .cloned()
                .unwrap_or_else(|| vec![0; SECTOR]);
            if bytes != want {
                return Err(format!("durable sector {s} differs"));
            }
        }
        Ok(())
    }
}

/// One generated sequence of about 60 operations.
type Outcome = (Vec<Op>, std::result::Result<(), (usize, String)>, Coverage);
fn sequence(seed: u64) -> Outcome {
    let mut rng = Rng(seed);
    let mut run = Run {
        device: Device::new(),
        oracle: Oracle::new(),
        saved: None,
        coverage: Coverage::default(),
    };
    let mut ops = Vec::new();
    let mut driver = Driver::new(LAYOUTS[0]);
    let mut saved_driver = None;
    let mut next = vec![Op::Reset(0)];
    let step = |run: &mut Run, ops: &mut Vec<Op>, batch: &mut Vec<Op>| {
        for op in batch.drain(..) {
            ops.push(op.clone());
            if let Err(error) = run.apply(&op) {
                return Err((ops.len() - 1, error));
            }
        }
        Ok(())
    };
    for _ in 0..60 {
        if let Err(failure) = step(&mut run, &mut ops, &mut next) {
            return (ops, Err(failure), run.coverage);
        }
        match rng.below(100) {
            0..=59 => {
                // A batch of requests, then one notification.
                let alias = rng.chance(50);
                let mut repair = None;
                // Requests in one batch never share descriptor slots.
                let mut budget = driver.layout.size;
                for _ in 0..1 + rng.below(3) {
                    if budget < 2 {
                        break;
                    }
                    let mut stores = Vec::new();
                    let (head, used) = driver.post(&mut rng, &mut stores, alias, budget);
                    budget -= used;
                    let at = driver.layout.descriptors + 16 * head as usize;
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
                for (at, length) in LAYOUTS[layout].rings() {
                    next.push(Op::Write(at, vec![0; length]));
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
    for seed in 0..250u64 {
        let (ops, result, coverage) = sequence(seed.wrapping_mul(0x2545_f491_4f6c_dd1d) | 1);
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
    // The generator must reach failed heads, ring aliases, resets and restores.
    assert!(total.notifications > 5_000, "{total:?}");
    assert!(total.failed > 150, "{total:?}");
    assert!(total.ring_payload > 150, "{total:?}");
    assert!(total.resets > 500, "{total:?}");
    assert!(total.restores > 50, "{total:?}");
}

// ------------------------------------------------------------ split exhaustion

/// Post one chain of `(at, length, writable)` spans at the start of a fresh
/// ring and notify; returns the device and RAM just before the notify.
fn single(
    store: &Store,
    stores: &[(usize, Vec<u8>)],
    spans: &[(usize, usize, bool)],
) -> (Device, Vec<u8>) {
    let mut device = Device::new();
    device.block = Block::new(store.clone());
    let l = LAYOUTS[0];
    device.negotiate(l);
    for (at, bytes) in stores {
        device.ram[*at..at + bytes.len()].copy_from_slice(bytes);
    }
    for (i, &(at, length, writable)) in spans.iter().enumerate() {
        let d = l.descriptors + 16 * i;
        let m = &mut device.ram;
        m[d..d + 8].copy_from_slice(&(BASE + at as u64).to_le_bytes());
        m[d + 8..d + 12].copy_from_slice(&(length as u32).to_le_bytes());
        let flags = (writable as u16 * DESCRIPTOR_WRITE)
            | (u16::from(i + 1 < spans.len()) * DESCRIPTOR_NEXT);
        m[d + 12..d + 14].copy_from_slice(&flags.to_le_bytes());
        m[d + 14..d + 16].copy_from_slice(&(i as u16 + 1).to_le_bytes());
    }
    device.ram[l.available + 2..l.available + 4].copy_from_slice(&1u16.to_le_bytes());
    let before = device.ram.clone();
    device.mmio(QUEUE_NOTIFY, 0).unwrap();
    (device, before)
}

/// Splits of `length` bytes at up to two cut points (equal cuts leave empty
/// descriptors).
fn splits(length: usize, candidates: &[usize]) -> Vec<Vec<usize>> {
    let c: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&c| c > 0 && c < length)
        .collect();
    let mut out = vec![vec![0, length]];
    for (i, &a) in c.iter().enumerate() {
        out.push(vec![0, a, length]);
        for &b in &c[i..] {
            out.push(vec![0, a, b, length]);
        }
    }
    out
}

/// Lay out pieces from just before a page boundary, contiguously or with gaps.
fn place(pieces: &[usize], shift: usize, gap: usize) -> Vec<usize> {
    let mut at = 0x1000 - shift;
    pieces
        .iter()
        .map(|length| {
            let here = at;
            at += length + gap;
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
    let used = LAYOUTS[0].used;
    let mut checked = 0;
    for kind in [T_OUT, T_IN] {
        let mut store = fresh_store();
        store
            .write(3, &payload.iter().map(|b| !b).collect::<Vec<_>>())
            .unwrap();
        let readable: Vec<u8> = if kind == T_OUT {
            [header(kind, 3), payload.clone()].concat()
        } else {
            header(kind, 3)
        };
        let writable = if kind == T_IN { 1025 } else { 1 };
        // Reference: one readable and one writable descriptor.
        let (reference, _) = single(
            &store,
            &[(0x400, readable.clone()), (0xa00, vec![0xee; writable])],
            &[(0x400, readable.len(), false), (0xa00, writable, true)],
        );
        let want_data = reference.ram[0xa00..0xa00 + writable].to_vec();
        let want_used = reference.ram[used + 4..used + 12].to_vec();
        let want_store = reference.block.store().state_digest(false).unwrap();
        assert_eq!(want_data[writable - 1], STATUS_OK, "reference status");
        for r in splits(readable.len(), &header_cuts) {
            for w in splits(writable, &data_cuts) {
                for &shift in &shifts {
                    for gap in [0, 40] {
                        let mut pieces: Vec<usize> = r.windows(2).map(|p| p[1] - p[0]).collect();
                        let readable_count = pieces.len();
                        pieces.extend(w.windows(2).map(|p| p[1] - p[0]));
                        let at = place(&pieces, shift, gap);
                        let mut stores = Vec::new();
                        let mut spans = Vec::new();
                        for (i, (&start, &length)) in at.iter().zip(&pieces).enumerate() {
                            let writable = i >= readable_count;
                            let bytes = if writable {
                                vec![0xee; length]
                            } else {
                                readable[r[i]..r[i + 1]].to_vec()
                            };
                            stores.push((start, bytes));
                            spans.push((start, length, writable));
                        }
                        let (device, before) = single(&store, &stores, &spans);
                        let got: Vec<u8> = spans
                            .iter()
                            .filter(|s| s.2)
                            .flat_map(|&(a, n, _)| device.ram[a..a + n].to_vec())
                            .collect();
                        let context = format!(
                            "kind {kind} readable {r:?} writable {w:?} shift {shift} gap {gap}"
                        );
                        assert_eq!(got, want_data, "{context}");
                        assert_eq!(
                            &device.ram[used + 4..used + 12],
                            &want_used[..],
                            "{context}"
                        );
                        assert_eq!(
                            device.block.store().state_digest(false).unwrap(),
                            want_store,
                            "{context}"
                        );
                        assert!(device.block.irq(), "{context}");
                        // Only received payload bytes, the status byte and
                        // the used element and index may change.
                        let places: Vec<usize> = spans
                            .iter()
                            .filter(|s| s.2)
                            .flat_map(|&(a, n, _)| a..a + n)
                            .collect();
                        let mut allowed = vec![false; RAM];
                        let data = if kind == T_IN { 1024 } else { 0 };
                        for &at in places[..data].iter().chain(places.last()) {
                            allowed[at] = true;
                        }
                        allowed[used + 2..used + 12].fill(true);
                        for at in (0..RAM).filter(|&i| device.ram[i] != before[i]) {
                            assert!(allowed[at], "{context}: byte {at:#x} changed");
                        }
                        checked += 1;
                    }
                }
            }
        }
    }
    assert!(checked > 1500, "{checked} splits");
}
