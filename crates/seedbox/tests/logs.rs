//! Portable format/reader controls, not KVM qualification.
use sb::logs::{Filter, Reader, Stream};
use seedbox as sb;
#[allow(dead_code)]
mod fixtures {
    use super::sb;
    include!("support/diff_fixture.rs");
    include!("support/logs_fixture.rs");
    pub(super) fn make(path: &std::path::Path) -> sb::orchestration::RunRecord {
        timed_fixture(path)
    }
    pub(super) fn replace(
        path: &std::path::Path,
        record: &mut sb::orchestration::RunRecord,
        name: &str,
        bytes: &[u8],
    ) {
        add_log(path, record, name, bytes);
        write_fixture(path, record, None, None);
    }
    pub(super) fn save(path: &std::path::Path, record: &mut sb::orchestration::RunRecord) {
        write_fixture(path, record, None, None);
    }
    pub(super) fn index(records: &[[u64; 5]]) -> Vec<u8> {
        chunk_index(records)
    }
}
use seedbox_engine_core::log_index as producer;

/// Index records as `[time, incarnation, order, offset, length]` rows.
fn rows(index: &producer::Index) -> Vec<[u64; 5]> {
    index
        .records
        .iter()
        .map(|r| [r.time_ns, r.incarnation, r.order, r.offset, r.length])
        .collect()
}

#[test]
fn binary_preservation_filters_equal_time_and_incarnations() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    fixtures::make(path);
    let mut reader = Reader::open(path, &Filter::default()).unwrap();
    assert!(reader.report.omissions.is_empty());
    assert_eq!(
        reader
            .report
            .chunks
            .iter()
            .map(|c| c.order)
            .collect::<Vec<_>>(),
        [0, 1, 2, 3]
    );
    let mut bytes = Vec::new();
    for i in 0..reader.report.chunks.len() {
        reader.copy_chunk(i, &mut bytes).unwrap();
    }
    assert_eq!(bytes, b"\0\xff\xfeA\nB");
    for (filter, expected) in [
        (
            Filter {
                since_ns: Some(10),
                until_ns: Some(20),
                ..Default::default()
            },
            vec![0, 1, 2],
        ),
        (
            Filter {
                since_ns: Some(20),
                ..Default::default()
            },
            vec![3],
        ),
        (
            Filter {
                since_ns: Some(10),
                until_ns: Some(10),
                ..Default::default()
            },
            vec![],
        ),
        (
            Filter {
                stream: Some(Stream::Mgmt),
                ..Default::default()
            },
            vec![1],
        ),
        (
            Filter {
                machine: Some("workload".into()),
                stream: Some(Stream::Console),
                ..Default::default()
            },
            vec![0, 2, 3],
        ),
    ] {
        let r = Reader::open(path, &filter).unwrap();
        assert_eq!(
            r.report.chunks.iter().map(|c| c.order).collect::<Vec<_>>(),
            expected
        );
    }
    let mut encoded = Vec::new();
    reader.write_chunk_json(0, &mut encoded).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&encoded).unwrap()["bytes_hex"],
        "00ff"
    );
}

#[test]
fn whole_report_writers_keep_the_cli_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    fixtures::make(tmp.path());
    let mut reader = Reader::open(tmp.path(), &Filter::default()).unwrap();
    let mut raw = Vec::new();
    reader.write_raw(&mut raw).unwrap();
    assert_eq!(raw, b"\0\xff\xfeA\nB");
    // The document the CLI assembled by hand before `write_json` existed.
    let mut expected = format!(
        "{{\n  \"status\": {},\n  \"omissions\": {},\n  \"chunks\": [\n",
        serde_json::to_string(&reader.report.status).unwrap(),
        serde_json::to_string_pretty(&reader.report.omissions).unwrap()
    )
    .into_bytes();
    let count = reader.report.chunks.len();
    for index in 0..count {
        expected.extend_from_slice(b"    ");
        reader.write_chunk_json(index, &mut expected).unwrap();
        if index + 1 < count {
            expected.push(b',');
        }
        expected.push(b'\n');
    }
    expected.extend_from_slice(b"  ]\n}\n");
    let mut json = Vec::new();
    reader.write_json(&mut json).unwrap();
    assert_eq!(
        String::from_utf8(json).unwrap(),
        String::from_utf8(expected).unwrap()
    );
}

#[test]
fn reject_corrupt_truncated_out_of_bounds_and_disordered_indexes_before_filtering() {
    for bytes in [
        b"SBLOG002".to_vec(),
        b"SBLOG001\0".to_vec(),
        vec![],
        fixtures::index(&[[10, 0, 0, 1, 1]]),
        fixtures::index(&[[10, 0, 0, 0, 0]]),
        fixtures::index(&[[10, 0, 0, 0, 6]]),
        fixtures::index(&[[10, 0, 0, 0, u64::MAX], [20, 0, 1, u64::MAX, 1]]),
        fixtures::index(&[[10, u32::MAX as u64 + 1, 0, 0, 1]]),
        fixtures::index(&[[10, 0, 0, 0, 1], [9, 0, 2, 1, 1]]),
        fixtures::index(&[[10, 1, 0, 0, 1], [20, 0, 2, 1, 1]]),
        fixtures::index(&[[10, 0, 2, 0, 1], [20, 0, 2, 1, 1]]),
        fixtures::index(&[[10, 0, 1, 0, 1]]), // duplicated cross-stream order
        fixtures::index(&[[11, 0, 0, 0, 1]]), // management order 1 regresses time
        fixtures::index(&vec![[10, 0, 0, 0, 1]; 65_537]),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path();
        let mut record = fixtures::make(path);
        fixtures::replace(
            path,
            &mut record,
            "machines/workload.console.chunks",
            &bytes,
        );
        assert!(Reader::open(
            path,
            &Filter {
                since_ns: Some(999),
                stream: Some(Stream::Mgmt),
                ..Default::default()
            }
        )
        .is_err());
    }
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    fixtures::make(path);
    std::fs::write(path.join("output/machines/workload.console"), b"changed").unwrap();
    assert!(Reader::open(path, &Filter::default())
        .err()
        .unwrap()
        .to_string()
        .contains("checksum mismatch"));
}

