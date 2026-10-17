//! Real KVM guests drive the virtio devices across checkpoints and crashes.
//!
//! NEXT-100 case 74 (a checkpoint is not a durability fence, and crashes
//! around unpaid I/O cost) and case 75 (incremental restoration through the
//! shadow's dirty log equals an independently retained full copy of RAM).
//! A tiny 32-bit guest stores to RAM and notifies the devices through real
//! MMIO exits; the host plays the parts of the driver a guest kernel would
//! already have done (feature negotiation, queue setup, posted buffers).
//! These are hardware tests: they run only with `--features kvm-tests` on a
//! Linux x86-64 KVM host.
use super::*;
use crate::disk::SECTOR;

const T_IN: u32 = 0;
const T_OUT: u32 = 1;
const T_FLUSH: u32 = 4;
const F_FLUSH: u32 = 1 << 9;
const S_OK: u8 = 0;
const DESC_NEXT: u16 = 1;
const DESC_WRITE: u16 = 2;
/// The legacy PC speaker port: a charged read that changes nothing.
const CHARGED_PORT: u8 = 0x61;
const BLK_NOTIFY: u32 = BLK_MMIO as u32 + 0x50;

/// A split virtqueue in guest RAM, written by the host before the guest runs.
struct Ring {
    desc: usize,
    avail: usize,
    used: usize,
}

impl Ring {
    fn descriptor(&self, m: &mut [u8], index: u16, addr: usize, len: u32, flags: u16, next: u16) {
        let d = self.desc + 16 * index as usize;
        m[d..d + 8].copy_from_slice(&(addr as u64).to_le_bytes());
        m[d + 8..d + 12].copy_from_slice(&len.to_le_bytes());
        m[d + 12..d + 14].copy_from_slice(&flags.to_le_bytes());
        m[d + 14..d + 16].copy_from_slice(&next.to_le_bytes());
    }
    /// Put `head` in avail slot `slot` without publishing it.
    fn offer(&self, m: &mut [u8], slot: u16, head: u16) {
        let at = self.avail + 4 + 2 * slot as usize;
        m[at..at + 2].copy_from_slice(&head.to_le_bytes());
    }
    fn publish(&self, m: &mut [u8], idx: u16) {
        m[self.avail + 2..self.avail + 4].copy_from_slice(&idx.to_le_bytes());
    }
    fn used_idx(&self, m: &[u8]) -> u16 {
        u16::from_le_bytes(m[self.used + 2..self.used + 4].try_into().unwrap())
    }
    /// The transport registers that make this queue `queue` ready.
    fn registers(&self, queue: u32, size: u32) -> [(u64, u32); 6] {
        [
            (0x030, queue),
            (0x038, size),
            (0x080, self.desc as u32),
            (0x090, self.avail as u32),
            (0x0a0, self.used as u32),
            (0x044, 1),
        ]
    }
}

/// A tiny protected-mode guest assembled by hand: 32-bit, flat, no paging.
#[derive(Default)]
struct Code(Vec<u8>);

impl Code {
    fn bytes(&mut self, bytes: &[u8]) -> &mut Self {
        self.0.extend_from_slice(bytes);
        self
    }
    fn imm32(&mut self, v: u32) -> &mut Self {
        self.bytes(&v.to_le_bytes())
    }
    /// mov dword [addr], imm32
    fn store32(&mut self, addr: u32, v: u32) -> &mut Self {
        self.bytes(&[0xc7, 0x05]).imm32(addr).imm32(v)
    }
    /// mov word [addr], imm16
    fn store16(&mut self, addr: u32, v: u16) -> &mut Self {
        self.bytes(&[0x66, 0xc7, 0x05])
            .imm32(addr)
            .bytes(&v.to_le_bytes())
    }
    /// mov [esi + disp32], eax
    fn store_eax(&mut self, disp: u32) -> &mut Self {
        self.bytes(&[0x89, 0x86]).imm32(disp)
    }
    fn mov_esi(&mut self, v: u32) -> &mut Self {
        self.bytes(&[0xbe]).imm32(v)
    }
    fn mov_eax(&mut self, v: u32) -> &mut Self {
        self.bytes(&[0xb8]).imm32(v)
    }
    fn add_eax(&mut self, v: u32) -> &mut Self {
        self.bytes(&[0x05]).imm32(v)
    }
    /// in al, port: an exit whose deterministic cost the machine then owes.
    fn charged_in(&mut self, port: u8) -> &mut Self {
        self.bytes(&[0xe4, port])
    }
    fn hlt(&mut self) -> &mut Self {
        self.bytes(&[0xf4])
    }
    /// The bzImage the boot path loads: a setup sector count, then the code.
    fn kernel(&self) -> Vec<u8> {
        let mut kernel = vec![0u8; 1024];
        kernel[0x1f1] = 1;
        kernel.extend_from_slice(&self.0);
        kernel
    }
}

