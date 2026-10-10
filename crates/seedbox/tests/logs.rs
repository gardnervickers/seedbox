//! Portable format/reader controls, not KVM qualification.
use sb::logs::{Filter, Reader};
use seedbox as sb;
#[allow(dead_code)]
mod fixtures {
    use super::sb;
    include!("../../seedbox-artifacts/tests/support/diff_fixture.rs");
    include!("../../seedbox-artifacts/tests/support/logs_fixture.rs");
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
