use super::*;
use std::{io, ops::Range};
const INITRD_LEN: u64 = b"exact-initramfs".len() as u64;
fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 64 << 20,
        allocation_bytes: 128 << 20,
        collection_items: 50000,
        total_items: 200000,
        string_bytes: 1 << 20,
        blob_bytes: 32 << 20,
    }
}
/// Linux boot-header fixture only: never translated, booted or executed.
#[cfg(target_arch = "x86_64")]
fn kernel() -> Vec<u8> {
    let mut k = vec![0; 3072];
    k[0x1f1] = 4;
    k[0x202..0x206].copy_from_slice(b"HdrS");
    k[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
    k[0x211] = 1;
    k[0x236..0x238].copy_from_slice(&1u16.to_le_bytes());
    k[0x22c..0x230].copy_from_slice(&0x7fffffffu32.to_le_bytes());
    k
}
#[cfg(target_arch = "aarch64")]
fn kernel() -> Vec<u8> {
    let mut k = vec![0; 4096];
    k[16..24].copy_from_slice(&4096u64.to_le_bytes());
    k[56..60].copy_from_slice(b"ARM\x64");
    k
}
fn machine(memory: u64, disk: Option<DiskImage>) -> PVMachine {
    PVMachine::construct_with_memory(
        Path::new("/never-launch-this"),
        kernel(),
        b"exact-initramfs".to_vec(),
        "rdinit=/init".into(),
        disk,
        None,
        memory,
    )
    .unwrap()
}
fn encode(s: &Snapshot) -> Vec<u8> {
    let mut b = vec![];
    s.write_to(&mut b).unwrap();
    b
}
fn decode(b: &[u8]) -> Result<Snapshot> {
    Snapshot::read_from(&mut &b[..], &limits())
}
fn closure(s: &Snapshot) {
    let wire = encode(s);
    for _ in 0..2 {
        let d = decode(&wire).unwrap();
        assert_eq!(&*d.bytes, &*s.bytes);
        assert_eq!(d.disk, s.disk);
        assert_eq!(encode(&d), wire);
    }
}
fn metadata(s: &Snapshot) -> Capture {
    let mut b = &s.bytes[..];
    protocol::receive(&mut b).unwrap().unwrap()
}
fn envelope(body: Vec<u8>, native: u8, extra: u64) -> Vec<u8> {
    let mut out = vec![];
    let mut w = Writer::new(&mut out);
    w.raw(MAGIC).unwrap();
    w.u32(VERSION).unwrap();
    w.u8(ARCH).unwrap();
    w.u8(native).unwrap();
    w.u16(0).unwrap();
    w.u64(body.len() as u64 + extra).unwrap();
    w.raw(&body).unwrap();
    out
}
fn unchecked(
    c: &Capture,
    n: Option<&worker::snapshot::Header>,
    disk: Option<&DiskImage>,
    lengths: [u64; 6],
    raw: &[&[u8]],
    extra: u64,
) -> Vec<u8> {
    let mut b = vec![];
    let mut w = Writer::new(&mut b);
    c.encode(&mut w).unwrap();
    retained::Aux {
        active: c.live.then_some(0),
        ..Default::default()
    }
    .encode(&mut w)
    .unwrap();
    if let Some(n) = n {
        n.write(&mut w).unwrap();
    }
    match disk {
        None => w.u8(0).unwrap(),
        Some(d) => {
            w.u8(if d.image().is_some() { 2 } else { 1 }).unwrap();
            w.u64(d.capacity_bytes()).unwrap();
        }
    }
    for l in lengths {
        w.u64(l).unwrap();
    }
    for body in raw {
        w.raw(body).unwrap();
    }
    envelope(b, if n.is_some() { NATIVE } else { 0 }, extra)
}
#[test]
fn deferred_source_independent_reusable_and_compact_sparse_capacity() {
    for memory in [2 << 20, 2048 << 20, 65536 << 20] {
        #[cfg(target_arch = "aarch64")]
        if memory > 2048 << 20 {
            continue;
        }
        let mut m = machine(memory, Some(DiskImage::zeroed(1 << 40).unwrap()));
        let path = m.directory.path().to_owned();
        let s = m.capture().unwrap();
        let wire = encode(&s);
        assert!(wire.len() < 16000, "RAM and zero base remain compact");
        drop(m);
        assert!(!path.exists());
        drop(s);
        for _ in 0..2 {
            let d = decode(&wire).unwrap();
            assert_eq!(encode(&d), wire);
            let restored = PVMachine::from_snapshot(Path::new("/never-launch-this"), &d).unwrap();
            assert!(restored.worker.is_none());
            assert_eq!(restored.memory_bytes, memory);
        }
    }
}
#[test]
fn disk_base_both_exact_layers_zero_sector_and_nonlive_shutdown_survive() {
    for image in [false, true] {
        let disk = if image {
            DiskImage::from_bytes(vec![9; 2048]).unwrap()
        } else {
            DiskImage::zeroed(2048).unwrap()
        };
        let mut m = machine(2 << 20, Some(disk));
        let store = m.disk.as_mut().unwrap().store.as_mut().unwrap();
        store.write(0, &[4; 512]).unwrap();
        store.flush();
        store.write(0, &[0; 512]).unwrap();
        store.write(2, &[7; 512]).unwrap();
        m.pending_boots = 0;
        m.reached = 42;
        m.seed = 99;
        m.state = State::Exited;
        m.shutdown = true;
        let s = m.capture().unwrap();
        drop(m);
        closure(&s);
        let d = decode(&encode(&s)).unwrap();
        let mut restored = PVMachine::from_snapshot(Path::new("/never-launch-this"), &d).unwrap();
        let store = restored.disk.as_mut().unwrap().store.as_mut().unwrap();
        let mut b = [1; 512];
        store.read(0, &mut b).unwrap();
        assert_eq!(b, [0; 512]);
        store.discard_unflushed();
        store.read(0, &mut b).unwrap();
        assert_eq!(b, [4; 512]);
        store.read(2, &mut b).unwrap();
        assert_eq!(b, if image { [9; 512] } else { [0; 512] });
    }
}
#[test]
fn failed_writer_bounded_chunks_no_source_mutation_and_error_reader() {
    struct Sink {
        count: usize,
        max: usize,
        fail: Option<usize>,
    }
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            if self.fail.is_some_and(|n| self.count >= n) {
                return Err(io::Error::other("injected write"));
            }
            self.max = self.max.max(b.len());
            let n = self.fail.map_or(b.len(), |l| b.len().min(l - self.count));
            self.count += n;
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut m = machine(
        2 << 20,
        Some(DiskImage::from_bytes(vec![6; 200 * 512]).unwrap()),
    );
    let s = m.capture().unwrap();
    let initial = encode(&s);
    let original = s.bytes.clone();
    for fail in [0, 7, 24, 100, initial.len() - 1] {
        let mut sink = Sink {
            count: 0,
            max: 0,
            fail: Some(fail),
        };
        assert!(matches!(s.write_to(&mut sink), Err(CodecError::Io(_))));
        assert_eq!(encode(&s), initial);
        assert!(std::sync::Arc::ptr_eq(&s.bytes, &original));
    }
    let mut sink = Sink {
        count: 0,
        max: 0,
        fail: None,
    };
    s.write_to(&mut sink).unwrap();
    assert_eq!(sink.count, initial.len());
    assert!(sink.max <= 65536);
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("injected read"))
        }
    }
    assert!(matches!(
        Snapshot::read_from(&mut Broken, &limits()),
        Err(CodecError::Io(_))
    ));
}
#[test]
fn every_small_prefix_header_tags_trailing_framing_and_limits() {
    let mut m = machine(2 << 20, None);
    let s = m.capture().unwrap();
    let b = encode(&s);
    for n in 0..b.len() {
        assert!(decode(&b[..n]).is_err(), "prefix {n}");
    }
    for at in [0usize, 8, 12, 13, 14, 16] {
        let mut bad = b.clone();
        bad[at] ^= 255;
        assert!(decode(&bad).is_err(), "offset {at}");
    }
    let mut bad = b.clone();
    bad.push(0);
    assert!(decode(&bad).is_err());
    for kind in 0..6 {
        let mut l = limits();
        match kind {
            0 => l.encoded_bytes = 23,
            1 => l.allocation_bytes = 0,
            2 => l.collection_items = 0,
            3 => l.total_items = 0,
            4 => l.string_bytes = 1,
            5 => l.blob_bytes = 1,
            _ => unreachable!(),
        };
        // The fixture's bridge collections are empty, so add one used identity to
        // exercise item quotas rather than pretending zero forbids empty records.
        let wire = if matches!(kind, 2 | 3) {
            let mut c = metadata(&s);
            let mut v = serde_json::to_value(&c.guest).unwrap();
            v["codec"]["used_ids"] = serde_json::json!([1]);
            c.guest = serde_json::from_value(v).unwrap();
            unchecked(
                &c,
                None,
                None,
                [kernel().len() as u64, INITRD_LEN, 0, 0, 0, 0],
                &[&kernel(), b"exact-initramfs"],
                0,
            )
        } else {
            b.clone()
        };
        assert!(
            Snapshot::read_from(&mut &wire[..], &l).is_err(),
            "limit {kind}"
        );
    }
}
#[test]
fn owner_state_boundary_and_disk_mismatches_are_rejected() {
    let mut m = machine(2 << 20, None);
    let s = m.capture().unwrap();
    for kind in 0..14 {
        let mut c = metadata(&s);
        match kind {
            0 => c.state = State::Invalid,
            1 => c.pending_boots = 2,
            2 => c.pending_at = Some(0),
            3 => c.input = vec![1],
            4 => {
                c.writes = vec![seedbox_machine::pv::transport::GuestWrite::Console(
                    seedbox_machine::pv::transport::LifecycleWrite {
                        time: 0,
                        bytes: vec![1],
                    },
                )]
            }
            5 => c.seed = 1,
            6 => c.reached = 1,
            7 => c.shutdown = true,
            8 => {
                c.pending_boots = 0;
                c.reached = 9;
                c.state = State::Blocked(Some(9));
            }
            9 => c.disk = Some(1),
            10 => c.layout_version = 0,
            11 => c.command_line = "bad\0args".into(),
            12 => c.memory_bytes = 1,
            13 => {
                c.live = true;
                c.pending_boots = 0;
            }
            _ => unreachable!(),
        }
        let b = unchecked(
            &c,
            None,
            None,
            [kernel().len() as u64, INITRD_LEN, 0, 0, 0, 0],
            &[&kernel(), b"exact-initramfs"],
            0,
        );
        assert!(decode(&b).is_err(), "kind {kind}");
    }
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
fn live(disk: bool) -> Snapshot {
    let memory = 2 << 20;
    let base = disk.then(|| DiskImage::from_bytes(vec![8; 2048]).unwrap());
    let mut m = machine(memory, base.clone());
    let mut c = metadata(&m.capture().unwrap());
    c.live = true;
    c.pending_boots = 0;
    c.reached = 7;
    c.seed = 55;
    c.state = State::Blocked(Some(10));
    c.shutdown = true;
    let h = worker::snapshot::fixture(memory, disk, None, 7);
    let mut image = vec![];
    h.json_write(&mut image).unwrap();
    image.resize(image.len() + PAGE as usize + memory as usize, 0x4a);
    if disk {
        let mut store = Store::zeroed(4).unwrap();
        store.write(1, &[3; 512]).unwrap();
        store.flush();
        store.write(1, &[0; 512]).unwrap();
        image.extend(store.encode_overlays());
    }
    let mut bytes = vec![];
    json_write(&c, &mut bytes).unwrap();
    for b in [&kernel()[..], b"exact-initramfs", &image] {
        bytes.extend_from_slice(&(b.len() as u64).to_le_bytes());
        bytes.extend_from_slice(b);
    }
    if disk {
        bytes.extend_from_slice(&0u64.to_le_bytes());
    }
    Snapshot {
        bytes: std::sync::Arc::new(bytes),
        aux: retained::Aux {
            active: Some(0),
            ..Default::default()
        },
        disk: base,
    }
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
#[test]
fn synthetic_live_native_diskless_and_both_owned_layers_roundtrip_without_restore() {
    for disk in [false, true] {
        let s = live(disk);
        closure(&s);
        let d = decode(&encode(&s)).unwrap();
        assert_eq!(d.bytes.len(), d.bytes.capacity());
        let p = Parts::new(&d).unwrap();
        assert_eq!(p.ram.len(), (2 << 20) + PAGE as usize);
        assert!(p.ram.iter().all(|b| *b == 0x4a));
        assert!(p.client.is_empty());
    }
}
struct BeforeBodies {
    input: io::Cursor<Vec<u8>>,
}
impl Read for BeforeBodies {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        assert!(
            (self.input.position() as usize) < self.input.get_ref().len(),
            "raw body was read"
        );
        self.input.read(b)
    }
}
#[test]
fn foreign_format_and_oversized_counts_are_rejected_before_body_read() {
    let mut m = machine(2 << 20, None);
    let s = m.capture().unwrap();
    let b = encode(&s);
    for at in [12usize, 13] {
        let mut h = b[..24].to_vec();
        h[at] = 255;
        let mut r = BeforeBodies {
            input: io::Cursor::new(h),
        };
        assert!(Snapshot::read_from(&mut r, &limits()).is_err());
    }
    // Kernel argument count follows memory+layout. It cannot fit the remaining
    // wire even with permissive quotas and must not request argument bytes.
    let mut h = b[..44].to_vec();
    h[36..44].copy_from_slice(&u64::MAX.to_le_bytes());
    let mut r = BeforeBodies {
        input: io::Cursor::new(h),
    };
    assert!(Snapshot::read_from(&mut r, &limits()).is_err());
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
#[test]
fn large_native_capacity_is_budget_rejected_without_reading_or_allocating_ram() {
    let mut m = machine(2048 << 20, None);
    let mut c = metadata(&m.capture().unwrap());
    c.pending_boots = 0;
    c.live = true;
    c.seed = 1;
    let h = worker::snapshot::fixture(2048 << 20, false, None, 0);
    let ram = PAGE + (2048 << 20);
    let b = unchecked(
        &c,
        Some(&h),
        None,
        [kernel().len() as u64, INITRD_LEN, ram, 0, 0, 0],
        &[],
        kernel().len() as u64 + INITRD_LEN + ram,
    );
    let mut reader = BeforeBodies {
        input: io::Cursor::new(b),
    };
    let mut l = limits();
    l.encoded_bytes = 3 << 30;
    l.blob_bytes = 3 << 30;
    assert!(matches!(
        Snapshot::read_from(&mut reader, &l),
        Err(CodecError::Limit("allocation bytes"))
    ));
}
#[test]
fn disk_tag_length_layer_and_native_ownership_errors() {
    let mut m = machine(2 << 20, Some(DiskImage::zeroed(512).unwrap()));
    let s = m.capture().unwrap();
    let c = metadata(&s);
    let k = kernel();
    for (lengths, raw, base) in [
        (
            [kernel().len() as u64, INITRD_LEN, 0, 0, 0, 0],
            vec![&k[..], b"exact-initramfs"],
            DiskImage::zeroed(512).unwrap(),
        ),
        (
            [kernel().len() as u64, INITRD_LEN, 0, 0, 1, 0],
            vec![&k[..], b"exact-initramfs", b"x"],
            DiskImage::zeroed(512).unwrap(),
        ),
        (
            [kernel().len() as u64, INITRD_LEN, 0, 0, 0, 0],
            vec![&k[..], b"exact-initramfs"],
            DiskImage::from_bytes(vec![0; 512]).unwrap(),
        ),
    ] {
        let b = unchecked(&c, None, Some(&base), lengths, &raw, 0);
        assert!(decode(&b).is_err());
    }
}

#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
#[test]
fn live_raw_streams_borrow_capture_and_decode_directly_into_final_owned_body() {
    struct BorrowWitness {
        source: Range<usize>,
        large: usize,
    }
    impl Write for BorrowWitness {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            assert!(b.len() <= 65536);
            if b.len() == 65536 {
                let start = b.as_ptr() as usize;
                assert!(start >= self.source.start && start + b.len() <= self.source.end);
                self.large += b.len();
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let s = live(false);
    let start = s.bytes.as_ptr() as usize;
    let mut witness = BorrowWitness {
        source: start..start + s.bytes.len(),
        large: 0,
    };
    s.write_to(&mut witness).unwrap();
    assert!(witness.large >= 2 << 20);
    struct OwnedWitness {
        input: io::Cursor<Vec<u8>>,
        large: Vec<Range<usize>>,
    }
    impl Read for OwnedWitness {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            if b.len() == 65536 {
                let start = b.as_ptr() as usize;
                self.large.push(start..start + b.len());
            }
            self.input.read(b)
        }
    }
    let wire = encode(&s);
    drop(s);
    let mut witness = OwnedWitness {
        input: io::Cursor::new(wire),
        large: vec![],
    };
    let restored = Snapshot::read_from(&mut witness, &limits()).unwrap();
    let start = restored.bytes.as_ptr() as usize;
    let end = start + restored.bytes.len();
    assert!(!witness.large.is_empty());
    for range in witness.large {
        assert!(
            range.start >= start && range.end <= end,
            "RAM was staged outside returned body"
        );
    }
}

#[test]
fn overlay_geometry_and_base_ownership_copy_are_preflighted_before_raw_body_reads() {
    let mut m = machine(2 << 20, Some(DiskImage::zeroed(512).unwrap()));
    let c = metadata(&m.capture().unwrap());
    let bad_len = 33 + 2 * 520;
    let wire = unchecked(
        &c,
        None,
        Some(&DiskImage::zeroed(512).unwrap()),
        [kernel().len() as u64, INITRD_LEN, 0, 0, bad_len, 0],
        &[],
        kernel().len() as u64 + INITRD_LEN + bad_len,
    );
    let mut reader = BeforeBodies {
        input: io::Cursor::new(wire),
    };
    assert!(matches!(
        Snapshot::read_from(&mut reader, &limits()),
        Err(CodecError::Invalid("overlay length exceeds disk geometry"))
    ));
    let disk = DiskImage::from_bytes(vec![0; 1 << 20]).unwrap();
    let mut m = machine(2 << 20, Some(disk.clone()));
    let c = metadata(&m.capture().unwrap());
    let wire = unchecked(
        &c,
        None,
        Some(&disk),
        [kernel().len() as u64, INITRD_LEN, 0, 0, 32, 1 << 20],
        &[],
        kernel().len() as u64 + INITRD_LEN + 32 + (1 << 20),
    );
    let mut reader = BeforeBodies {
        input: io::Cursor::new(wire),
    };
    let mut l = limits();
    l.allocation_bytes = (1 << 20) + (1 << 19);
    assert!(matches!(
        Snapshot::read_from(&mut reader, &l),
        Err(CodecError::Limit("allocation bytes"))
    ));
}
#[test]
fn nested_empty_collections_and_native_identity_indexes_have_strict_item_limits() {
    let mut l = limits();
    l.total_items = 999;
    l.collection_items = 1000;
    let mut reader = BeforeBodies {
        input: io::Cursor::new(1000u64.to_le_bytes().to_vec()),
    };
    let mut r = Reader::new(&mut reader, 8008, 0, &l).unwrap();
    assert!(matches!(
        Vec::<Vec<u8>>::decode(&mut r),
        Err(CodecError::Limit("aggregate items"))
    ));
    l = limits();
    l.allocation_bytes = 1000 * std::mem::size_of::<Vec<u8>>() as u64 - 1;
    let mut reader = BeforeBodies {
        input: io::Cursor::new(1000u64.to_le_bytes().to_vec()),
    };
    let mut r = Reader::new(&mut reader, 8008, 0, &l).unwrap();
    assert!(matches!(
        Vec::<Vec<u8>>::decode(&mut r),
        Err(CodecError::Limit("allocation bytes"))
    ));
    // The shared internal scalar-index parser is pure on either host. This
    // exercises strict decoding used by ARM GIC maps/sets, not native ARM restore.
    let mut b = vec![];
    let mut w = Writer::new(&mut b);
    w.u64(2).unwrap();
    w.u16(7).unwrap();
    w.u64(1).unwrap();
    w.u16(7).unwrap();
    w.u64(2).unwrap();
    let mut input = &b[..];
    let mut r = Reader::new(&mut input, b.len() as u64, 0, &limits()).unwrap();
    assert!(std::collections::BTreeMap::<u16, u64>::decode(&mut r).is_err());
    let mut b = vec![];
    let mut w = Writer::new(&mut b);
    w.u64(2).unwrap();
    w.u32(7).unwrap();
    w.u32(7).unwrap();
    let mut input = &b[..];
    let mut r = Reader::new(&mut input, b.len() as u64, 0, &limits()).unwrap();
    assert!(std::collections::BTreeSet::<u32>::decode(&mut r).is_err());
}

#[test]
fn nonlive_completed_boot_must_be_exited_or_crashed_and_valid_lifecycles_continue() {
    let mut m = machine(2 << 20, None);
    let unstarted = m.capture().unwrap();
    closure(&unstarted);
    for state in [State::Ready, State::Blocked(None), State::Blocked(Some(8))] {
        let mut c = metadata(&unstarted);
        c.pending_boots = 0;
        c.seed = 1;
        c.reached = 7;
        c.state = state;
        let k = kernel();
        let wire = unchecked(
            &c,
            None,
            None,
            [k.len() as u64, INITRD_LEN, 0, 0, 0, 0],
            &[&k, b"exact-initramfs"],
            0,
        );
        assert!(matches!(
            decode(&wire),
            Err(CodecError::Invalid("machine state or blocked deadline"))
        ));
    }
    for state in [State::Exited, State::Crashed] {
        m.pending_boots = 0;
        m.seed = 55;
        m.reached = 7;
        m.state = state;
        m.shutdown = true;
        closure(&m.capture().unwrap());
    }
}
#[test]
fn production_outer_mac_invariant_rejects_zero_and_multicast_but_not_missing_ip_arguments() {
    let mut m = machine(2 << 20, None);
    let s = m.capture().unwrap();
    let k = kernel();
    for mac in [[0; 6], [1, 0, 0, 0, 0, 1], [255; 6]] {
        let mut c = metadata(&s);
        c.network = Some(mac);
        let wire = unchecked(
            &c,
            None,
            None,
            [k.len() as u64, INITRD_LEN, 0, 0, 0, 0],
            &[&k, b"exact-initramfs"],
            0,
        );
        assert!(matches!(
            decode(&wire),
            Err(CodecError::Invalid("network MAC"))
        ));
    }
    m.network = Some([2, 0, 0, 0, 0, 1]);
    assert!(!m.command_line.contains("seedbox.ipv4="));
    closure(&m.capture().unwrap());
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
#[test]
fn public_live_codec_accepts_actual_mmio_backlog_without_driver_ok() {
    let source = live(false);
    let p = Parts::new(&source).unwrap();
    let mut c = metadata(&source);
    c.network = Some([2, 0, 0, 0, 0, 1]);
    let header = worker::snapshot::backlog_without_driver_ok_fixture(c.memory_bytes, c.reached);
    let mut image = vec![];
    header.json_write(&mut image).unwrap();
    image.extend_from_slice(p.ram);
    let mut bytes = vec![];
    json_write(&c, &mut bytes).unwrap();
    for body in [p.kernel, p.initrd, &image] {
        bytes.extend_from_slice(&(body.len() as u64).to_le_bytes());
        bytes.extend_from_slice(body);
    }
    let s = Snapshot {
        bytes: std::sync::Arc::new(bytes),
        aux: retained::Aux {
            active: Some(0),
            ..Default::default()
        },
        disk: None,
    };
    drop(source);
    closure(&s);
}
#[cfg(any(
    all(target_arch = "x86_64", target_os = "linux"),
    all(target_arch = "aarch64", target_os = "macos")
))]
#[test]
fn invalid_boot_inputs_are_rejected_before_any_ram_body_read() {
    let source = live(false);
    let c = metadata(&source);
    let header = worker::snapshot::fixture(c.memory_bytes, false, None, c.reached);
    let mut k = kernel();
    #[cfg(target_arch = "x86_64")]
    {
        k[0x202] = 0;
    }
    #[cfg(target_arch = "aarch64")]
    {
        k[56] = 0;
    }
    let ram = PAGE + c.memory_bytes;
    let wire = unchecked(
        &c,
        Some(&header),
        None,
        [k.len() as u64, INITRD_LEN, ram, 0, 0, 0],
        &[&k, b"exact-initramfs"],
        ram,
    );
    let mut reader = BeforeBodies {
        input: io::Cursor::new(wire),
    };
    let result = Snapshot::read_from(&mut reader, &limits());
    assert!(
        matches!(&result, Err(CodecError::Invalid("kernel/initrd layout"))),
        "{:?}",
        result.err()
    );
}
#[test]
fn private_metadata_count_stops_at_cap_with_limit_not_generic_serialization_error() {
    use serde::ser::SerializeSeq;
    struct Many<'a>(&'a std::cell::Cell<usize>);
    impl Serialize for Many<'_> {
        fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
            let mut sequence = s.serialize_seq(Some(1_000_000))?;
            for _ in 0..1_000_000 {
                self.0.set(self.0.get() + 1);
                sequence.serialize_element(&u64::MAX)?;
            }
            sequence.end()
        }
    }
    let visited = std::cell::Cell::new(0);
    assert!(matches!(
        json_len(&Many(&visited)),
        Err(CodecError::Limit("private metadata bytes"))
    ));
    assert!(
        visited.get() > 0 && visited.get() < 1_000_000,
        "counting must stop serialization, not merely classify it afterward"
    );
    struct Broken;
    impl Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _: S) -> std::result::Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("synthetic serializer failure"))
        }
    }
    assert!(matches!(
        json_len(&Broken),
        Err(CodecError::Invalid("private metadata serialization"))
    ));
    let mut count = Count {
        bytes: protocol::MAX_MESSAGE as u64,
        exceeded: false,
    };
    assert_eq!(count.write(&[]).unwrap(), 0);
    assert!(count.write(&[0]).is_err());
    assert!(count.exceeded);
    assert_eq!(count.bytes, protocol::MAX_MESSAGE as u64);
}
#[test]
fn writer_short_writes_and_write_zero_keep_ordinary_io_contract_after_counting_change() {
    struct Short(Vec<u8>);
    impl Write for Short {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            let n = b.len().min(7);
            self.0.extend_from_slice(&b[..n]);
            Ok(n)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    struct Zero;
    impl Write for Zero {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Ok(0)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut m = machine(2 << 20, None);
    let s = m.capture().unwrap();
    let expected = encode(&s);
    let mut short = Short(vec![]);
    s.write_to(&mut short).unwrap();
    assert_eq!(short.0, expected);
    assert!(
        matches!(s.write_to(&mut Zero),Err(CodecError::Io(e)) if e.kind()==io::ErrorKind::WriteZero)
    );
    assert_eq!(encode(&s), expected);
}
