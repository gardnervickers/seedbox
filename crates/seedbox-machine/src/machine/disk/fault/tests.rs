use super::*;
use crate::machine::disk::request::{STATUS_IOERR, STATUS_OK};

/// Contiguous gathered streams; the writable stream starts filled with 0xcc.
struct Gathered {
    readable: Vec<u8>,
    writable: Vec<u8>,
}
impl Buffers for Gathered {
    fn readable(&self) -> usize {
        self.readable.len()
    }
    fn writable(&self) -> usize {
        self.writable.len()
    }
    fn read(&mut self, offset: usize, out: &mut [u8]) {
        out.copy_from_slice(&self.readable[offset..offset + out.len()]);
    }
    fn write(&mut self, offset: usize, bytes: &[u8]) {
        self.writable[offset..offset + bytes.len()].copy_from_slice(bytes);
    }
}
fn header(kind: u32, sector: u64) -> Vec<u8> {
    let mut bytes = kind.to_le_bytes().to_vec();
    bytes.extend([0; 4]);
    bytes.extend(sector.to_le_bytes());
    bytes
}
fn read(sector: u64, sectors: usize) -> Gathered {
    Gathered {
        readable: header(TYPE_IN, sector),
        writable: vec![0xcc; sectors * SECTOR + 1],
    }
}
fn write(sector: u64, data: &[u8]) -> Gathered {
    let mut readable = header(TYPE_OUT, sector);
    readable.extend_from_slice(data);
    Gathered {
        readable,
        writable: vec![0xcc],
    }
}
fn flush() -> Gathered {
    Gathered {
        readable: header(TYPE_FLUSH, 0),
        writable: vec![0xcc],
    }
}
fn classify(buffers: &mut Gathered) -> Request {
    let mut head = [0u8; 16];
    head.copy_from_slice(&buffers.readable[..16]);
    Request::decode(&head, buffers.readable(), buffers.writable())
}
fn run(faults: &mut DiskFaults, store: &mut Store, buffers: &mut Gathered) -> Completion {
    let request = classify(buffers);
    faults.execute(store, buffers, request).unwrap()
}
fn error(op: DiskOp, ppm: u32, effect: DiskEffect, scope: DiskScope) -> DeviceFault {
    DeviceFault::DiskError {
        op,
        ppm,
        effect,
        scope,
    }
}
fn stored(store: &Store, sector: u64) -> Vec<u8> {
    let mut bytes = vec![0; SECTOR];
    store.read(sector, &mut bytes).unwrap();
    bytes
}

#[test]
fn requests_classify_by_header_and_streams() {
    assert_eq!(
        classify(&mut read(7, 2)),
        Request {
            kind: Kind::Read,
            sector: 7,
            sectors: 2
        }
    );
    assert_eq!(classify(&mut write(3, &[1; 1024])).sectors, 2);
    assert_eq!(classify(&mut flush()).kind, Kind::Flush);
    let mut id = read(0, 0);
    id.readable[..4].copy_from_slice(&8u32.to_le_bytes());
    assert_eq!(classify(&mut id).kind, Kind::Other);
    let short = Request::decode(&[0; 16], 8, 1);
    assert_eq!(short.kind, Kind::Other);
}

#[test]
fn eio_fails_selected_requests_before_any_effect_and_counts_them() {
    let mut store = Store::zeroed(64).unwrap();
    store.write(4, &[0x55; SECTOR]).unwrap();
    let before = store.clone();
    let mut faults = DiskFaults::new(1);
    faults
        .install(
            7,
            99,
            error(DiskOp::Any, 1_000_000, DiskEffect::Eio, DiskScope::All),
        )
        .unwrap();
    let mut buffers = write(4, &[0xaa; SECTOR]);
    let completion = run(&mut faults, &mut store, &mut buffers);
    assert_eq!((completion.status, completion.written), (STATUS_IOERR, 1));
    assert_eq!(store.encode_overlays(), before.encode_overlays());
    let mut buffers = read(4, 1);
    let completion = run(&mut faults, &mut store, &mut buffers);
    assert_eq!(completion.status, STATUS_IOERR);
    // No data was returned: only the status byte changed.
    assert!(buffers.writable[..SECTOR].iter().all(|&b| b == 0xcc));
    let completion = run(&mut faults, &mut store, &mut flush());
    assert_eq!(completion.status, STATUS_IOERR);
    assert_eq!(
        faults.heal(7),
        Some(FaultCounts {
            matched: 3,
            affected: 3
        })
    );
    assert_eq!(faults.heal(7), None);
    let completion = run(&mut faults, &mut store, &mut read(4, 1));
    assert_eq!(completion.status, STATUS_OK);
}

