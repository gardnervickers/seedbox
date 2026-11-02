use super::super::boot::RAM_BASE as BASE;
use super::*;
use seedbox_machine::machine::disk::{
    request::{STATUS_IOERR, STATUS_OK, STATUS_UNSUPP},
    SECTOR,
};

// Guest physical layout of the synthetic ring, relative to RAM.
const QUEUE: u16 = 16;
const DESCRIPTORS: u64 = 0;
const AVAILABLE: u64 = 0x1000;
const USED: u64 = 0x2000;
const BUFFERS: u64 = 0x4000;
const ACKNOWLEDGE_DRIVER: u32 = 1 | 2;

enum Part {
    Read(Vec<u8>),
    Write(usize),
}

/// A minimal split-ring driver writing synthetic chains into guest RAM.
struct Guest {
    ram: Vec<u8>,
    size: u16,
    next_descriptor: u16,
    available: u16,
    next_buffer: u64,
    used_seen: u16,
}

impl Guest {
    fn new() -> Self {
        Self::with(256 << 10, QUEUE)
    }
    fn with(ram: usize, size: u16) -> Self {
        Self {
            ram: vec![0; ram],
            size,
            next_descriptor: 0,
            available: 0,
            next_buffer: BUFFERS,
            used_seen: 0,
        }
    }
    fn put(&mut self, at: u64, bytes: &[u8]) {
        self.ram[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    }
    fn get(&self, at: u64, length: usize) -> &[u8] {
        &self.ram[at as usize..at as usize + length]
    }
    fn descriptor(&mut self, index: u16, address: u64, length: usize, flags: u16, next: u16) {
        let at = DESCRIPTORS + index as u64 * 16;
        self.put(at, &(BASE + address).to_le_bytes());
        self.put(at + 8, &(length as u32).to_le_bytes());
        self.put(at + 12, &flags.to_le_bytes());
        self.put(at + 14, &next.to_le_bytes());
    }
    fn allocate(&mut self, length: usize) -> u64 {
        let at = self.next_buffer;
        self.next_buffer += (length as u64).div_ceil(64).max(1) * 64;
        at
    }
    /// Lay out a chain from explicit (address, length, writable) spans.
    fn spans(&mut self, spans: &[(u64, usize, bool)]) -> u16 {
        let head = self.next_descriptor;
        for (i, &(address, length, writable)) in spans.iter().enumerate() {
            let index = self.next_descriptor;
            self.next_descriptor = (self.next_descriptor + 1) % self.size;
            let more = i + 1 < spans.len();
            let flags = (more as u16 * DESCRIPTOR_NEXT) | (writable as u16 * DESCRIPTOR_WRITE);
            self.descriptor(index, address, length, flags, self.next_descriptor);
        }
        head
    }
    /// Lay out a chain and return its head and writable buffer addresses.
    fn chain(&mut self, parts: &[Part]) -> (u16, Vec<u64>) {
        let mut spans = Vec::new();
        let mut writable = Vec::new();
        for part in parts {
            match part {
                Part::Read(bytes) => {
                    let at = self.allocate(bytes.len());
                    self.put(at, bytes);
                    spans.push((at, bytes.len(), false));
                }
                Part::Write(length) => {
                    let at = self.allocate(*length);
                    self.put(at, &vec![0xcc; *length]);
                    spans.push((at, *length, true));
                    writable.push(at);
                }
            }
        }
        (self.spans(&spans), writable)
    }
    fn offer(&mut self, head: u16) {
        let slot = AVAILABLE + 4 + (self.available % self.size) as u64 * 2;
        self.put(slot, &head.to_le_bytes());
        self.available = self.available.wrapping_add(1);
        self.put(AVAILABLE + 2, &self.available.to_le_bytes());
    }
    fn suppress_interrupts(&mut self) {
        self.put(AVAILABLE, &1u16.to_le_bytes());
    }
    /// New used entries as (head, length), in ring order.
    fn used(&mut self) -> Vec<(u16, u32)> {
        let index = u16::from_le_bytes(self.get(USED + 2, 2).try_into().unwrap());
        let mut entries = Vec::new();
        while self.used_seen != index {
            let at = USED + 4 + (self.used_seen % self.size) as u64 * 8;
            let entry = self.get(at, 8);
            entries.push((
                u32::from_le_bytes(entry[..4].try_into().unwrap()) as u16,
                u32::from_le_bytes(entry[4..].try_into().unwrap()),
            ));
            self.used_seen = self.used_seen.wrapping_add(1);
        }
        entries
    }
}

fn header(kind: u32, sector: u64) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0; 4]);
    bytes.extend(sector.to_le_bytes());
    bytes
}