#[test]
fn omissions_cover_legacy_runs_missing_streams_and_unindexed_suffixes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    let mut record = fixtures::make(path);
    fixtures::replace(
        path,
        &mut record,
        "machines/workload.console",
        b"\0\xffA\nBtail",
    );
    record.logs.remove("machines/workload.mgmt");
    record
        .omitted_logs
        .insert("machines/workload.mgmt".into(), "retention cap".into());
    fixtures::save(path, &mut record);
    let r = Reader::open(path, &Filter::default()).unwrap();
    assert_eq!(r.report.chunks.len(), 3);
    assert_eq!(
        r.report.omissions["machines/workload.mgmt"],
        "retention cap"
    );
    assert!(r.report.omissions["machines/workload.console.chunks"].contains("[5, 9)"));
    record.logs.retain(|name, _| !name.ends_with(".chunks"));
    fixtures::save(path, &mut record);
    let r = Reader::open(path, &Filter::default()).unwrap();
    assert!(r.report.chunks.is_empty());
    assert!(r.report.omissions["machines/workload.console.chunks"].contains("unavailable"));
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
        fixtures::index(&[[10, 0, 0, 0, 2], [10, 0, 1, 2, 2]])
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
        fixtures::index(&[[10, 0, 0, 0, 2], [11, 0, 1, 2, 1]])
    );
    let mut failed = producer::Index::default();
    failed.open(tmp.path().join("missing/index").to_str().unwrap());
    failed.capture(100, 10, 0, &mut order);
    assert!(failed.records.is_empty());
    assert_eq!(failed.seen, 100);
}

#[test]
fn merged_machine_ties_are_stable_and_symlinked_indexes_are_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    let mut record = fixtures::make(path);
    let digest = record.identity.inputs.guests["workload"].clone();
    record.identity.inputs.guests.insert("alpha".into(), digest);
    fixtures::replace(path, &mut record, "machines/alpha.console", b"a");
    fixtures::replace(
        path,
        &mut record,
        "machines/alpha.console.chunks",
        &fixtures::index(&[[10, 0, 0, 0, 1]]),
    );
    let r = Reader::open(path, &Filter::default()).unwrap();
    assert_eq!(r.report.chunks[0].machine, "alpha");
    assert_eq!(r.report.chunks[1].machine, "workload");
    let index = path.join("output/machines/alpha.console.chunks");
    std::fs::remove_file(&index).unwrap();
    std::os::unix::fs::symlink("workload.console.chunks", index).unwrap();
    assert!(Reader::open(path, &Filter::default())
        .err()
        .unwrap()
        .to_string()
        .contains("regular file"));
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
fn one_mib_of_byte_exits_coalesces_into_slice_ranges_and_round_trips() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path();
    let mut record = fixtures::make(path);
    let index_path = path.join("large.chunks");
    let mut index = producer::Index::default();
    index.open(index_path.to_str().unwrap());
    let mut order = 0;
    let raw: Vec<u8> = (0..1_048_576).map(|n| n as u8).collect();
    for end in 1..=raw.len() as u64 {
        // COM1 exits one byte at a time, with advancing simulated time.
        index.capture(end, end, 0, &mut order);
        if end % 4096 == 0 {
            index.finish();
        }
    }
    assert_eq!(index.records.len(), 256);
    assert!(index.records.len() < producer::MAX_CHUNKS / 100);
    assert_eq!(order, 256);
    let encoded = std::fs::read(index_path).unwrap();
    assert_eq!(encoded.len(), 8 + 256 * 40);
    for (i, chunk) in rows(&index).iter().enumerate() {
        let offset = i as u64 * 4096;
        assert_eq!(*chunk, [offset + 1, 0, i as u64, offset, 4096]);
    }
    fixtures::replace(path, &mut record, "machines/workload.console", &raw);
    fixtures::replace(
        path,
        &mut record,
        "machines/workload.console.chunks",
        &encoded,
    );
    fixtures::replace(path, &mut record, "machines/workload.mgmt", b"");
    fixtures::replace(
        path,
        &mut record,
        "machines/workload.mgmt.chunks",
        producer::MAGIC,
    );
    let mut reader = Reader::open(path, &Filter::default()).unwrap();
    assert!(reader.report.omissions.is_empty());
    let mut copied = Vec::new();
    for i in 0..reader.report.chunks.len() {
        reader.copy_chunk(i, &mut copied).unwrap();
    }
    assert_eq!(copied, raw);
    let mut reader = Reader::open(
        path,
        &Filter {
            since_ns: Some(4096),
            until_ns: Some(8193),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(reader.report.chunks.len(), 1);
    let mut copied = Vec::new();
    reader.copy_chunk(0, &mut copied).unwrap();
    assert_eq!(copied, raw[4096..8192]);
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
        fixtures::index(&[[7, 0, 0, 0, 3]])
    );
}
