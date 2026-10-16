//! Public overlay validation, diagnostic parity and current-thread allocations.
use seedbox_machine::machine::disk::{
    codec::{validate_overlays, CodecError},
    Store, SECTOR,
};
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    hint::black_box,
};

// This integration target alone uses the counting allocator. No shared test
// lock: only the calling test thread is counted, and const TLS needs no heap.
struct ThreadCountingAllocator;
thread_local! { static COUNTS: Cell<Option<[usize; 4]>> = const { Cell::new(None) }; }
fn record(which: usize) {
    let _ = COUNTS.try_with(|slot| {
        if let Some(mut counts) = slot.get() {
            counts[which] += 1;
            slot.set(Some(counts));
        }
    });
}
// SAFETY: forwards every operation and its original arguments to System;
// accounting touches only non-allocating, current-thread Cell storage.
unsafe impl GlobalAlloc for ThreadCountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(0);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(1);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(2);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record(3);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: ThreadCountingAllocator = ThreadCountingAllocator;

fn counted<T>(f: impl FnOnce() -> T) -> (T, [usize; 4]) {
    COUNTS.with(|slot| {
        assert!(slot.get().is_none());
        slot.set(Some([0; 4]));
    });
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            COUNTS.with(|slot| slot.set(None));
        }
    }
    let reset = Reset;
    let value = f();
    let counts = COUNTS.with(|slot| slot.replace(None).unwrap());
    drop(reset);
    (value, counts)
}
const ENTRY: usize = 8 + SECTOR;
fn wire(capacity: u64, durable: &[u64], unflushed: &[u64]) -> Vec<u8> {
    let mut bytes = b"SBXSTOR1".to_vec();
    bytes.extend(capacity.to_le_bytes());
    for layer in [durable, unflushed] {
        bytes.extend((layer.len() as u64).to_le_bytes());
        for sector in layer {
            bytes.extend(sector.to_le_bytes());
            bytes.extend([5; SECTOR]);
        }
    }
    bytes
}
fn fixture() -> Store {
    let mut store = Store::from_image(vec![9; 4 * SECTOR]).unwrap();
    store.write(0, &[1; SECTOR]).unwrap();
    store.write(2, &[0; SECTOR]).unwrap();
    store.flush();
    store.write(0, &[0; SECTOR]).unwrap();
    store.write(3, &[7; SECTOR]).unwrap();
    store
}
fn check(bytes: &[u8], capacity: u64, expected: Option<Result<(), CodecError>>) {
    // All construction, snapshots and assertions stay outside the count scope.
    let mut destination = if capacity == 4 {
        fixture()
    } else {
        Store::zeroed(capacity).unwrap()
    };
    let before = destination.encode_overlays();
    let (validated, counts) =
        counted(|| black_box(validate_overlays(black_box(bytes), black_box(capacity))));
    assert_eq!(counts, [0; 4], "validator allocated or deallocated");
    let replaced = destination.replace_overlays(bytes);
    assert_eq!(validated, replaced, "error precedence differs");
    if let Some(expected) = expected {
        assert_eq!(validated, expected);
    }
    if replaced.is_err() {
        assert_eq!(
            destination.encode_overlays(),
            before,
            "failed replacement mutated overlays"
        );
        if capacity == 4 {
            let mut data = [0; SECTOR];
            destination.read(1, &mut data).unwrap();
            assert_eq!(data, [9; SECTOR]);
        }
    } else {
        assert_eq!(destination.encode_overlays(), bytes);
    }
}