struct Devices {
    disk: Option<Disk>,
    net: bool,
    console: bool,
}

fn boot(kvm: &Kvm, kernel: &[u8], devices: &Devices) -> Machine {
    let spec = BootSpec {
        kernel,
        initrd: &[],
        cmdline: String::new(),
        mem: INITRD_ADDR + 0x100000,
        rng_seed: [0; 32],
        mac: devices.net.then_some([2, 0, 0, 0, 0, 1]),
        control_channel: devices.console,
        disk: devices.disk.clone(),
        barrier: false,
        page_timer: false,
    };
    let m = Machine::new(kvm, "devices", &spec).unwrap();
    // Interrupts enabled so HLT is a halt, not an exit; the PIC stays masked,
    // so device interrupts are raised but never injected into this IDT-less guest.
    let mut r = m.vcpu.get_regs().unwrap();
    r.rflags = 0x202;
    m.vcpu.set_regs(&r).unwrap();
    m
}

fn ram(m: &mut Machine) -> &'static mut [u8] {
    // SAFETY: the mapping outlives every use in these single-threaded tests,
    // and no device write runs while the returned slice is used.
    unsafe { std::slice::from_raw_parts_mut(m.mem.0, m.mem_size) }
}

fn negotiate_block(m: &mut Machine, ring: &Ring, size: u32) {
    let memory = ram(m);
    let blk = m.blk.as_mut().unwrap();
    for (off, v) in [(0x020, F_FLUSH), (0x024, 1), (0x020, 1), (0x070, 15)]
        .into_iter()
        .chain(ring.registers(0, size))
    {
        blk.write(memory, off, v).unwrap();
    }
}

fn block_header(m: &mut [u8], at: usize, kind: u32, sector: u64) {
    m[at..at + 4].copy_from_slice(&kind.to_le_bytes());
    m[at + 4..at + 8].fill(0);
    m[at + 8..at + 16].copy_from_slice(&sector.to_le_bytes());
}

fn sector(disk: &Disk, s: u64) -> [u8; SECTOR] {
    let mut buf = [0; SECTOR];
    disk.read(s, &mut buf).unwrap();
    buf
}

fn clock() -> PvClock {
    PvClock::new(std::time::Duration::from_millis(20)).unwrap()
}

/// Crash `old` now: boot its successor from the same spec and hand it the
/// disk the crash policy leaves. `old` itself is left as it was.
fn crash(kvm: &Kvm, kernel: &[u8], devices: &Devices, old: &mut Machine) -> (Machine, Option<u64>) {
    let mut successor = boot(kvm, kernel, devices);
    let at = old.world.time;
    let lost = successor.succeed(old, at, 7, None).unwrap();
    (successor, lost)
}

// Block layout shared by case 74's guests: rings, headers, payloads, statuses.
const BLK_RING: Ring = Ring {
    desc: 0x200000,
    avail: 0x201000,
    used: 0x202000,
};
const HEADERS: usize = 0x210000;
const PAYLOADS: usize = 0x211000;
const STATUS: usize = 0x220000;