#[test]
fn selection_matches_op_and_draws_one_seeded_stream_per_boot() {
    let draws = |seed: u64, boot: u64| {
        let mut store = Store::zeroed(16).unwrap();
        let mut faults = DiskFaults::new(boot);
        faults
            .install(
                1,
                seed,
                error(DiskOp::Write, 300_000, DiskEffect::Eio, DiskScope::All),
            )
            .unwrap();
        let mut statuses = Vec::new();
        for _ in 0..64 {
            // Reads never match a write fault, so they draw nothing.
            assert_eq!(
                run(&mut faults, &mut store, &mut read(0, 1)).status,
                STATUS_OK
            );
            statuses.push(run(&mut faults, &mut store, &mut write(0, &[1; SECTOR])).status);
        }
        (statuses, faults.installed()[0].counts)
    };
    let (first, counts) = draws(5, 0);
    assert_eq!(draws(5, 0).0, first);
    assert_ne!(draws(5, 1).0, first, "a new boot draws a fresh stream");
    assert_ne!(draws(6, 0).0, first);
    assert_eq!(counts.matched, 64);
    let failed = first.iter().filter(|&&s| s == STATUS_IOERR).count() as u64;
    assert_eq!(counts.affected, failed);
    assert!((5..40).contains(&failed), "{failed} of 64 at 30%");
}

#[test]
fn corrupt_replaces_one_seeded_sector_of_read_data_or_stored_writes() {
    let mut store = Store::zeroed(64).unwrap();
    for sector in 0..4 {
        store.write(sector, &[sector as u8 + 1; SECTOR]).unwrap();
    }
    let before = store.clone();
    let mut faults = DiskFaults::new(0);
    faults
        .install(
            3,
            11,
            error(DiskOp::Read, 1_000_000, DiskEffect::Corrupt, DiskScope::All),
        )
        .unwrap();
    let mut buffers = read(0, 4);
    assert_eq!(run(&mut faults, &mut store, &mut buffers).status, STATUS_OK);
    let changed: Vec<_> = (0..4)
        .filter(|&s| buffers.writable[s * SECTOR..(s + 1) * SECTOR] != [s as u8 + 1; SECTOR])
        .collect();
    assert_eq!(changed.len(), 1, "exactly one sector replaced");
    assert_eq!(
        store.encode_overlays(),
        before.encode_overlays(),
        "the disk keeps its data"
    );
    assert_eq!(faults.heal(3).unwrap().affected, 1);

    faults
        .install(
            4,
            11,
            error(
                DiskOp::Write,
                1_000_000,
                DiskEffect::Corrupt,
                DiskScope::All,
            ),
        )
        .unwrap();
    let data = [0x42; 2 * SECTOR];
    assert_eq!(
        run(&mut faults, &mut store, &mut write(10, &data)).status,
        STATUS_OK
    );
    let kept: Vec<_> = (10..12)
        .map(|s| stored(&store, s) == [0x42; SECTOR])
        .collect();
    assert_eq!(
        kept.iter().filter(|k| !**k).count(),
        1,
        "one stored sector replaced"
    );
    // An invalid request completes as before and is not affected.
    let mut past = write(63, &data);
    assert_eq!(run(&mut faults, &mut store, &mut past).status, STATUS_IOERR);
    assert_eq!(
        faults.heal(4),
        Some(FaultCounts {
            matched: 2,
            affected: 1
        })
    );
}

#[test]
fn data_scope_spares_static_ext4_metadata_and_unformatted_disks() {
    let mut store = Store::zeroed(65536 * 8).unwrap();
    let mut faults = DiskFaults::new(0);
    faults
        .install(
            1,
            2,
            error(DiskOp::Any, 1_000_000, DiskEffect::Eio, DiskScope::Data),
        )
        .unwrap();
    // Before formatting nothing is data.
    assert_eq!(
        run(&mut faults, &mut store, &mut read(50_000 * 8, 1)).status,
        STATUS_OK
    );
    store
        .write(0, &crate::machine::disk::ext4::fixture::image())
        .unwrap();
    // The superblock, an inode table and the journal are spared.
    for sector in [2, 9 * 8, 40_100 * 8] {
        assert_eq!(
            run(&mut faults, &mut store, &mut read(sector, 1)).status,
            STATUS_OK
        );
    }
    // A request straddling metadata is spared too.
    assert_eq!(
        run(&mut faults, &mut store, &mut read(41 * 8 - 1, 2)).status,
        STATUS_OK
    );
    assert_eq!(
        run(
            &mut faults,
            &mut store,
            &mut write(50_000 * 8, &[1; SECTOR])
        )
        .status,
        STATUS_IOERR
    );
    assert_eq!(run(&mut faults, &mut store, &mut flush()).status, STATUS_OK);
    assert_eq!(
        faults.heal(1),
        Some(FaultCounts {
            matched: 1,
            affected: 1
        })
    );
}