#[test]
fn malformed_matrix_and_every_prefix_have_exact_error_parity_without_allocations() {
    use CodecError::*;
    let valid = fixture().encode_overlays();
    check(&valid, 4, Some(Ok(())));
    let mut magic = valid.clone();
    magic[..8].copy_from_slice(b"SBXSTOR2");
    let mut trailing = valid.clone();
    trailing.push(0);
    let mut cases = vec![
        (magic, Magic),
        (wire(5, &[], &[]), Capacity),
        (trailing, Trailing),
        (wire(4, &[1, 1], &[]), Order),
        (wire(4, &[], &[2, 1]), Order),
        (wire(4, &[4], &[]), Range),
        (wire(4, &[u64::MAX], &[]), Range),
        (wire(4, &[0, 1, 2, 3, 3], &[]), Count),
        // Competing faults: count before entry range/order; range before order;
        // a first-layer error before a later-layer error or trailing bytes.
        (wire(4, &[4, 4], &[1, 1]), Range),
        (wire(4, &[3, 3, 4], &[]), Order),
        (wire(4, &[], &[u64::MAX]), Range),
    ];
    for count in [2, u64::MAX] {
        let mut bytes = wire(4, &[0], &[]);
        bytes[16..24].copy_from_slice(&count.to_le_bytes());
        cases.push((bytes, Count));
        let mut bytes = wire(4, &[], &[0]);
        bytes[24..32].copy_from_slice(&count.to_le_bytes());
        cases.push((bytes, Count));
    }
    let mut mismatched = wire(5, &[u64::MAX], &[]);
    mismatched.push(0);
    cases.push((mismatched, Capacity));
    for (bytes, error) in cases {
        check(&bytes, 4, Some(Err(error)));
    }
    // The frozen codec checks count against remaining/520 before reading data.
    let durable_end = 24 + 2 * ENTRY;
    for end in 0..valid.len() {
        let error = if (24..durable_end).contains(&end) || end >= durable_end + 8 {
            Count
        } else {
            Truncated
        };
        check(&valid[..end], 4, Some(Err(error)));
    }
    for extra in [vec![0], vec![0; ENTRY], vec![255; 17]] {
        let mut bytes = valid.clone();
        bytes.extend(extra);
        check(&bytes, 4, Some(Err(Trailing)));
    }
}

#[test]
fn deterministic_mutations_and_layer_corpus_have_exact_parity_without_allocations() {
    let valid = fixture().encode_overlays();
    for offset in 0..valid.len() {
        let mut bytes = valid.clone();
        bytes[offset] ^= 0x81;
        check(&bytes, 4, None);
    }
    let mut state = 0x71be_2035_969a_bcd1u64;
    for _ in 0..512 {
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            state
        };
        let durable: Vec<_> = (0..next() % 6).map(|_| next() % 7).collect();
        let unflushed: Vec<_> = (0..next() % 6).map(|_| next() % 7).collect();
        let bytes = wire(4, &durable, &unflushed);
        check(&bytes, 4, None);
    }
}

#[test]
fn shared_sectors_zero_data_base_ownership_and_huge_capacities() {
    let bytes = fixture().encode_overlays();
    let (result, counts) = counted(|| validate_overlays(black_box(&bytes), 4));
    assert_eq!(result, Ok(()));
    assert_eq!(counts, [0; 4]);
    let mut restored = Store::from_image(vec![8; 4 * SECTOR]).unwrap();
    let (result, counts) = counted(|| restored.replace_overlays(black_box(&bytes)));
    assert_eq!(result, Ok(()));
    assert!(
        counts[0] + counts[1] + counts[2] > 0,
        "positive control must observe restore allocations"
    );
    let mut data = [255; SECTOR];
    restored.read(0, &mut data).unwrap();
    assert_eq!(data, [0; SECTOR]);
    restored.read(1, &mut data).unwrap();
    assert_eq!(data, [8; SECTOR], "encoding does not replace the base");
    restored.read(2, &mut data).unwrap();
    assert_eq!(data, [0; SECTOR], "zero data masks nonzero base");
    restored.discard_unflushed();
    restored.read(0, &mut data).unwrap();
    assert_eq!(data, [1; SECTOR], "shared sector retains its durable value");
    for capacity in [1, u64::MAX / SECTOR as u64 + 1, u64::MAX] {
        let empty = Store::zeroed(capacity).unwrap().encode_overlays();
        assert_eq!(empty.len(), 32);
        check(&empty, capacity, Some(Ok(())));
        for end in 0..empty.len() {
            check(&empty[..end], capacity, Some(Err(CodecError::Truncated)));
        }
        check(
            &empty,
            if capacity == 1 { 2 } else { 1 },
            Some(Err(CodecError::Capacity)),
        );
    }
    let mut huge = Store::zeroed(u64::MAX).unwrap();
    huge.write(u64::MAX - 1, &[0; SECTOR]).unwrap();
    check(&huge.encode_overlays(), u64::MAX, Some(Ok(())));
    check(
        &wire(u64::MAX, &[u64::MAX], &[]),
        u64::MAX,
        Some(Err(CodecError::Range)),
    );
    // Capacity validity belongs to the caller/base constructor, not the scan.
    assert!(Store::zeroed(0).is_err());
    let zero_capacity = wire(0, &[], &[]);
    let (result, counts) = counted(|| validate_overlays(black_box(&zero_capacity), 0));
    assert_eq!(result, Ok(()));
    assert_eq!(counts, [0; 4]);
}
