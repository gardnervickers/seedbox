use super::super::boot::RAM_BASE as BASE;
use super::*;

const SIZE: u16 = 16;
// Per-queue ring layout relative to RAM: [descriptors, available, used].
const RINGS: [[u64; 3]; 2] = [[0x0000, 0x1000, 0x2000], [0x3000, 0x4000, 0x5000]];
const BUFFERS: u64 = 0x8000;
const ACKNOWLEDGE_DRIVER: u32 = 1 | 2;
const MAC: [u8; 6] = [0x52, 0x54, 0, 0, 0, 7];

/// A minimal two-queue split-ring driver writing chains into guest RAM.
struct Guest {
    ram: Vec<u8>,
    next_descriptor: [u16; 2],
    available: [u16; 2],
    used_seen: [u16; 2],
    next_buffer: u64,
}

impl Guest {
    fn new(ram: usize) -> Self {
        Self {
            ram: vec![0; ram],
            next_descriptor: [0; 2],
            available: [0; 2],
            used_seen: [0; 2],
            next_buffer: BUFFERS,
        }
    }
    fn put(&mut self, at: u64, bytes: &[u8]) {
        self.ram[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    }
    fn get(&self, at: u64, length: usize) -> &[u8] {
        &self.ram[at as usize..at as usize + length]
    }
    fn allocate(&mut self, length: usize) -> u64 {
        let at = self.next_buffer;
        self.next_buffer += (length as u64).div_ceil(64).max(1) * 64;
        at
    }
    /// Lay out a chain from (address, length, writable) spans on `queue`.
    fn spans(&mut self, queue: usize, spans: &[(u64, usize, bool)]) -> u16 {
        let head = self.next_descriptor[queue];
        for (i, &(address, length, writable)) in spans.iter().enumerate() {
            let index = self.next_descriptor[queue];
            let next = (index + 1) % SIZE;
            self.next_descriptor[queue] = next;
            let flags = ((i + 1 < spans.len()) as u16 * DESCRIPTOR_NEXT)
                | (writable as u16 * DESCRIPTOR_WRITE);
            let at = RINGS[queue][0] + index as u64 * 16;
            self.put(at, &(BASE + address).to_le_bytes());
            self.put(at + 8, &(length as u32).to_le_bytes());
            self.put(at + 12, &flags.to_le_bytes());
            self.put(at + 14, &next.to_le_bytes());
        }
        head
    }
    fn offer(&mut self, queue: usize, head: u16) {
        let available = RINGS[queue][1];
        let slot = available + 4 + (self.available[queue] % SIZE) as u64 * 2;
        self.put(slot, &head.to_le_bytes());
        self.available[queue] = self.available[queue].wrapping_add(1);
        let index = self.available[queue];
        self.put(available + 2, &index.to_le_bytes());
    }
    /// Offer one TX chain holding `packet` split at `cuts`.
    fn send(&mut self, packet: &[u8], cuts: &[usize]) -> u16 {
        let mut spans = Vec::new();
        let mut start = 0;
        for end in cuts.iter().copied().chain([packet.len()]) {
            let at = self.allocate(end - start);
            self.put(at, &packet[start..end]);
            spans.push((at, end - start, false));
            start = end;
        }
        let head = self.spans(TX, &spans);
        self.offer(TX, head);
        head
    }
    /// Offer one writable RX buffer of `length` bytes.
    fn post(&mut self, length: usize) -> (u16, u64) {
        let at = self.allocate(length);
        self.put(at, &vec![0xcc; length]);
        let head = self.spans(RX, &[(at, length, true)]);
        self.offer(RX, head);
        (head, at)
    }
    fn suppress_interrupts(&mut self, queue: usize) {
        self.put(RINGS[queue][1], &AVAILABLE_NO_INTERRUPT.to_le_bytes());
    }
    /// New used entries as (head, length), in ring order.
    fn used(&mut self, queue: usize) -> Vec<(u16, u32)> {
        let used = RINGS[queue][2];
        let index = u16::from_le_bytes(self.get(used + 2, 2).try_into().unwrap());
        let mut entries = Vec::new();
        while self.used_seen[queue] != index {
            let at = used + 4 + (self.used_seen[queue] % SIZE) as u64 * 8;
            let entry = self.get(at, 8);
            entries.push((
                u32::from_le_bytes(entry[..4].try_into().unwrap()) as u16,
                u32::from_le_bytes(entry[4..].try_into().unwrap()),
            ));
            self.used_seen[queue] = self.used_seen[queue].wrapping_add(1);
        }
        entries
    }
}

fn write(device: &mut Net, guest: &mut Guest, offset: u64, value: u32) -> Result<()> {
    device.write(&mut guest.ram, offset, 4, value, 0)
}

/// Negotiate like Linux virtio-mmio, enabling the given queues.
fn setup(device: &mut Net, guest: &mut Guest, queues: &[usize], driver_ok: bool) {
    let mut w = |offset, value| write(device, guest, offset, value).unwrap();
    w(STATUS, ACKNOWLEDGE_DRIVER);
    w(DRIVER_FEATURES_SEL, 0);
    w(DRIVER_FEATURES, FEATURES);
    w(DRIVER_FEATURES_SEL, 1);
    w(DRIVER_FEATURES, FEATURE_VERSION_1_HIGH);
    w(STATUS, ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK);
    for &queue in queues {
        w(QUEUE_SEL, queue as u32);
        w(QUEUE_NUM, SIZE as u32);
        for (i, (low, high)) in [
            (QUEUE_DESC_LOW, QUEUE_DESC_HIGH),
            (QUEUE_AVAIL_LOW, QUEUE_AVAIL_HIGH),
            (QUEUE_USED_LOW, QUEUE_USED_HIGH),
        ]
        .into_iter()
        .enumerate()
        {
            let address = BASE + RINGS[queue][i];
            w(low, address as u32);
            w(high, (address >> 32) as u32);
        }
        w(QUEUE_READY, 1);
    }
    if driver_ok {
        w(
            STATUS,
            ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
        );
    }
}

fn ready() -> (Net, Guest) {
    let mut device = Net::new(MAC);
    let mut guest = Guest::new(1 << 20);
    setup(&mut device, &mut guest, &[RX, TX], true);
    (device, guest)
}

fn ethernet(length: usize, tag: u8) -> Vec<u8> {
    (0..length).map(|i| tag ^ i as u8).collect()
}

fn packet(frame: &[u8]) -> Vec<u8> {
    let mut packet = vec![0; HEADER];
    packet.extend_from_slice(frame);
    packet
}

fn notify(device: &mut Net, guest: &mut Guest, queue: usize, now: u64) -> Result<()> {
    device.write(&mut guest.ram, QUEUE_NOTIFY, 4, queue as u32, now)
}

#[test]
fn identifies_as_a_mac_mtu_network_device() {
    let mut device = Net::new(MAC);
    for (offset, value) in [
        (MAGIC, 0x74726976),
        (VERSION, 2),
        (DEVICE_ID, 1),
        (DEVICE_FEATURES, (1 << 3) | (1 << 5)),
        (QUEUE_NUM_MAX, 256),
        (CONFIG_GENERATION, 0),
    ] {
        assert_eq!(device.read(offset, 4).unwrap(), value, "{offset:#x}");
    }
    let mut ram = vec![0; 64];
    device
        .write(&mut ram, DEVICE_FEATURES_SEL, 4, 1, 0)
        .unwrap();
    assert_eq!(device.read(DEVICE_FEATURES, 4).unwrap(), 1);
    device.write(&mut ram, QUEUE_SEL, 4, 1, 0).unwrap();
    assert_eq!(device.read(QUEUE_NUM_MAX, 4).unwrap(), 256);
    device.write(&mut ram, QUEUE_SEL, 4, 2, 0).unwrap();
    assert_eq!(
        device.read(QUEUE_NUM_MAX, 4).unwrap(),
        0,
        "no control queue"
    );
    assert!(device.write(&mut ram, QUEUE_NUM, 4, 8, 0).is_err());
    // Linux reads the MAC bytewise and the MTU as a 16-bit field.
    let mac: Vec<u8> = (0..6)
        .map(|i| device.read(0x100 + i, 1).unwrap() as u8)
        .collect();
    assert_eq!(mac, MAC);
    assert_eq!(device.read(0x10a, 2).unwrap(), 1500);
    assert_eq!(device.read(0x100, 4).unwrap(), 0x0000_5452);
    assert_eq!(device.read(0x106, 2).unwrap(), 0, "status not offered");
    assert_eq!(device.read(0x108, 4).unwrap(), 1500 << 16);
    for (offset, width) in [(0x101, 2), (0x10b, 2), (0x10c, 1), (0x100, 8), (0x100, 3)] {
        assert!(device.read(offset, width).is_err(), "{offset:#x}/{width}");
    }
    assert!(
        device.read(STATUS, 1).is_err(),
        "common registers are 32-bit"
    );
    assert!(
        device.write(&mut ram, 0x100, 1, 0, 0).is_err(),
        "MAC is read-only"
    );
    assert!(device.write(&mut ram, STATUS, 2, 0, 0).is_err());
}

#[test]
fn rejects_unoffered_or_legacy_feature_sets() {
    let checksum = 1;
    let control_queue = 1 << 17;
    let merged_buffers = 1 << 15;
    for (low, high) in [
        (FEATURES | checksum, 1),
        (FEATURES | control_queue, 1),
        (FEATURES | merged_buffers, 1),
        (FEATURES, 0),
        (FEATURES, 1 | 2),
    ] {
        let mut device = Net::new(MAC);
        let mut ram = vec![0; 64];
        device.write(&mut ram, DRIVER_FEATURES, 4, low, 0).unwrap();
        device
            .write(&mut ram, DRIVER_FEATURES_SEL, 4, 1, 0)
            .unwrap();
        device.write(&mut ram, DRIVER_FEATURES, 4, high, 0).unwrap();
        assert!(device
            .write(&mut ram, STATUS, 4, STATUS_FEATURES_OK, 0)
            .is_err());
    }
}

#[test]
fn tx_stamps_frames_at_notify_in_guest_order_without_header() {
    let (mut device, mut guest) = ready();
    let a = ethernet(14, 1);
    let b = ethernet(1514, 2);
    let c = ethernet(60, 3);
    // Header split across descriptors, header-only first descriptor, one span.
    let heads = [
        guest.send(&packet(&a), &[5]),
        guest.send(&packet(&b), &[12, 700]),
        guest.send(&packet(&c), &[]),
    ];
    notify(&mut device, &mut guest, TX, 41).unwrap();
    assert_eq!(guest.used(TX), heads.map(|h| (h, 0)));
    assert!(device.irq());
    let d = ethernet(100, 4);
    let late = guest.send(&packet(&d), &[]);
    notify(&mut device, &mut guest, TX, 57).unwrap();
    assert_eq!(guest.used(TX), [(late, 0)]);
    let frames = device.take_transmitted();
    assert_eq!(
        frames,
        [(41, a), (41, b), (41, c), (57, d)].map(|(time, bytes)| Frame { time, bytes })
    );
    assert!(device.take_transmitted().is_empty());
    assert_eq!(device.stats().tx_frames, 4);
}

#[test]
fn tx_drops_bad_lengths_and_offload_headers_but_completes_them() {
    let (mut device, mut guest) = ready();
    let mut offload = packet(&ethernet(60, 5));
    offload[0] = 1; // NEEDS_CSUM
    let mut segmented = packet(&ethernet(60, 6));
    segmented[1] = 1; // GSO_TCPV4
                      // Fields other than flags and gso_type are ignored, not errors.
    let mut ignored = packet(&ethernet(60, 7));
    ignored[2..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
    let heads = [
        guest.send(&packet(&ethernet(13, 1)), &[]),
        guest.send(&packet(&ethernet(1515, 2)), &[]),
        guest.send(&[0; 11], &[]),
        guest.send(&offload, &[]),
        guest.send(&segmented, &[3]),
        guest.send(&ignored, &[]),
    ];
    notify(&mut device, &mut guest, TX, 3).unwrap();
    assert_eq!(guest.used(TX), heads.map(|h| (h, 0)));
    let frames = device.take_transmitted();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].bytes, ethernet(60, 7));
    assert_eq!(device.stats().tx_dropped, 5);
}