fn out(sector: u64, value: u8, sectors: usize) -> Vec<Part> {
    vec![
        Part::Read(header(1, sector)),
        Part::Read(vec![value; sectors * SECTOR]),
        Part::Write(1),
    ]
}

fn read(sector: u64, sectors: usize) -> Vec<Part> {
    vec![
        Part::Read(header(0, sector)),
        Part::Write(sectors * SECTOR),
        Part::Write(1),
    ]
}

fn flush() -> Vec<Part> {
    vec![Part::Read(header(4, 0)), Part::Write(1)]
}

/// Negotiate like Linux virtio-mmio and enable the queue.
fn setup(device: &mut Block, guest: &mut Guest) {
    let size = guest.size;
    let ram = &mut guest.ram;
    let mut write = |offset, value| device.write(ram, offset, value).unwrap();
    write(STATUS, ACKNOWLEDGE_DRIVER);
    write(DRIVER_FEATURES_SEL, 0);
    write(DRIVER_FEATURES, FEATURES);
    write(DRIVER_FEATURES_SEL, 1);
    write(DRIVER_FEATURES, FEATURE_VERSION_1_HIGH);
    write(STATUS, ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK);
    write(QUEUE_SEL, 0);
    write(QUEUE_NUM, size as u32);
    for (low, high, address) in [
        (QUEUE_DESC_LOW, QUEUE_DESC_HIGH, DESCRIPTORS),
        (QUEUE_AVAIL_LOW, QUEUE_AVAIL_HIGH, AVAILABLE),
        (QUEUE_USED_LOW, QUEUE_USED_HIGH, USED),
    ] {
        write(low, (BASE + address) as u32);
        write(high, ((BASE + address) >> 32) as u32);
    }
    write(QUEUE_READY, 1);
    write(
        STATUS,
        ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
}

fn ready(sectors: u64) -> (Block, Guest) {
    let mut device = Block::new(
        Store::from_image(vec![b'.'; sectors as usize * SECTOR]).unwrap(),
        Default::default(),
    );
    let mut guest = Guest::new();
    setup(&mut device, &mut guest);
    (device, guest)
}

fn notify(device: &mut Block, guest: &mut Guest) -> Result<()> {
    device.write(&mut guest.ram, QUEUE_NOTIFY, 0)
}

/// Each sector's first byte, now and after a power loss.
fn view(store: &Store) -> (Vec<u8>, Vec<u8>) {
    let first = |store: &Store| {
        (0..store.capacity())
            .map(|s| {
                let mut b = [0; SECTOR];
                store.read(s, &mut b).unwrap();
                b[0]
            })
            .collect()
    };
    let mut lost = store.clone();
    lost.discard_unflushed();
    (first(store), first(&lost))
}

#[test]
fn identifies_as_a_flushing_block_device_of_store_capacity() {
    let mut device = Block::new(Store::zeroed((5 << 32) + 7).unwrap(), Default::default());
    let expected = [
        (MAGIC, 0x74726976),
        (VERSION, 2),
        (DEVICE_ID, 2),
        (DEVICE_FEATURES, FEATURE_SEG_MAX | FEATURE_FLUSH),
        (QUEUE_NUM_MAX, 256),
        (CAPACITY_LOW, 7),
        (CAPACITY_HIGH, 5),
        (SEG_MAX, 254),
    ];
    for (offset, value) in expected {
        assert_eq!(device.read(offset).unwrap(), value, "{offset:#x}");
    }
    let mut ram = vec![0; 64];
    device.write(&mut ram, DEVICE_FEATURES_SEL, 1).unwrap();
    assert_eq!(
        device.read(DEVICE_FEATURES).unwrap(),
        FEATURE_VERSION_1_HIGH
    );
    device.write(&mut ram, QUEUE_SEL, 1).unwrap();
    assert_eq!(device.read(QUEUE_NUM_MAX).unwrap(), 0);
    assert!(device.write(&mut ram, QUEUE_NUM, 8).is_err());
    assert!(device.read(0x110).is_err(), "unadvertised config");
}

#[test]
fn rejects_unoffered_or_legacy_feature_sets() {
    for (low, high) in [(FEATURES | 1 << 28, 1), (FEATURE_FLUSH, 0), (0, 1 | 2)] {
        let mut device = Block::new(Store::zeroed(1).unwrap(), Default::default());
        let mut ram = vec![0; 64];
        device.write(&mut ram, DRIVER_FEATURES, low).unwrap();
        device.write(&mut ram, DRIVER_FEATURES_SEL, 1).unwrap();
        device.write(&mut ram, DRIVER_FEATURES, high).unwrap();
        assert!(device.write(&mut ram, STATUS, STATUS_FEATURES_OK).is_err());
    }
}

#[test]
fn one_notify_completes_requests_in_ring_order() {
    let (mut device, mut guest) = ready(4);
    let (write_a, _) = guest.chain(&out(0, b'A', 2));
    let (read_back, buffers) = guest.chain(&read(0, 3));
    let (flushed, _) = guest.chain(&flush());
    let (write_b, _) = guest.chain(&out(1, b'B', 1));
    let (read_again, again) = guest.chain(&read(1, 1));
    for head in [write_a, read_back, flushed, write_b, read_again] {
        guest.offer(head);
    }
    let mut completions = Vec::new();
    device
        .write_traced(
            &mut guest.ram,
            QUEUE_NOTIFY,
            0,
            0,
            &mut |kind, status, sector| completions.push((kind, status, sector)),
        )
        .unwrap();
    assert_eq!(
        completions,
        [
            (1, STATUS_OK, 0),
            (0, STATUS_OK, 0),
            (4, STATUS_OK, 0),
            (1, STATUS_OK, 1),
            (0, STATUS_OK, 1)
        ]
    );
    assert_eq!(
        guest.used(),
        [
            (write_a, 1),
            (read_back, 3 * SECTOR as u32 + 1),
            (flushed, 1),
            (write_b, 1),
            (read_again, SECTOR as u32 + 1)
        ]
    );
    // The read saw the earlier write in the same notify; so did the last one.
    let data = guest.get(buffers[0], 3 * SECTOR);
    assert!(data[..2 * SECTOR].iter().all(|&b| b == b'A'));
    assert!(data[2 * SECTOR..].iter().all(|&b| b == b'.'));
    assert_eq!(guest.get(buffers[1], 1), [STATUS_OK]);
    assert!(guest.get(again[0], SECTOR).iter().all(|&b| b == b'B'));
    // The flush covered only the writes before it.
    assert_eq!(view(device.store()), (b"AB..".to_vec(), b"AA..".to_vec()));
    assert!(device.irq());
    assert_eq!(device.read(INTERRUPT_STATUS).unwrap(), 1);
    device.write(&mut guest.ram, INTERRUPT_ACK, 1).unwrap();
    assert!(!device.irq());
    // A repeated notify finds nothing new.
    notify(&mut device, &mut guest).unwrap();
    assert!(guest.used().is_empty() && !device.irq());
}

#[test]
fn data_precedes_status_and_status_is_the_last_writable_byte() {
    let (mut device, mut guest) = ready(2);
    let h = guest.allocate(16);
    guest.put(h, &header(0, 0));
    // Data and status share one writable descriptor.
    let shared = guest.allocate(SECTOR + 1);
    let one = guest.spans(&[(h, 16, false), (shared, SECTOR + 1, true)]);
    // The status descriptor aliases the data's last byte: data is written
    // first, then status overwrites that byte.
    let aliased = guest.allocate(SECTOR);
    let two = guest.spans(&[
        (h, 16, false),
        (aliased, SECTOR, true),
        (aliased + SECTOR as u64 - 1, 1, true),
    ]);
    // A trailing zero-length writable descriptor does not move the status.
    let status = guest.allocate(1);
    guest.put(status, &[0xcc]);
    let three = guest.spans(&[(h, 16, false), (status, 1, true), (status + 1, 0, true)]);
    for head in [one, two, three] {
        guest.offer(head);
    }
    notify(&mut device, &mut guest).unwrap();
    assert!(guest.get(shared, SECTOR).iter().all(|&b| b == b'.'));
    assert_eq!(guest.get(shared + SECTOR as u64, 1), [STATUS_OK]);
    assert!(guest.get(aliased, SECTOR - 1).iter().all(|&b| b == b'.'));
    assert_eq!(guest.get(aliased + SECTOR as u64 - 1, 1), [STATUS_OK]);
    assert_eq!(guest.get(status, 1), [STATUS_OK]);
    let full = SECTOR as u32 + 1;
    assert_eq!(guest.used(), [(one, full), (two, full), (three, 1)]);
}

#[test]
fn guest_errors_complete_and_later_requests_continue() {
    let (mut device, mut guest) = ready(2);
    let (good, _) = guest.chain(&out(0, b'G', 1));
    guest.offer(good);
    notify(&mut device, &mut guest).unwrap();
    guest.used();
    let before = view(device.store());
    let mut heads = Vec::new();
    let mut statuses = Vec::new();
    for parts in [
        out(1, b'X', 2),                                  // past the end
        vec![Part::Read(header(0, 0)), Part::Write(101)], // partial read
        vec![Part::Read(header(7, 0)), Part::Write(1)],   // unsupported type
        vec![Part::Read(header(11, 0)), Part::Write(SECTOR + 1)],
    ] {
        let (head, writable) = guest.chain(&parts);
        guest.offer(head);
        heads.push(head);
        statuses.push(*writable.last().unwrap());
    }
    let (last, last_buffers) = guest.chain(&read(0, 1));
    guest.offer(last);
    notify(&mut device, &mut guest).unwrap();
    let used = guest.used();
    assert_eq!(used.len(), 5);
    assert!(used[..4].iter().zip(&heads).all(|(u, h)| *u == (*h, 1)));
    let codes: Vec<u8> = statuses.iter().map(|&at| guest.get(at, 1)[0]).collect();
    // The partial read's status is its single 101-byte buffer's last byte.
    assert_eq!(codes, [STATUS_IOERR, 0xcc, STATUS_UNSUPP, 0xcc]);
    let partial = guest.get(statuses[1], 101);
    assert!(partial[..100].iter().all(|&b| b == 0xcc));
    assert_eq!(partial[100], STATUS_IOERR);
    let unsupported = guest.get(statuses[3], SECTOR + 1);
    assert!(unsupported[..SECTOR].iter().all(|&b| b == 0xcc));
    assert_eq!(unsupported[SECTOR], STATUS_UNSUPP);
    assert_eq!(view(device.store()), before);
    assert!(guest
        .get(last_buffers[0], SECTOR)
        .iter()
        .all(|&b| b == b'G'));
}

#[test]
fn malformed_requests_fail_after_completing_the_earlier_prefix() {
    type Bad = fn(&mut Guest) -> u16;
    let cases: [(&str, Bad); 6] = [
        ("readable after writable", |g| {
            let (h, _) = g.chain(&[
                Part::Read(header(1, 1)),
                Part::Write(1),
                Part::Read(vec![b'M'; SECTOR]),
            ]);
            h
        }),
        ("short header", |g| {
            g.chain(&[Part::Read(vec![1; 15]), Part::Write(1)]).0
        }),
        ("no status byte", |g| {
            g.chain(&[Part::Read(header(1, 1)), Part::Read(vec![b'M'; SECTOR])])
                .0
        }),
        ("descriptor outside RAM", |g| {
            let head = g.chain(&out(1, b'M', 1)).0;
            g.descriptor(head + 1, 1 << 40, SECTOR, DESCRIPTOR_NEXT, head + 2);
            head
        }),
        ("descriptor loop", |g| {
            let head = g.chain(&out(1, b'M', 1)).0;
            g.descriptor(
                head + 2,
                BUFFERS,
                1,
                DESCRIPTOR_NEXT | DESCRIPTOR_WRITE,
                head,
            );
            head
        }),
        ("indirect descriptor", |g| {
            let head = g.chain(&out(1, b'M', 1)).0;
            g.descriptor(head, BUFFERS, 16, DESCRIPTOR_NEXT | 4, head + 1);
            head
        }),
    ];
    for (name, bad) in cases {
        let (mut device, mut guest) = ready(2);
        let (first, _) = guest.chain(&out(0, b'P', 1));
        let malformed = bad(&mut guest);
        let (after, _) = guest.chain(&out(1, b'Q', 1));
        for head in [first, malformed, after] {
            guest.offer(head);
        }
        let used_before = guest.ram.clone();
        assert!(notify(&mut device, &mut guest).is_err(), "{name}");
        // Only the first request completed: its write and its used entry.
        assert_eq!(guest.used(), [(first, 1)], "{name}");
        assert_eq!(
            view(device.store()),
            (b"P.".to_vec(), b"..".to_vec()),
            "{name}"
        );
        let changed: Vec<usize> = (0..guest.ram.len())
            .filter(|&i| guest.ram[i] != used_before[i])
            .collect();
        let allowed = |i: usize| {
            let i = i as u64;
            (USED..USED + 4 + 8).contains(&i)
        };
        let status = changed.iter().filter(|&&i| !allowed(i)).count();
        assert_eq!(status, 1, "{name}: only the first status byte besides used");
    }
}

#[test]
fn notifies_before_driver_ok_or_for_other_queues() {
    let mut device = Block::new(Store::zeroed(1).unwrap(), Default::default());
    let mut guest = Guest::new();
    let (head, _) = guest.chain(&out(0, b'Z', 1));
    guest.offer(head);
    notify(&mut device, &mut guest).unwrap();
    assert!(guest.used().is_empty());
    assert!(device.write(&mut guest.ram, QUEUE_NOTIFY, 1).is_err());
    setup(&mut device, &mut guest);
    notify(&mut device, &mut guest).unwrap();
    assert_eq!(guest.used(), [(head, 1)]);
}

#[test]
fn suppressed_interrupts_still_complete_requests() {
    let (mut device, mut guest) = ready(1);
    guest.suppress_interrupts();
    let (head, _) = guest.chain(&out(0, b'S', 1));
    guest.offer(head);
    notify(&mut device, &mut guest).unwrap();
    assert_eq!(guest.used(), [(head, 1)]);
    assert!(!device.irq());
}

#[test]
fn driver_reset_clears_transport_and_preserves_the_store() {
    let (mut device, mut guest) = ready(2);
    for parts in [out(0, b'D', 1), flush(), out(1, b'U', 1)] {
        let (head, _) = guest.chain(&parts);
        guest.offer(head);
    }
    notify(&mut device, &mut guest).unwrap();
    let stored = view(device.store());
    assert_eq!(stored, (b"DU".to_vec(), b"D.".to_vec()));
    device.write(&mut guest.ram, STATUS, 0).unwrap();
    assert_eq!(
        [STATUS, INTERRUPT_STATUS, QUEUE_READY].map(|r| device.read(r).unwrap()),
        [0; 3]
    );
    assert_eq!(view(device.store()), stored, "reset is not a power loss");
    // A fresh driver restarts its ring and still reads the unflushed write.
    let mut fresh = Guest::new();
    setup(&mut device, &mut fresh);
    let (head, buffers) = fresh.chain(&read(1, 1));
    fresh.offer(head);
    notify(&mut device, &mut fresh).unwrap();
    assert_eq!(fresh.used(), [(head, SECTOR as u32 + 1)]);
    assert!(fresh.get(buffers[0], SECTOR).iter().all(|&b| b == b'U'));
}

#[test]
fn device_clones_are_independent_captures() {
    let (mut device, mut guest) = ready(2);
    let (head, _) = guest.chain(&out(0, b'C', 1));
    guest.offer(head);
    notify(&mut device, &mut guest).unwrap();
    let capture = device.clone();
    let mut ram = guest.ram.clone();
    for parts in [flush(), out(1, b'N', 1)] {
        let (head, _) = guest.chain(&parts);
        guest.offer(head);
    }
    notify(&mut device, &mut guest).unwrap();
    assert_eq!(view(capture.store()), (b"C.".to_vec(), b"..".to_vec()));
    // The capture resumes from its own ring cursor against its own RAM copy.
    let mut restored = capture.clone();
    restored.write(&mut ram, QUEUE_NOTIFY, 0).unwrap();
    assert_eq!(view(restored.store()), (b"C.".to_vec(), b"..".to_vec()));
    assert_eq!(view(capture.store()), (b"C.".to_vec(), b"..".to_vec()));
    assert_eq!(view(device.store()), (b"CN".to_vec(), b"C.".to_vec()));
}

#[test]
fn chain_size_limit_is_exact() {
    let limit = u32::MAX as usize;
    assert!(representable(limit, 0) && representable(0, limit));
    assert!(representable(limit - 1, 1));
    assert!(!representable(limit, 1) && !representable(1, limit));
    assert!(!representable(usize::MAX, 1));
}

#[test]
fn overlapping_chains_past_u32_fail_before_any_effect() {
    // 130 descriptors overlapping one 33 MiB span exceed 2^32 bytes in total.
    const SPAN: usize = 33 << 20;
    for writable in [false, true] {
        let mut device = Block::new(Store::zeroed(1 << 20).unwrap(), Default::default());
        let mut guest = Guest::with(SPAN + 0x10000, 256);
        setup(&mut device, &mut guest);
        let (first, _) = guest.chain(&out(0, b'P', 1));
        let h = guest.allocate(16);
        guest.put(h, &header(if writable { 0 } else { 1 }, 0));
        let span = guest.allocate(0) + 0x1000;
        let mut spans = vec![(h, 16, false)];
        spans.extend((0..130).map(|_| (span, SPAN - 0x10000, writable)));
        spans.push((h + 15, 1, true));
        let huge = guest.spans(&spans);
        guest.offer(first);
        guest.offer(huge);
        let before = guest.ram[span as usize..span as usize + 64].to_vec();
        let error = notify(&mut device, &mut guest).unwrap_err();
        assert!(error.contains("2^32"), "{error}");
        assert_eq!(guest.used(), [(first, 1)]);
        assert_eq!(&guest.ram[span as usize..span as usize + 64], before);
        assert_eq!(guest.get(h + 15, 1), [header(0, 0)[15]]);
        let mut sector = [0; SECTOR];
        device.store().read(0, &mut sector).unwrap();
        assert!(sector.iter().all(|&b| b == b'P'));
    }
}

#[test]
fn transport_and_store_restore_into_an_identical_device() {
    let (mut device, mut guest) = ready(8);
    let (head, _) = guest.chain(&out(3, 0x5a, 1));
    guest.offer(head);
    notify(&mut device, &mut guest).unwrap();
    let transport: Transport =
        serde_json::from_slice(&serde_json::to_vec(device.transport()).unwrap()).unwrap();
    let mut restored = Block::restore(transport, device.store().clone(), device.faults().clone());
    for offset in [QUEUE_READY, INTERRUPT_STATUS, STATUS] {
        assert_eq!(restored.read(offset).unwrap(), device.read(offset).unwrap());
    }
    // The restored queue cursor continues after the completed request.
    let (head, _) = guest.chain(&flush());
    guest.offer(head);
    let mut other = guest.ram.clone();
    restored.write(&mut guest.ram, QUEUE_NOTIFY, 0).unwrap();
    device.write(&mut other, QUEUE_NOTIFY, 0).unwrap();
    assert_eq!(guest.ram, other);
    assert_eq!(view(restored.store()), view(device.store()));
}

fn latency(op: seedbox_machine::pv::DiskOp, latency_ns: u64) -> DiskFaults {
    let mut faults = DiskFaults::new(0);
    faults
        .install(
            1,
            3,
            seedbox_machine::pv::DeviceFault::DiskLatency {
                op,
                latency_ns,
                jitter_ns: 0,
            },
        )
        .unwrap();
    faults
}

fn notify_at(device: &mut Block, guest: &mut Guest, now: u64) -> Option<u32> {
    device
        .write_traced(&mut guest.ram, QUEUE_NOTIFY, 0, now, &mut |_, _, _| {})
        .unwrap()
}

#[test]
fn delayed_requests_complete_late_in_submission_order_with_their_effects_then() {
    use seedbox_machine::pv::DiskOp;
    let mut device = Block::new(Store::zeroed(64).unwrap(), latency(DiskOp::Write, 1_000));
    let mut guest = Guest::new();
    setup(&mut device, &mut guest);
    let (write, _) = guest.chain(&out(2, b'W', 1));
    // A read the fault does not select still queues behind the write.
    let (read_back, data) = guest.chain(&read(2, 1));
    let (flushed, _) = guest.chain(&flush());
    for head in [write, read_back, flushed] {
        guest.offer(head);
    }
    assert_eq!(notify_at(&mut device, &mut guest, 500), None);
    assert!(guest.used().is_empty() && !device.irq());
    assert_eq!(device.next_due(), Some(1_500));
    assert!(device.store().read(2, &mut [0; SECTOR]).is_ok());
    let mut stored = [0; SECTOR];
    device.store().read(2, &mut stored).unwrap();
    assert_eq!(stored, [0; SECTOR], "no disk effect before completion");
    let mut completions = Vec::new();
    device
        .complete_due(&mut guest.ram, 1_499, &mut |k, s, _| {
            completions.push((k, s))
        })
        .unwrap();
    assert!(completions.is_empty());
    device
        .complete_due(&mut guest.ram, 1_500, &mut |k, s, _| {
            completions.push((k, s))
        })
        .unwrap();
    assert_eq!(
        completions,
        [(1, STATUS_OK), (0, STATUS_OK), (4, STATUS_OK)]
    );
    assert_eq!(
        guest.used(),
        [(write, 1), (read_back, SECTOR as u32 + 1), (flushed, 1)]
    );
    assert_eq!(guest.get(data[0], SECTOR), &[b'W'; SECTOR][..]);
    assert!(device.irq());
    assert_eq!(device.next_due(), None);
    // Healed with nothing pending, requests complete at notification again.
    assert_eq!(
        device.faults_mut().heal(1),
        Some(seedbox_machine::pv::FaultCounts {
            matched: 1,
            affected: 1
        })
    );
    let (again, _) = guest.chain(&read(2, 1));
    guest.offer(again);
    notify_at(&mut device, &mut guest, 2_000);
    assert_eq!(guest.used(), [(again, SECTOR as u32 + 1)]);
}

#[test]
fn a_driver_reset_drops_pending_requests_but_keeps_the_faults() {
    use seedbox_machine::pv::DiskOp;
    let mut device = Block::new(Store::zeroed(8).unwrap(), latency(DiskOp::Any, 10));
    let mut guest = Guest::new();
    setup(&mut device, &mut guest);
    let (write, _) = guest.chain(&out(0, 1, 1));
    guest.offer(write);
    notify_at(&mut device, &mut guest, 0);
    assert_eq!(device.next_due(), Some(10));
    device.write(&mut guest.ram, STATUS, 0).unwrap();
    assert_eq!(device.next_due(), None);
    assert_eq!(device.faults().installed().len(), 1);
    device
        .complete_due(&mut guest.ram, 100, &mut |_, _, _| {})
        .unwrap();
    assert!(guest.used().is_empty());
    assert_eq!(
        device.store().encode_overlays(),
        Store::zeroed(8).unwrap().encode_overlays()
    );
}

#[test]
fn a_hold_stops_processing_at_its_request_without_any_effect() {
    use seedbox_machine::machine::disk::fault::Hold;
    use seedbox_machine::pv::DiskOp;
    let mut faults = DiskFaults::new(0);
    faults
        .hold(Hold {
            trigger: 7,
            op: DiskOp::Flush,
            nth: 1,
            seen: 0,
            until: None,
        })
        .unwrap();
    let mut device = Block::new(Store::zeroed(8).unwrap(), faults);
    let mut guest = Guest::new();
    setup(&mut device, &mut guest);
    let (write, _) = guest.chain(&out(0, 9, 1));
    let (flushed, _) = guest.chain(&flush());
    let (after, _) = guest.chain(&out(1, 9, 1));
    for head in [write, flushed, after] {
        guest.offer(head);
    }
    assert_eq!(notify_at(&mut device, &mut guest, 5), Some(7));
    // The earlier write completed; the flush and everything after did not.
    assert_eq!(guest.used(), [(write, 1)]);
    let mut stored = [0; SECTOR];
    device.store().read(1, &mut stored).unwrap();
    assert_eq!(stored, [0; SECTOR]);
    assert!(device.faults().holds().is_empty());
}

#[test]
fn pending_requests_and_faults_survive_a_closed_snapshot() {
    use seedbox_machine::pv::DiskOp;
    use seedbox_machine::snapshot::Record;
    let mut device = Block::new(Store::zeroed(8).unwrap(), latency(DiskOp::Read, 40));
    let mut guest = Guest::new();
    setup(&mut device, &mut guest);
    let (first, _) = guest.chain(&read(0, 1));
    let (second, _) = guest.chain(&read(1, 1));
    guest.offer(first);
    guest.offer(second);
    notify_at(&mut device, &mut guest, 0);
    let mut bytes = Vec::new();
    device
        .transport()
        .encode(&mut seedbox_machine::snapshot::Writer::new(&mut bytes))
        .unwrap();
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 1 << 20,
        allocation_bytes: 1 << 20,
        collection_items: 64,
        total_items: 64,
        string_bytes: 64,
        blob_bytes: 1 << 20,
    };
    let mut input = &bytes[..];
    let mut reader =
        seedbox_machine::snapshot::Reader::new(&mut input, bytes.len() as u64, 0, &limits).unwrap();
    let transport = Transport::decode(&mut reader).unwrap();
    transport.validate_snapshot().unwrap();
    let json: Transport =
        serde_json::from_value(serde_json::to_value(device.transport()).unwrap()).unwrap();
    assert_eq!(json.pending, transport.pending);
    let mut restored = Block::restore(transport, device.store().clone(), device.faults().clone());
    assert_eq!(restored.next_due(), Some(40));
    let mut completions = Vec::new();
    restored
        .complete_due(&mut guest.ram, 40, &mut |k, s, sector| {
            completions.push((k, s, sector))
        })
        .unwrap();
    assert_eq!(completions, [(0, STATUS_OK, 0), (0, STATUS_OK, 1)]);
}