/// Post WRITE(sector a, 'A'), FLUSH, WRITE(sectors b.., 'B') as three
/// unpublished chains: the guest publishes and notifies each one itself.
fn post_write_flush_write(m: &mut Machine, a: u64, b: u64, b_sectors: usize) {
    let memory = ram(m);
    let ring = &BLK_RING;
    let mut index = 0;
    let requests = [
        (T_OUT, a, vec![b'A'; SECTOR]),
        (T_FLUSH, 0, Vec::new()),
        (T_OUT, b, vec![b'B'; b_sectors * SECTOR]),
    ];
    for (slot, (kind, sector, data)) in requests.iter().enumerate() {
        let head = index;
        let header = HEADERS + 16 * slot;
        block_header(memory, header, *kind, *sector);
        ring.descriptor(memory, index, header, 16, DESC_NEXT, index + 1);
        index += 1;
        if !data.is_empty() {
            let payload = PAYLOADS + 0x1000 * slot;
            memory[payload..payload + data.len()].copy_from_slice(data);
            ring.descriptor(
                memory,
                index,
                payload,
                data.len() as u32,
                DESC_NEXT,
                index + 1,
            );
            index += 1;
        }
        memory[STATUS + slot] = 0xff;
        ring.descriptor(memory, index, STATUS + slot, 1, DESC_WRITE, 0);
        index += 1;
        ring.offer(memory, slot as u16, head);
    }
}

/// Publish request `n` (one-based) and notify the block device, optionally
/// followed by a charged port read whose cost the machine then owes.
fn publish_and_notify(code: &mut Code, n: u16, charged: bool) {
    code.store16(BLK_RING.avail as u32 + 2, n)
        .store32(BLK_NOTIFY, 0);
    if charged {
        code.charged_in(CHARGED_PORT);
    }
}

fn statuses(m: &mut Machine) -> [u8; 3] {
    let memory = ram(m);
    [memory[STATUS], memory[STATUS + 1], memory[STATUS + 2]]
}

#[test]
#[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
fn a_checkpoint_is_not_a_durability_fence_for_written_unflushed_sectors() {
    let kvm = Kvm::new().unwrap();
    let mut code = Code::default();
    for n in 1..=3 {
        publish_and_notify(&mut code, n, false);
    }
    code.hlt();
    let kernel = code.kernel();
    let devices = Devices {
        disk: Some(Disk::new(None, Some(16)).unwrap()),
        net: false,
        console: false,
    };
    let mut m = boot(&kvm, &kernel, &devices);
    negotiate_block(&mut m, &BLK_RING, 16);
    // WRITE(A) to sector 3, FLUSH, then WRITE(B) over sector 3 and onto 4.
    post_write_flush_write(&mut m, 3, 3, 2);
    let stop = m
        .run_until(&mut clock(), 1_000_000, 0, &mut Vec::new())
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(BLK_RING.used_idx(ram(&mut m)), 3);
    assert_eq!(statuses(&mut m), [S_OK; 3]);
    let disk = m.blk.as_ref().unwrap().disk();
    // Read-your-writes: the guest already sees B.
    assert_eq!(sector(disk, 3), [b'B'; SECTOR]);
    assert_eq!((disk.stable_sectors(), disk.volatile_sectors()), (1, 2));
    let captured = disk.digest();

    let saved = m.snapshot().unwrap();
    // Capturing flushed nothing: the stable and volatile layers are unchanged
    // on the source and identical in the snapshot.
    let disk = m.blk.as_ref().unwrap().disk();
    assert_eq!(disk.digest(), captured);
    assert_eq!((disk.stable_sectors(), disk.volatile_sectors()), (1, 2));
    let MachineSnapshot::Memory(memory) = &saved else {
        panic!("a fresh snapshot is in memory");
    };
    let held = memory.blk.as_ref().unwrap().disk();
    assert_eq!(held.digest(), captured);
    assert_eq!((held.stable_sectors(), held.volatile_sectors()), (1, 2));

    // The same capture through its native checkpoint file.
    let directory = tempfile::tempdir().unwrap();
    saved
        .persist(&directory.path().join("machine.snapshot"))
        .unwrap();
    let file: MachineSnapshot =
        serde_json::from_slice(&serde_json::to_vec(&saved).unwrap()).unwrap();

    let mut from_memory = boot(&kvm, &kernel, &devices);
    from_memory.restore(&saved).unwrap();
    let mut from_file = boot(&kvm, &kernel, &devices);
    from_file.restore(&file).unwrap();
    for (what, machine) in [
        ("original", &mut m),
        ("restored from memory", &mut from_memory),
        ("restored from file", &mut from_file),
    ] {
        let disk = machine.blk.as_ref().unwrap().disk();
        assert_eq!(disk.digest(), captured, "{what}");
        assert_eq!(
            (disk.stable_sectors(), disk.volatile_sectors()),
            (1, 2),
            "{what}"
        );
        // Zero volatile survival: A was flushed and survives; B was only
        // written, so its two sectors are lost.
        let (successor, lost) = crash(&kvm, &kernel, &devices, machine);
        assert_eq!(lost, Some(2), "{what}");
        let disk = successor.blk.as_ref().unwrap().disk();
        assert_eq!(sector(disk, 3), [b'A'; SECTOR], "{what}");
        assert_eq!(sector(disk, 4), [0; SECTOR], "{what}");
        assert_eq!(
            (disk.stable_sectors(), disk.volatile_sectors()),
            (1, 0),
            "{what}"
        );
    }
}

