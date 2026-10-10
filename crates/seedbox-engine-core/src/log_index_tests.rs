use super as producer;
/// Index records as `[time, incarnation, order, offset, length]` rows.
fn rows(index: &producer::Index) -> Vec<[u64; 5]> {
    index
        .records
        .iter()
        .map(|r| [r.time_ns, r.incarnation, r.order, r.offset, r.length])
        .collect()
}

fn chunk_index(records: &[[u64; 5]]) -> Vec<u8> {
    let mut bytes = producer::MAGIC.to_vec();
    for row in records {
        for value in row {
            bytes.extend(value.to_le_bytes());
        }
    }
    bytes
}
#[test]
fn actual_producer_is_bounded_streams_complete_prefix_and_forks_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("index");
    let mut index = producer::Index::default();
    index.limit = Some(2);
    // Public API uses Default because the failure latch is private.
    index.open(path.to_str().unwrap());
    let mut order = 0;
    index.capture(2, 10, 0, &mut order);
    index.capture(2, 10, 0, &mut order);
    index.finish();
    let snapshot = index.snapshot();
    index.capture(4, 10, 0, &mut order);
    index.capture(9, 20, 1, &mut order);
    index.finish();
    assert_eq!(index.records.len(), 2);
    assert_eq!(index.seen, 9);
    assert_eq!(order, 3);
    assert_eq!(
        std::fs::read(path).unwrap(),
        chunk_index(&[[10, 0, 0, 0, 2], [10, 0, 1, 2, 2]])
    );
    assert_eq!(snapshot.records.len(), 1);
    assert_eq!(snapshot.seen, 2);
    let mut restored = snapshot;
    restored.open(tmp.path().join("fork").to_str().unwrap());
    let mut restored_order = 1;
    restored.capture(3, 11, 0, &mut restored_order);
    restored.finish();
    assert_eq!(
        std::fs::read(tmp.path().join("fork")).unwrap(),
        chunk_index(&[[10, 0, 0, 0, 2], [11, 0, 1, 2, 1]])
    );
    let mut failed = producer::Index::default();
    failed.open(tmp.path().join("missing/index").to_str().unwrap());
    failed.capture(100, 10, 0, &mut order);
    assert!(failed.records.is_empty());
    assert_eq!(failed.seen, 100);
}

#[test]
fn producer_enforces_hard_cap_and_file_errors_do_not_fail_output_capture() {
    let mut index = producer::Index::default();
    index.limit = Some(usize::MAX);
    let mut order = 0;
    for end in 1..=65_540 {
        index.capture(end, 7, 0, &mut order);
        index.finish();
    }
    assert_eq!(index.records.len(), 65_536);
    assert_eq!(index.seen, 65_540);
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("readonly");
    std::fs::write(&path, b"SBLOG001").unwrap();
    let mut index = producer::Index::default();
    index.file = Some(std::io::BufWriter::new(std::fs::File::open(&path).unwrap()));
    index.capture(1, 7, 0, &mut order);
    index.finish(); // Buffered write failure latches at the slice boundary.
    index.capture(2, 8, 0, &mut order);
    assert_eq!(index.records.len(), 1);
    assert_eq!(std::fs::read(path).unwrap(), b"SBLOG001");
    assert_eq!(index.seen, 2);
}

#[test]
fn coalescing_preserves_stream_switches_incarnations_slice_boundaries_and_cap() {
    let mut console = producer::Index::default();
    let mut mgmt = producer::Index::default();
    console.limit = Some(4);
    let mut order = 0;
    console.capture(1, 10, 0, &mut order);
    console.capture(2, 10, 0, &mut order);
    console.capture(3, 11, 0, &mut order);
    mgmt.capture(1, 11, 0, &mut order);
    console.capture(4, 11, 0, &mut order);
    console.capture(5, 11, 1, &mut order);
    console.finish();
    mgmt.finish();
    console.capture(6, 11, 1, &mut order);
    // The last allowed record may still grow, preserving all its bytes.
    console.capture(100, 12, 1, &mut order);
    console.finish();
    console.capture(101, 12, 1, &mut order);
    console.capture(102, 13, 1, &mut order);
    assert_eq!(
        rows(&console),
        [
            [10, 0, 0, 0, 3],
            [11, 0, 2, 3, 1],
            [11, 1, 3, 4, 1],
            [11, 1, 4, 5, 95],
        ]
    );
    assert_eq!(rows(&mgmt), [[11, 0, 1, 0, 1]]);
    assert_eq!(console.seen, 102);
    assert_eq!(order, 7);

    // Even unindexed output on the other stream breaks a burst.
    mgmt.limit = Some(0);
    let mut other = producer::Index::default();
    other.capture(1, 14, 1, &mut order);
    mgmt.capture(2, 14, 1, &mut order);
    other.capture(2, 14, 1, &mut order);
    assert_eq!(rows(&other), [[14, 1, 7, 0, 1], [14, 1, 9, 1, 1]]);
}

#[test]
fn drop_flushes_the_last_coalesced_record() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("index");
    {
        let mut index = producer::Index::default();
        index.open(path.to_str().unwrap());
        let mut order = 0;
        index.capture(1, 7, 0, &mut order);
        index.capture(3, 8, 0, &mut order);
    }
    assert_eq!(
        std::fs::read(path).unwrap(),
        chunk_index(&[[7, 0, 0, 0, 3]])
    );
}
