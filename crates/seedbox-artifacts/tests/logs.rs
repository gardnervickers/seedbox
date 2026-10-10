//! Portable format/reader controls, not KVM qualification.
use sb::logs::{Filter, Reader, Stream};
use seedbox_artifacts as sb;
#[allow(dead_code)]
mod fixtures {
    use super::sb;
    include!("support/diff_fixture.rs");
    include!("support/logs_fixture.rs");
    pub(super) fn make(path: &std::path::Path) -> sb::record::RunRecord {
        timed_fixture(path)
    }
    pub(super) fn replace(
        path: &std::path::Path,
        record: &mut sb::record::RunRecord,
        name: &str,
        bytes: &[u8],
    ) {
        add_log(path, record, name, bytes);
        write_fixture(path, record, None, None);
    }
    pub(super) fn save(path: &std::path::Path, record: &mut sb::record::RunRecord) {
        write_fixture(path, record, None, None);
    }
    pub(super) fn index(records: &[[u64; 5]]) -> Vec<u8> {
        chunk_index(records)
    }
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