#[test]
fn latency_delays_matching_requests_by_a_bounded_seeded_draw() {
    let mut faults = DiskFaults::new(2);
    faults
        .install(
            9,
            1,
            DeviceFault::DiskLatency {
                op: DiskOp::Flush,
                latency_ns: 1_000,
                jitter_ns: 500,
            },
        )
        .unwrap();
    let mut delays = Vec::new();
    for _ in 0..32 {
        assert_eq!(faults.arrive(classify(&mut read(0, 1)), 0), Arrival::Now);
        match faults.arrive(classify(&mut flush()), 0) {
            Arrival::Delay(delay) => delays.push(delay),
            other => panic!("{other:?}"),
        }
    }
    assert!(delays.iter().all(|d| (1_000..=1_500).contains(d)));
    assert!(delays.iter().any(|&d| d != delays[0]), "jitter draws vary");
    assert_eq!(
        faults.heal(9),
        Some(FaultCounts {
            matched: 32,
            affected: 32
        })
    );
}

#[test]
fn holds_count_matching_arrivals_until_their_nth_or_deadline() {
    let mut faults = DiskFaults::new(0);
    faults
        .hold(Hold {
            trigger: 4,
            op: DiskOp::Write,
            nth: 3,
            seen: 1,
            until: Some(100),
        })
        .unwrap();
    assert!(faults
        .hold(Hold {
            trigger: 4,
            op: DiskOp::Read,
            nth: 1,
            seen: 0,
            until: None
        })
        .is_err());
    let w = classify(&mut write(0, &[0; SECTOR]));
    assert_eq!(faults.arrive(classify(&mut read(0, 1)), 0), Arrival::Now);
    assert_eq!(faults.arrive(w, 10), Arrival::Now);
    assert_eq!(faults.holds()[0].seen, 2);
    // At the deadline nothing is counted.
    assert_eq!(faults.arrive(w, 100), Arrival::Now);
    assert_eq!(faults.holds()[0].seen, 2);
    assert_eq!(faults.arrive(w, 99), Arrival::Held(4));
    assert!(faults.holds().is_empty());
    faults
        .hold(Hold {
            trigger: 5,
            op: DiskOp::Any,
            nth: 2,
            seen: 0,
            until: None,
        })
        .unwrap();
    assert_eq!(faults.release(5).map(|h| h.seen), Some(0));
    assert_eq!(faults.arrive(w, 0), Arrival::Now);
}

#[test]
fn closed_records_round_trip_and_restored_state_is_checked() {
    use crate::snapshot::Record;
    let mut faults = DiskFaults::new(3);
    faults
        .install(
            1,
            5,
            error(DiskOp::Any, 7, DiskEffect::Corrupt, DiskScope::Data),
        )
        .unwrap();
    faults
        .install(
            2,
            6,
            DeviceFault::DiskLatency {
                op: DiskOp::Read,
                latency_ns: 10_000,
                jitter_ns: 0,
            },
        )
        .unwrap();
    faults
        .hold(Hold {
            trigger: 1,
            op: DiskOp::Flush,
            nth: 2,
            seen: 1,
            until: None,
        })
        .unwrap();
    let mut store = Store::zeroed(8).unwrap();
    run(&mut faults, &mut store, &mut read(0, 1));
    let mut bytes = Vec::new();
    faults
        .encode(&mut crate::snapshot::Writer::new(&mut bytes))
        .unwrap();
    let limits = crate::snapshot::DecodeLimits {
        encoded_bytes: 1 << 20,
        allocation_bytes: 1 << 20,
        collection_items: 64,
        total_items: 64,
        string_bytes: 64,
        blob_bytes: 1 << 20,
    };
    let mut input = &bytes[..];
    let mut reader =
        crate::snapshot::Reader::new(&mut input, bytes.len() as u64, 0, &limits).unwrap();
    let decoded = DiskFaults::decode(&mut reader).unwrap();
    assert_eq!(decoded, faults);
    decoded.validate_snapshot().unwrap();
    let json: DiskFaults = serde_json::from_value(serde_json::to_value(&faults).unwrap()).unwrap();
    assert_eq!(json, faults);
    let mut moved = faults.clone();
    moved.boot = 4;
    assert!(
        moved.validate_snapshot().is_err(),
        "streams belong to their boot"
    );
    assert!(faults
        .install(2, 1, DeviceFault::Preempt { ppm: 1 })
        .is_err());
}