/// What the original holds at one stop of the owed-I/O guest, and what its
/// successor's disk holds after a crash there.
struct Point {
    limit: u64,
    stop: Stop,
    owed: Option<u64>,
    used: u16,
    layers: (u64, u64),
    lost: u64,
    c_survives: bool,
}

#[test]
#[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
fn crashes_before_at_and_after_owed_io_keep_exactly_the_flushed_writes() {
    let kvm = Kvm::new().unwrap();
    // WRITE(C), FLUSH and WRITE(D) each complete at their MMIO notify; a
    // charged port read follows each, so the device mutation is committed
    // while the machine still owes 1000 ns of I/O cost.
    let mut code = Code::default();
    for n in 1..=3 {
        publish_and_notify(&mut code, n, true);
    }
    code.hlt();
    let kernel = code.kernel();
    let charge = io_charge(CHARGED_PORT as u16, false);
    assert_eq!(charge, 1000);
    let devices = Devices {
        disk: Some(Disk::new(None, Some(16)).unwrap()),
        net: false,
        console: false,
    };
    let mut m = boot(&kvm, &kernel, &devices);
    negotiate_block(&mut m, &BLK_RING, 16);
    post_write_flush_write(&mut m, 5, 6, 1);
    let points = [
        // WRITE(C) is in the volatile layer; its I/O cost is half paid.
        Point {
            limit: 500,
            stop: Stop::Reached,
            owed: Some(500),
            used: 1,
            layers: (0, 1),
            lost: 1,
            c_survives: false,
        },
        // Exactly at debt completion, before the guest notifies FLUSH.
        Point {
            limit: 1000,
            stop: Stop::Reached,
            owed: None,
            used: 1,
            layers: (0, 1),
            lost: 1,
            c_survives: false,
        },
        // FLUSH has completed although its cost is unpaid: C is durable.
        Point {
            limit: 1500,
            stop: Stop::Reached,
            owed: Some(500),
            used: 2,
            layers: (1, 0),
            lost: 0,
            c_survives: true,
        },
        Point {
            limit: 2000,
            stop: Stop::Reached,
            owed: None,
            used: 2,
            layers: (1, 0),
            lost: 0,
            c_survives: true,
        },
        // WRITE(D) completed with its cost owed; D is volatile.
        Point {
            limit: 2500,
            stop: Stop::Reached,
            owed: Some(500),
            used: 3,
            layers: (1, 1),
            lost: 1,
            c_survives: true,
        },
        // All cost paid and halted.
        Point {
            limit: 1_000_000,
            stop: Stop::Halted,
            owed: None,
            used: 3,
            layers: (1, 1),
            lost: 1,
            c_survives: true,
        },
    ];
    let mut clock = clock();
    let mut branch = None;
    for point in &points {
        let what = format!("stop at {} ns", point.limit);
        let stop = m
            .run_until(&mut clock, point.limit, 0, &mut Vec::new())
            .unwrap();
        assert_eq!(stop, point.stop, "{what}");
        assert_eq!(m.time, point.limit.min(3 * charge), "{what}");
        assert_eq!(
            m.pending_io.as_ref().map(|p| p.remaining_ns),
            point.owed,
            "{what}"
        );
        let used = BLK_RING.used_idx(ram(&mut m));
        assert_eq!(used, point.used, "{what}");
        let completed = statuses(&mut m);
        for (slot, status) in completed.iter().enumerate() {
            let want = if slot < used as usize { S_OK } else { 0xff };
            assert_eq!(*status, want, "{what}: status {slot}");
        }
        let disk = m.blk.as_ref().unwrap().disk();
        assert_eq!(
            (disk.stable_sectors(), disk.volatile_sectors()),
            point.layers,
            "{what}"
        );
        let (successor, lost) = crash(&kvm, &kernel, &devices, &mut m);
        assert_eq!(lost, Some(point.lost), "{what}");
        assert!(
            successor.pending_io.is_none(),
            "{what}: owed I/O is discarded"
        );
        let after = successor.blk.as_ref().unwrap().disk();
        let c = if point.c_survives {
            [b'A'; SECTOR]
        } else {
            [0; SECTOR]
        };
        assert_eq!(sector(after, 5), c, "{what}");
        assert_eq!(sector(after, 6), [0; SECTOR], "{what}: D is never flushed");
        assert_eq!(after.volatile_sectors(), 0, "{what}");
        if point.limit == 1500 {
            // Capture with the FLUSH's cost owed: the restored machine owes the
            // same debt, holds the same layers, and crashes the same way.
            let saved = m.snapshot().unwrap();
            let mut restored = boot(&kvm, &kernel, &devices);
            restored.restore(&saved).unwrap();
            assert_eq!(
                restored.pending_io.as_ref().map(|p| p.remaining_ns),
                Some(500)
            );
            assert_eq!(
                restored.blk.as_ref().unwrap().disk().digest(),
                m.blk.as_ref().unwrap().disk().digest()
            );
            let (successor, lost) = crash(&kvm, &kernel, &devices, &mut restored);
            assert_eq!(lost, Some(0));
            assert_eq!(
                successor.blk.as_ref().unwrap().disk().digest(),
                after.digest()
            );
            branch = Some(restored);
        }
    }
    // The branch pays the same debt and completes the same requests.
    let mut restored = branch.unwrap();
    let stop = restored
        .run_until(&mut clock, 1_000_000, 0, &mut Vec::new())
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(restored.time, 3 * charge);
    assert_eq!(restored.state_digest().0, m.state_digest().0);
}