#[test]
fn tx_rejects_a_giant_chain_without_gathering_it() {
    let (mut device, mut guest) = ready();
    // Sixteen overlapping 512 KiB descriptors: 8 MiB claimed from 512 KiB RAM.
    let at = guest.allocate(512 << 10);
    let spans = vec![(at, 512 << 10, false); SIZE as usize];
    let head = guest.spans(TX, &spans);
    guest.offer(TX, head);
    notify(&mut device, &mut guest, TX, 0).unwrap();
    assert_eq!(guest.used(TX), [(head, 0)]);
    assert!(device.take_transmitted().is_empty());
    assert_eq!(device.stats().tx_dropped, 1);
}

#[test]
fn malformed_tx_is_an_error_after_completed_prefix() {
    let (mut device, mut guest) = ready();
    let good = guest.send(&packet(&ethernet(20, 1)), &[]);
    let buffer = guest.allocate(64);
    let writable = guest.spans(TX, &[(buffer, 64, true)]);
    guest.offer(TX, writable);
    assert!(notify(&mut device, &mut guest, TX, 9).is_err());
    assert_eq!(guest.used(TX), [(good, 0)]);
    assert_eq!(device.take_transmitted().len(), 1);
    // A descriptor loop is malformed too.
    let (mut device, mut guest) = ready();
    let buffer = guest.allocate(64);
    let head = guest.spans(TX, &[(buffer, 32, false), (buffer, 32, false)]);
    let last = RINGS[TX][0] + (head as u64 + 1) * 16;
    guest.put(last + 12, &DESCRIPTOR_NEXT.to_le_bytes());
    guest.put(last + 14, &head.to_le_bytes());
    guest.offer(TX, head);
    assert!(notify(&mut device, &mut guest, TX, 9).is_err());
    assert!(guest.used(TX).is_empty());
    assert!(notify(&mut device, &mut guest, 2, 9).is_err(), "no queue 2");
}

