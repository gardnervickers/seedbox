// Exercise the actual portable device implementation without requiring KVM.
#[rustfmt::skip]
#[path = "../../seedbox-vmm/src/virtio_console.rs"]
mod virtio_console;
use virtio_console::VirtioConsole;

struct Guest {
    device: VirtioConsole,
    ram: Vec<u8>,
    avail: [u16; 4],
}
fn base(q: usize) -> usize {
    0x1000 + q * 0x1000
}
fn u16_at(m: &[u8], a: usize) -> u16 {
    u16::from_le_bytes(m[a..a + 2].try_into().unwrap())
}
fn put16(m: &mut [u8], a: usize, v: u16) {
    m[a..a + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(m: &mut [u8], a: usize, v: u32) {
    m[a..a + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(m: &mut [u8], a: usize, v: u64) {
    m[a..a + 8].copy_from_slice(&v.to_le_bytes());
}
impl Guest {
    fn new() -> Self {
        let mut g = Self {
            device: VirtioConsole::new(),
            ram: vec![0; 0x30000],
            avail: [0; 4],
        };
        for (off, val) in [(0x020, 2), (0x024, 1), (0x020, 1), (0x070, 11)] {
            g.write(off, val);
        }
        assert_eq!(g.device.read(0x070), 11);
        for q in 0..4 {
            for (off, val) in [
                (0x030, q as u32),
                (0x038, 8),
                (0x080, base(q) as u32),
                (0x090, (base(q) + 0x200) as u32),
                (0x0a0, (base(q) + 0x300) as u32),
                (0x044, 1),
            ] {
                g.write(off, val);
            }
        }
        g.write(0x070, 15);
        g
    }
    fn write(&mut self, off: u64, val: u32) -> virtio_console::WriteOutcome {
        self.device.write(&mut self.ram, off, val).unwrap()
    }
    fn post(&mut self, q: usize, head: u16, bytes: &[u8], writable: bool) -> usize {
        let payload = 0x10000 + q * 0x4000 + head as usize * 0x400;
        self.ram[payload..payload + bytes.len()].copy_from_slice(bytes);
        let desc = base(q) + head as usize * 16;
        put64(&mut self.ram, desc, payload as u64);
        put32(&mut self.ram, desc + 8, bytes.len() as u32);
        put16(&mut self.ram, desc + 12, if writable { 2 } else { 0 });
        let avail = base(q) + 0x200;
        put16(
            &mut self.ram,
            avail + 4 + 2 * (self.avail[q] % 8) as usize,
            head,
        );
        self.avail[q] = self.avail[q].wrapping_add(1);
        put16(&mut self.ram, avail + 2, self.avail[q]);
        payload
    }
    fn control(
        &mut self,
        head: u16,
        id: u32,
        event: u16,
        value: u16,
    ) -> virtio_console::WriteOutcome {
        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_le_bytes());
        packet.extend_from_slice(&event.to_le_bytes());
        packet.extend_from_slice(&value.to_le_bytes());
        self.post(3, head, &packet, false);
        self.write(0x050, 3)
    }
    fn open() -> Self {
        let mut g = Self::new();
        for head in 0..3 {
            g.post(2, head, &[0; 32], true);
        }
        let out = g.control(0, u32::MAX, 0, 1);
        assert!(out.irq && out.bytes.is_empty());
        let out = g.control(1, 0, 3, 1);
        assert!(out.irq && out.bytes.is_empty());
        g.control(2, 0, 6, 1);
        g
    }
}
#[test]
fn standard_non_console_port_handshake_names_port_and_signals_dma() {
    let mut g = Guest::open();
    assert_eq!(g.device.read(0x008), 3);
    assert_eq!(g.device.read(0x104), 1);
    assert_eq!(u16_at(&g.ram, base(2) + 0x302), 3);
    let packets: Vec<_> = (0..3)
        .map(|i| &g.ram[0x18000 + i * 0x400..0x18000 + i * 0x400 + 8])
        .collect();
    assert_eq!(u16_at(packets[0], 4), 1);
    assert_eq!(u16_at(packets[1], 4), 7);
    assert_eq!(u16_at(packets[2], 4), 6);
    assert_eq!(&g.ram[0x18408..0x1840f], b"seedbox");
    assert!(!g.device.dirty.is_empty());
    assert_eq!(g.device.read(0x060), 1);
    g.write(0x064, 1);
    assert_eq!(g.device.read(0x060), 0);
}
#[test]
fn transmit_is_bulk_stream_across_chains_and_fragmented_notifications() {
    let mut g = Guest::open();
    g.post(1, 0, b"hello", false);
    // Extend one descriptor chain into a second descriptor, not a second ring entry.
    let second = 0x14500;
    g.ram[second..second + 1].copy_from_slice(b" ");
    put16(&mut g.ram, base(1) + 12, 1);
    put16(&mut g.ram, base(1) + 14, 1);
    put64(&mut g.ram, base(1) + 16, second as u64);
    put32(&mut g.ram, base(1) + 24, 1);
    assert_eq!(g.write(0x050, 1).bytes, b"hello ");
    g.post(1, 2, b"world\n", false);
    assert_eq!(g.write(0x050, 1).bytes, b"world\n");
    assert_eq!(u16_at(&g.ram, base(1) + 0x302), 2);
    assert!(g.write(0x050, 1).bytes.is_empty()); // no duplicate consumption
}
#[test]
fn receive_retains_partial_bytes_and_journals_every_dma_write() {
    let mut g = Guest::open();
    g.device.dirty.clear();
    let first = g.post(0, 0, &[0; 3], true);
    let before = g.ram.clone();
    let result = g.device.receive(&mut g.ram, b"abcdef").unwrap();
    assert!(result.accepted && result.irq);
    assert_eq!(&g.ram[first..first + 3], b"abc");
    assert_eq!(g.device.pending_rx_bytes(), 3);
    for (i, (&a, &b)) in before.iter().zip(&g.ram).enumerate() {
        if a != b {
            assert!(g
                .device
                .dirty
                .iter()
                .any(|&(start, len)| i >= start && i < start + len));
        }
    }
    let second = g.post(0, 1, &[0; 8], true);
    assert!(g.write(0x050, 0).irq);
    assert_eq!(&g.ram[second..second + 3], b"def");
    assert_eq!(g.device.pending_rx_bytes(), 0);
}
#[test]
fn receive_backpressure_is_bounded_and_reset_preserves_uncollected_dirty_ranges() {
    let mut g = Guest::open();
    g.device.receive(&mut g.ram, &vec![1; 65536]).unwrap();
    assert_eq!(g.device.pending_rx_bytes(), 65536);
    let before = g.ram.clone();
    assert!(g.device.receive(&mut g.ram, b"!").is_err());
    assert_eq!(before, g.ram);
    let dirty = g.device.dirty.clone();
    g.write(0x070, 0);
    assert_eq!(g.device.pending_rx_bytes(), 0);
    assert_eq!(g.device.read(0x070), 0);
    assert_eq!(g.device.read(0x060), 0);
    assert_eq!(g.device.dirty, dirty);
    assert!(g.device.receive(&mut g.ram, b"offline").is_err());
}
#[test]
fn descriptor_validation_rejects_bounds_cycles_direction_and_ring_overrun_before_dma() {
    for defect in 0..7 {
        let mut g = Guest::open();
        g.post(1, 0, b"payload", false);
        let d = base(1);
        match defect {
            0 => put64(&mut g.ram, d, u64::MAX),
            1 => put32(&mut g.ram, d + 8, 65537),
            2 => {
                put16(&mut g.ram, d + 12, 1);
                put16(&mut g.ram, d + 14, 0);
            }
            3 => put16(&mut g.ram, d + 12, 2),
            4 => put16(&mut g.ram, d + 12, 4),
            5 => put16(&mut g.ram, d + 0x204, 8),
            6 => put16(&mut g.ram, d + 0x202, 9),
            _ => unreachable!(),
        }
        let before = g.ram.clone();
        let dirty = g.device.dirty.clone();
        assert!(
            g.device.write(&mut g.ram, 0x050, 1).is_err(),
            "defect {defect}"
        );
        assert_eq!(g.ram, before);
        assert_eq!(g.device.dirty, dirty);
    }
}
#[test]
fn queue_configuration_rejects_bad_sizes_selectors_and_unnegotiated_features() {
    let mut d = VirtioConsole::new();
    let mut m = vec![0; 4096];
    for size in [0, 3, 257] {
        assert!(d.write(&mut m, 0x038, size).is_err());
    }
    d.write(&mut m, 0x030, 4).unwrap();
    assert_eq!(d.read(0x034), 0);
    assert!(d.write(&mut m, 0x044, 1).is_err());
    d.write(&mut m, 0x070, 8).unwrap();
    assert_eq!(d.read(0x070) & 8, 0);
    assert!(d.write(&mut m, 0x070, 12).is_err());
}