// Case 75 layout. Each step k owns two data pages from DATA + 2k pages:
//   +0x000   guest store before DMA
//   +0xe00   block READ payload, 1024 bytes, crossing into the second page
//   +0xe04   guest store after DMA, inside the payload
//   +0xffe   guest store crossing the page boundary, inside the payload
//   +0x1100  guest store after DMA, inside the payload's second page
//   +0x1800  block status byte
//   +0x1c00  console receive buffer, 64 bytes
//   +0x1f80  network receive buffer, 256 bytes, crossing into the next step's
//            first page, where that step's first store lands later
const STEPS: usize = 6;
const DATA: usize = 0x300000;
const NET_RING: Ring = Ring {
    desc: 0x203000,
    avail: 0x204000,
    used: 0x205000,
};
const CONSOLE_RINGS: usize = 0x206000;
const CONSOLE_PAYLOADS: usize = 0x230000;

fn step_page(k: usize) -> usize {
    DATA + 2 * k * 0x1000
}

fn console_ring(q: usize) -> Ring {
    let base = CONSOLE_RINGS + q * 0x1000;
    Ring {
        desc: base,
        avail: base + 0x200,
        used: base + 0x300,
    }
}

/// Every device ready and every buffer the suffixes need already posted, so
/// later RAM changes come only from guest stores and device DMA.
fn prepare_dma_guest(m: &mut Machine) {
    let memory = ram(m);
    // Block: one READ chain per step, each published later by the guest.
    negotiate_block(m, &BLK_RING, 32);
    for k in 0..STEPS {
        let page = step_page(k);
        let header = HEADERS + 16 * k;
        block_header(memory, header, T_IN, 2 * k as u64);
        let head = 3 * k as u16;
        BLK_RING.descriptor(memory, head, header, 16, DESC_NEXT, head + 1);
        BLK_RING.descriptor(
            memory,
            head + 1,
            page + 0xe00,
            2 * SECTOR as u32,
            DESC_WRITE | DESC_NEXT,
            head + 2,
        );
        BLK_RING.descriptor(memory, head + 2, page + 0x1800, 1, DESC_WRITE, 0);
        BLK_RING.offer(memory, k as u16, head);
    }
    // Network: receive buffers, all published, consumed one per frame.
    let net = m.net.as_mut().unwrap();
    for (off, v) in [(0x024, 1), (0x020, 1), (0x070, 15)]
        .into_iter()
        .chain(NET_RING.registers(0, 16))
    {
        net.write(memory, off, v).unwrap();
    }
    for k in 0..STEPS {
        NET_RING.descriptor(memory, k as u16, step_page(k) + 0x1f80, 256, DESC_WRITE, 0);
        NET_RING.offer(memory, k as u16, k as u16);
    }
    NET_RING.publish(memory, STEPS as u16);
    let net = m.net.as_mut().unwrap();
    net.write(memory, 0x050, 0).unwrap();
    // Console: the multiport handshake, then receive buffers on port 0.
    let channel = m.channel.as_mut().unwrap();
    for (off, v) in [(0x020, 2), (0x024, 1), (0x020, 1), (0x070, 11)] {
        channel.write(memory, off, v).unwrap();
    }
    for q in 0..4 {
        for (off, v) in console_ring(q).registers(q as u32, 8) {
            channel.write(memory, off, v).unwrap();
        }
    }
    channel.write(memory, 0x070, 15).unwrap();
    let mut posted = [0u16; 4];
    let mut post = |memory: &mut [u8], q: usize, at: usize, bytes: &[u8], writable: bool| {
        let ring = console_ring(q);
        let head = posted[q];
        memory[at..at + bytes.len()].copy_from_slice(bytes);
        let flags = if writable { DESC_WRITE } else { 0 };
        ring.descriptor(memory, head, at, bytes.len() as u32, flags, 0);
        ring.offer(memory, head, head);
        posted[q] += 1;
        ring.publish(memory, posted[q]);
    };
    for head in 0..3 {
        post(memory, 2, CONSOLE_PAYLOADS + head * 0x100, &[0; 32], true);
    }
    // DEVICE_READY, PORT_READY and PORT_OPEN, as the guest driver sends them.
    for (n, (id, event)) in [(u32::MAX, 0u16), (0, 3), (0, 6)].into_iter().enumerate() {
        let mut packet = id.to_le_bytes().to_vec();
        packet.extend_from_slice(&event.to_le_bytes());
        packet.extend_from_slice(&1u16.to_le_bytes());
        post(
            memory,
            3,
            CONSOLE_PAYLOADS + 0x1000 + n * 0x100,
            &packet,
            false,
        );
        m.channel.as_mut().unwrap().write(memory, 0x050, 3).unwrap();
    }
    for k in 0..STEPS {
        post(memory, 0, step_page(k) + 0x1c00, &[0; 64], true);
    }
    m.channel.as_mut().unwrap().write(memory, 0x050, 0).unwrap();
    // Everything above was the VMM writing RAM before any fork point; the
    // first fork point copies every page, so the journals start clean here.
    m.take_dma();
}