#[test]
fn rx_writes_modern_header_and_frame() {
    let (mut device, mut guest) = ready();
    let (head, at) = guest.post(1526);
    let frame = ethernet(1514, 9);
    assert_eq!(
        device.receive(&mut guest.ram, frame.clone()),
        Ok(Received::Delivered)
    );
    assert_eq!(guest.used(RX), [(head, 1526)]);
    assert_eq!(guest.get(at, 12), [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(guest.get(at + 12, 1514), frame);
    assert!(device.irq());
    // A split buffer and a buffer larger than needed.
    let a = guest.allocate(5);
    let b = guest.allocate(100);
    guest.put(b, &[0xcc; 100]);
    let head = guest.spans(RX, &[(a, 5, true), (b, 100, true)]);
    guest.offer(RX, head);
    let frame = ethernet(20, 3);
    device.receive(&mut guest.ram, frame.clone()).unwrap();
    assert_eq!(guest.used(RX), [(head, 32)]);
    assert_eq!(guest.get(a, 5), [0; 5]);
    assert_eq!(guest.get(b, 7), [0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(guest.get(b + 7, 20), frame);
    assert_eq!(guest.get(b + 27, 1), [0xcc]);
}

#[test]
fn short_rx_buffer_completes_empty_and_drops_only_that_frame() {
    let (mut device, mut guest) = ready();
    let at = guest.allocate(64);
    guest.put(at, &[0xcc; 64]);
    // One byte short of header plus frame, split across two descriptors.
    let short = guest.spans(RX, &[(at, 40, true), (at + 40, 31, true)]);
    guest.offer(RX, short);
    let (full, full_at) = guest.post(1526);
    assert_eq!(
        device.receive(&mut guest.ram, ethernet(60, 1)),
        Ok(Received::Dropped)
    );
    assert_eq!(guest.used(RX), [(short, 0)]);
    assert_eq!(guest.get(at, 64), [0xcc; 64], "no payload DMA");
    assert!(device.irq());
    // The ring advances: the next frame takes the next buffer.
    assert_eq!(
        device.receive(&mut guest.ram, ethernet(60, 2)),
        Ok(Received::Delivered)
    );
    assert_eq!(guest.used(RX), [(full, 72)]);
    assert_eq!(guest.get(full_at + 12, 60), ethernet(60, 2));
    // An exact fit is not short.
    let (exact, _) = guest.post(12 + 14);
    device.receive(&mut guest.ram, ethernet(14, 3)).unwrap();
    assert_eq!(guest.used(RX), [(exact, 26)]);
    assert_eq!(device.stats().rx_dropped, 1);
}

#[test]
fn host_frame_length_outside_bounds_is_a_caller_error() {
    let (mut device, mut guest) = ready();
    guest.post(1526);
    for length in [0, 13, 1515, 65536] {
        assert!(device.receive(&mut guest.ram, vec![0; length]).is_err());
    }
    // Even a driver that is not ready does not hide the contract violation.
    let mut idle = Net::new(MAC);
    assert!(idle.receive(&mut guest.ram, vec![0; 13]).is_err());
    assert!(guest.used(RX).is_empty());
    assert_eq!(device.stats().rx_dropped, 0);
}

#[test]
fn rx_drops_before_driver_ok_or_while_rx_is_not_ready() {
    // Queues ready but no DRIVER_OK.
    let mut device = Net::new(MAC);
    let mut guest = Guest::new(1 << 20);
    setup(&mut device, &mut guest, &[RX, TX], false);
    guest.post(1526);
    assert_eq!(
        device.receive(&mut guest.ram, ethernet(60, 1)),
        Ok(Received::Dropped)
    );
    // DRIVER_OK but RX never enabled.
    let mut device = Net::new(MAC);
    let mut guest = Guest::new(1 << 20);
    setup(&mut device, &mut guest, &[TX], true);
    assert_eq!(
        device.receive(&mut guest.ram, ethernet(60, 1)),
        Ok(Received::Dropped)
    );
    assert_eq!(device.stats().rx_dropped, 1);
    assert!(guest.used(RX).is_empty() && !device.irq());
}

#[test]
fn backlog_holds_256_frames_in_order_then_tail_drops_and_refill_drains() {
    let (mut device, mut guest) = ready();
    let frames: Vec<_> = (0..BACKLOG + 3)
        .map(|i| ethernet(14 + i, i as u8))
        .collect();
    for (i, frame) in frames.iter().enumerate() {
        let expected = if i < BACKLOG {
            Received::Backlogged
        } else {
            Received::Dropped
        };
        assert_eq!(device.receive(&mut guest.ram, frame.clone()), Ok(expected));
    }
    assert!(!device.irq());
    // Admission drains the backlog head first, then queues behind it.
    let (head, at) = guest.post(1526);
    let late = ethernet(60, 0xee);
    assert_eq!(
        device.receive(&mut guest.ram, late.clone()),
        Ok(Received::Backlogged)
    );
    assert_eq!(guest.used(RX), [(head, 12 + 14)]);
    assert_eq!(guest.get(at + 12, 14), frames[0]);
    let mut expected: Vec<_> = frames[1..BACKLOG].to_vec();
    expected.push(late);
    let mut delivered = 0;
    while delivered < expected.len() {
        let mut posted = Vec::new();
        for _ in 0..SIZE.min((expected.len() - delivered) as u16) {
            posted.push(guest.post(1526));
        }
        notify(&mut device, &mut guest, RX, 0).unwrap();
        let used = guest.used(RX);
        assert_eq!(used.len(), posted.len());
        for ((head, at), (used_head, length)) in posted.into_iter().zip(used) {
            let frame = &expected[delivered];
            assert_eq!((used_head, length as usize), (head, 12 + frame.len()));
            assert_eq!(guest.get(at + 12, frame.len()), frame);
            delivered += 1;
        }
    }
    // Empty again: the next frame goes straight to a fresh buffer.
    let (head, _) = guest.post(1526);
    assert_eq!(
        device.receive(&mut guest.ram, ethernet(60, 1)),
        Ok(Received::Delivered)
    );
    assert_eq!(guest.used(RX), [(head, 72)]);
    assert_eq!(device.stats().rx_dropped, 3);
    assert_eq!(device.stats().rx_frames, BACKLOG as u64 + 2);
}

#[test]
fn admission_delivers_backlog_into_unnotified_buffers_before_the_new_frame() {
    let (mut device, mut guest) = ready();
    let first = ethernet(30, 1);
    let second = ethernet(40, 2);
    assert_eq!(
        device.receive(&mut guest.ram, first.clone()),
        Ok(Received::Backlogged)
    );
    // Posted without a refill notify.
    let (a, a_at) = guest.post(1526);
    let (b, b_at) = guest.post(1526);
    assert_eq!(
        device.receive(&mut guest.ram, second.clone()),
        Ok(Received::Delivered)
    );
    assert_eq!(guest.used(RX), [(a, 42), (b, 52)]);
    assert_eq!(guest.get(a_at + 12, 30), first);
    assert_eq!(guest.get(b_at + 12, 40), second);
}

#[test]
fn notifies_before_driver_ok_are_ignored() {
    let mut device = Net::new(MAC);
    let mut guest = Guest::new(1 << 20);
    setup(&mut device, &mut guest, &[RX, TX], false);
    guest.send(&packet(&ethernet(60, 1)), &[]);
    guest.post(1526);
    notify(&mut device, &mut guest, TX, 0).unwrap();
    notify(&mut device, &mut guest, RX, 0).unwrap();
    assert!(guest.used(TX).is_empty() && guest.used(RX).is_empty());
    assert!(device.take_transmitted().is_empty() && !device.irq());
}

#[test]
fn malformed_rx_ring_is_an_error() {
    let (mut device, mut guest) = ready();
    let buffer = guest.allocate(1526);
    let head = guest.spans(RX, &[(buffer, 1526, false)]);
    guest.offer(RX, head);
    assert!(device.receive(&mut guest.ram, ethernet(60, 1)).is_err());
    assert!(guest.used(RX).is_empty());
}

#[test]
fn no_interrupt_flag_suppresses_irq_and_ack_clears_it() {
    let (mut device, mut guest) = ready();
    guest.suppress_interrupts(TX);
    guest.send(&packet(&ethernet(60, 1)), &[]);
    notify(&mut device, &mut guest, TX, 0).unwrap();
    assert_eq!(guest.used(TX).len(), 1);
    assert!(!device.irq());
    guest.post(1526);
    device.receive(&mut guest.ram, ethernet(60, 1)).unwrap();
    assert!(device.irq());
    assert_eq!(device.read(INTERRUPT_STATUS, 4).unwrap(), 1);
    write(&mut device, &mut guest, INTERRUPT_ACK, 1).unwrap();
    assert!(!device.irq());
}

#[test]
fn reset_and_rx_disable_clear_backlog_but_keep_emitted_frames() {
    let (mut device, mut guest) = ready();
    device.receive(&mut guest.ram, ethernet(60, 1)).unwrap();
    guest.send(&packet(&ethernet(60, 2)), &[]);
    notify(&mut device, &mut guest, TX, 5).unwrap();
    write(&mut device, &mut guest, STATUS, 0).unwrap();
    assert!(!device.irq());
    assert_eq!(device.read(STATUS, 4).unwrap(), 0);
    assert_eq!(device.read(QUEUE_READY, 4).unwrap(), 0);
    assert_eq!(device.take_transmitted().len(), 1);
    // A fresh negotiation sees nothing from before the reset.
    let mut guest = Guest::new(1 << 20);
    setup(&mut device, &mut guest, &[RX, TX], true);
    guest.post(1526);
    notify(&mut device, &mut guest, RX, 0).unwrap();
    assert!(guest.used(RX).is_empty());
    // Disabling RX drops its backlog too.
    let (mut device, mut guest) = ready();
    device.receive(&mut guest.ram, ethernet(60, 1)).unwrap();
    write(&mut device, &mut guest, QUEUE_SEL, RX as u32).unwrap();
    write(&mut device, &mut guest, QUEUE_READY, 0).unwrap();
    write(&mut device, &mut guest, QUEUE_READY, 1).unwrap();
    guest.post(1526);
    notify(&mut device, &mut guest, RX, 0).unwrap();
    assert!(guest.used(RX).is_empty());
}

#[test]
fn snapshot_retains_backlog_cursor_and_untaken_tx_without_repeating() {
    let (mut device, mut guest) = ready();
    let backlogged = ethernet(70, 1);
    device.receive(&mut guest.ram, backlogged.clone()).unwrap();
    guest.send(&packet(&ethernet(80, 2)), &[]);
    notify(&mut device, &mut guest, TX, 12).unwrap();
    let image = serde_json::to_vec(&device).unwrap();
    let ram = guest.ram.clone();
    let mut branches = Vec::new();
    for _ in 0..2 {
        let mut restored: Net = serde_json::from_slice(&image).unwrap();
        let mut guest = Guest {
            ram: ram.clone(),
            next_descriptor: guest.next_descriptor,
            available: guest.available,
            used_seen: guest.used_seen,
            next_buffer: guest.next_buffer,
        };
        // The consumed TX chain is not transmitted again.
        notify(&mut restored, &mut guest, TX, 20).unwrap();
        let (_, at) = guest.post(1526);
        notify(&mut restored, &mut guest, RX, 20).unwrap();
        assert_eq!(guest.get(at + 12, 70), backlogged);
        notify(&mut restored, &mut guest, RX, 21).unwrap();
        assert_eq!(guest.used(RX).len(), 1);
        branches.push((restored.take_transmitted(), guest.ram));
    }
    assert_eq!(branches[0], branches[1]);
    assert_eq!(
        branches[0].0,
        [Frame {
            time: 12,
            bytes: ethernet(80, 2)
        }]
    );
    // The source still owns its own copy.
    assert_eq!(device.take_transmitted().len(), 1);
}

/// One RX ring placement as RAM offsets: (size, descriptors, available,
/// used, buffers), with one 128-byte buffer per descriptor.
type Placement = (u16, u64, u64, u64, u64);

/// Negotiate RX at `rx` and TX at a fixed placement, as a fresh driver does.
fn negotiate(device: &mut Net, ram: &mut [u8], rx: Placement) {
    let mut w = |offset, value| device.write(ram, offset, 4, value, 0).unwrap();
    w(STATUS, ACKNOWLEDGE_DRIVER);
    w(DRIVER_FEATURES_SEL, 0);
    w(DRIVER_FEATURES, FEATURES);
    w(DRIVER_FEATURES_SEL, 1);
    w(DRIVER_FEATURES, FEATURE_VERSION_1_HIGH);
    w(STATUS, ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK);
    for (queue, (size, descriptors, available, used, _)) in
        [(RX, rx), (TX, (2, 0xe00, 0xe40, 0xe80, 0))]
    {
        w(QUEUE_SEL, queue as u32);
        w(QUEUE_NUM, size as u32);
        for (low, high, address) in [
            (QUEUE_DESC_LOW, QUEUE_DESC_HIGH, descriptors),
            (QUEUE_AVAIL_LOW, QUEUE_AVAIL_HIGH, available),
            (QUEUE_USED_LOW, QUEUE_USED_HIGH, used),
        ] {
            w(low, (BASE + address) as u32);
            w(high, ((BASE + address) >> 32) as u32);
        }
        w(QUEUE_READY, 1);
    }
    w(
        STATUS,
        ACKNOWLEDGE_DRIVER | STATUS_FEATURES_OK | STATUS_DRIVER_OK,
    );
}

/// Post the 128-byte buffer of descriptor `slot` and publish it.
fn post_slot(ram: &mut [u8], (size, descriptors, available, _, buffers): Placement, slot: u16) {
    let mut put = |at: u64, bytes: &[u8]| {
        ram[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
    };
    let at = descriptors + 16 * slot as u64;
    put(at, &(BASE + buffers + 128 * slot as u64).to_le_bytes());
    put(at + 8, &128u32.to_le_bytes());
    put(at + 12, &DESCRIPTOR_WRITE.to_le_bytes());
    let index = u16::from_le_bytes(
        ram[available as usize + 2..available as usize + 4]
            .try_into()
            .unwrap(),
    );
    let entry = available + 4 + 2 * (index % size) as u64;
    ram[entry as usize..entry as usize + 2].copy_from_slice(&slot.to_le_bytes());
    ram[available as usize + 2..available as usize + 4]
        .copy_from_slice(&index.wrapping_add(1).to_le_bytes());
}

/// The device's serialized state without its diagnostic counters, which a
/// driver reset deliberately keeps.
fn semantic(device: &Net) -> serde_json::Value {
    let mut value = serde_json::to_value(device).unwrap();
    value.as_object_mut().unwrap().remove("stats");
    value
}

/// A driver reset after a partial receive: the first frame was DMAed and
/// interrupted, the second waits in the backlog, and the driver resets
/// through STATUS with the interrupt still asserted, then negotiates fresh
/// rings elsewhere. Nothing from before the reset may reach the new rings or
/// the interrupt, the old rings are guest memory again, and the device is
/// indistinguishable from one that only saw the fresh negotiation. The NIC
/// is backend-neutral: this covers the ARM and x86 workers alike.
#[test]
fn reset_after_a_partial_receive_leaves_no_stale_frame_or_interrupt() {
    const OLD: Placement = (2, 0x100, 0x200, 0x300, 0x400);
    const NEW: Placement = (4, 0x800, 0x900, 0xa00, 0xc00);
    let within = |(size, descriptors, available, used, buffers): Placement, at: usize| {
        let n = size as u64;
        [
            (descriptors, 16 * n),
            (available, 6 + 2 * n),
            (used, 6 + 8 * n),
            (buffers, 128 * n),
        ]
        .iter()
        .any(|&(start, length)| (start..start + length).contains(&(at as u64)))
    };
    let (first, second, third) = (ethernet(60, 1), ethernet(60, 2), ethernet(60, 3));
    let mut device = Net::new(MAC);
    let mut ram = vec![0; 0x1000];
    negotiate(&mut device, &mut ram, OLD);
    post_slot(&mut ram, OLD, 0);
    assert_eq!(
        device.receive(&mut ram, first.clone()),
        Ok(Received::Delivered)
    );
    assert_eq!(&ram[OLD.4 as usize + HEADER..][..60], first);
    // No second buffer: the frame waits in the backlog.
    assert_eq!(
        device.receive(&mut ram, second.clone()),
        Ok(Received::Backlogged)
    );
    assert_eq!(device.backlog.len(), 1);
    assert_eq!(device.interrupt_status(), USED_RING_INTERRUPT);

    let before = ram.clone();
    device.write(&mut ram, STATUS, 4, 0, 0).unwrap();
    assert_eq!(ram, before, "a reset writes no guest RAM");
    assert!(!device.irq(), "a reset drops the asserted interrupt");
    assert_eq!(device.read(STATUS, 4).unwrap(), 0);
    assert!(device.backlog.is_empty());
    for queue in &device.queues {
        assert!(!queue.ready);
        assert_eq!((queue.size, queue.cursor), (0, 0));
    }
    let at_reset = ram.clone();

    negotiate(&mut device, &mut ram, NEW);
    post_slot(&mut ram, NEW, 0);
    post_slot(&mut ram, NEW, 1);
    let posted = ram.clone();
    // The driver kicks RX after refilling; the stale frame must not appear.
    device
        .write(&mut ram, QUEUE_NOTIFY, 4, RX as u32, 0)
        .unwrap();
    assert!(!device.irq(), "the stale frame raised an interrupt");
    assert_eq!(ram, posted, "the kick completed a stale frame");

    assert_eq!(
        device.receive(&mut ram, third.clone()),
        Ok(Received::Delivered)
    );
    assert!(device.irq());
    assert_eq!(&ram[NEW.4 as usize + HEADER..][..60], third);
    let used = NEW.3 as usize;
    assert_eq!(&ram[used + 2..used + 4], 1u16.to_le_bytes());
    assert_eq!(&ram[used + 4..used + 12], [0, 0, 0, 0, 72, 0, 0, 0]);
    assert_eq!(device.queues[RX].cursor, 1);
    // Every byte changed since the reset belongs to the fresh rings; the old
    // rings are untouched.
    for (at, (&now, &then)) in ram.iter().zip(&at_reset).enumerate() {
        if now != then {
            assert!(
                within(NEW, at),
                "byte {at:#x} written outside the new rings"
            );
        }
        if within(OLD, at) {
            assert_eq!(now, then, "old ring byte {at:#x}");
        }
    }
    assert!(!ram.windows(60).any(|w| w == second));

    let mut fresh = Net::new(MAC);
    let mut fresh_ram = at_reset;
    negotiate(&mut fresh, &mut fresh_ram, NEW);
    post_slot(&mut fresh_ram, NEW, 0);
    post_slot(&mut fresh_ram, NEW, 1);
    fresh
        .write(&mut fresh_ram, QUEUE_NOTIFY, 4, RX as u32, 0)
        .unwrap();
    fresh.receive(&mut fresh_ram, third).unwrap();
    assert_eq!(fresh_ram, ram);
    assert_eq!(semantic(&fresh), semantic(&device));
    // Only the diagnostic counters remember the frame received before the reset.
    assert_eq!((device.stats().rx_frames, fresh.stats().rx_frames), (2, 1));
}