/// Each step stores around a block READ's DMA, then halts for the host.
fn dma_guest() -> Vec<u8> {
    let mut code = Code::default();
    code.mov_esi(DATA as u32).mov_eax(0x1111_1111);
    for k in 0..STEPS {
        let page = (step_page(k) - DATA) as u32;
        code.store_eax(page)
            .store_eax(page + 0xffe)
            .store16(BLK_RING.avail as u32 + 2, k as u16 + 1)
            .store32(BLK_NOTIFY, 0)
            .store_eax(page + 0xe04)
            .store_eax(page + 0x1100)
            .add_eax(0x0101_0101)
            .hlt();
    }
    code.kernel()
}

/// One suffix after the checkpoint: the eax the guest stores, then for each
/// halt whether the host delivers a frame and console bytes before the next
/// step. The first entry acts at the checkpoint's own halt.
struct Suffix {
    eax: u32,
    steps: Vec<(bool, bool)>,
}

fn run_suffix(m: &mut Machine, clock: &mut PvClock, suffix: &Suffix, salt: u8) {
    let mut regs = m.vcpu.get_regs().unwrap();
    regs.rax = suffix.eax as u64;
    m.vcpu.set_regs(&regs).unwrap();
    for (i, &(frame, console)) in suffix.steps.iter().enumerate() {
        if frame {
            let bytes: Vec<u8> = (0..200).map(|b| b as u8 ^ salt ^ i as u8).collect();
            assert!(m.deliver_frame(&bytes).unwrap().accepted);
        }
        if console {
            let bytes = vec![salt.wrapping_add(i as u8); 40];
            m.deliver_control(&bytes).unwrap();
        }
        if i + 1 == suffix.steps.len() {
            break;
        }
        let at = m.time;
        m.wake(at);
        let stop = m
            .run_until(clock, at + 1_000_000, 0, &mut Vec::new())
            .unwrap();
        assert_eq!(stop, Stop::Halted);
    }
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

#[test]
#[cfg_attr(not(feature = "kvm-tests"), ignore = "requires Linux x86-64 KVM")]
fn incremental_restore_after_cpu_and_dma_suffixes_equals_the_retained_copy() {
    let kvm = Kvm::new().unwrap();
    let kernel = dma_guest();
    let image: Vec<u8> = (0..64 * SECTOR)
        .map(|i| (i / SECTOR * 7 + 1) as u8)
        .collect();
    let devices = Devices {
        disk: Some(Disk::new(Some(image), None).unwrap()),
        net: true,
        console: true,
    };
    let mut m = boot(&kvm, &kernel, &devices);
    prepare_dma_guest(&mut m);
    let mut clock = clock();
    // Step 0 runs before the checkpoint, so its pages already hold DMA.
    let stop = m
        .run_until(&mut clock, 1_000_000, 0, &mut Vec::new())
        .unwrap();
    assert_eq!(stop, Stop::Halted);
    assert_eq!(BLK_RING.used_idx(ram(&mut m)), 1);
    let saved = m.snapshot().unwrap();
    // The independent copy: plain bytes, never touched by the shadow.
    let retained = m.memory().to_vec();
    let digest = m.state_digest().0;
    let total = (m.mem_size / 4096) as u64;
    let suffixes = [
        Suffix {
            eax: 0x2222_2222,
            steps: vec![(true, true), (false, false), (true, false)],
        },
        Suffix {
            eax: 0xa5a5_a5a5,
            steps: vec![(true, false); STEPS],
        },
        // Device DMA only: no guest instruction runs.
        Suffix {
            eax: 0,
            steps: vec![(true, true)],
        },
        Suffix {
            eax: 0x5a5a_5a5a,
            steps: vec![(false, false), (false, true), (false, false), (false, true)],
        },
    ];
    let mut results = Vec::new();
    for (n, suffix) in suffixes.iter().enumerate() {
        run_suffix(&mut m, &mut clock, suffix, n as u8 * 17);
        let after = m.memory().to_vec();
        assert_ne!(after, retained, "suffix {n} changed nothing");
        results.push(after);
        let pages = m.restore(&saved).unwrap();
        assert!(
            pages > 0 && pages < total,
            "suffix {n}: {pages} of {total} pages copied back; the dirty log was not used"
        );
        let restored = m.memory().to_vec();
        assert_eq!(
            first_difference(&restored, &retained),
            None,
            "suffix {n}: RAM differs from the retained copy after restore"
        );
        assert_eq!(m.state_digest().0, digest, "suffix {n}");
    }
    // Repeating a suffix after the others reproduces its RAM exactly: no
    // write from an earlier suffix survived a restore.
    for (n, suffix) in suffixes.iter().enumerate().rev() {
        run_suffix(&mut m, &mut clock, suffix, n as u8 * 17);
        assert_eq!(
            first_difference(m.memory(), &results[n]),
            None,
            "suffix {n} repeated"
        );
        m.restore(&saved).unwrap();
        assert_eq!(first_difference(m.memory(), &retained), None, "suffix {n}");
    }
    // A fresh VM restores by full copy and then runs a suffix identically.
    let mut fresh = boot(&kvm, &kernel, &devices);
    assert_eq!(fresh.restore(&saved).unwrap(), total);
    assert_eq!(first_difference(fresh.memory(), &retained), None);
    assert_eq!(fresh.state_digest().0, digest);
    run_suffix(&mut fresh, &mut clock, &suffixes[0], 0);
    assert_eq!(first_difference(fresh.memory(), &results[0]), None);
}
