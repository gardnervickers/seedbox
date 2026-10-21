//! Synthetic retained evidence: validates addressing/format, not native RAM capture.
use super::*;
use sb::{
    evidence::{RamRegion, StateCapture, StateDisposition, StateReport},
    store::ContentDigest,
};
use std::collections::BTreeMap;

fn capture(ram: &[u8], digest: &[u8]) -> StateCapture {
    let regions = vec![
        RamRegion {
            base: 0,
            length: 8192,
        },
        RamRegion {
            base: 0x4000_0000,
            length: 4097,
        },
    ];
    let encoded: Vec<_> = regions
        .iter()
        .flat_map(|region| [region.base.to_le_bytes(), region.length.to_le_bytes()].concat())
        .collect();
    StateCapture {
        page_log: "machines/workload.pages".into(),
        boot: 3,
        disposition: StateDisposition::Crashed,
        digest: ContentDigest::of(digest),
        parts: BTreeMap::from([
            ("ram".into(), ContentDigest::of(ram)),
            ("ram_pages".into(), ContentDigest::of(ram)),
            ("ram_regions".into(), ContentDigest::of(&encoded)),
            ("cpu".into(), ContentDigest::of(b"synthetic cpu part")),
        ]),
        regions,
    }
}
fn write(path: &std::path::Path, capture: StateCapture, hashes: &[u64]) {
    let mut record = fixture();
    record.report.as_mut().unwrap().full_state = Some(BTreeMap::from([(
        "workload".into(),
        capture.digest.clone(),
    )]));
    let report = StateReport {
        version: 1,
        machines: BTreeMap::from([("workload".into(), capture)]),
    };
    let bytes = serde_json::to_vec(&report).unwrap();
    let pages: Vec<_> = hashes.iter().flat_map(|hash| hash.to_le_bytes()).collect();
    write_fixture_logs(
        path,
        &mut record,
        None,
        &[
            ("machines/driver-state.json", Some(&bytes)),
            ("machines/machines/workload.pages", Some(&pages)),
        ],
    );
}
#[test]
fn two_machines_cannot_claim_the_same_page_log_path() {
    for second_path in ["machines/workload.pages", "machines//workload.pages"] {
        let first = capture(b"first synthetic RAM", b"first state");
        let mut second = capture(b"second synthetic RAM", b"second state");
        second.page_log = second_path.into();
        let mut report = StateReport {
            version: 1,
            machines: BTreeMap::from([("first".into(), first), ("second".into(), second)]),
        };
        assert!(report
            .validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate stopped-state page log path"));
        report.machines.get_mut("second").unwrap().page_log = "machines/second.pages".into();
        report.validate().unwrap();
    }
}
#[test]
fn retained_sparse_pages_report_physical_pfns_and_gpas_instead_of_vector_indices() {
    let root = tempfile::tempdir().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    write(&left, capture(b"left RAM", b"left combined"), &[1, 2, 3, 4]);
    write(
        &right,
        capture(b"right RAM", b"right combined"),
        &[1, 99, 3, 88],
    );
    let report = sb::diff::replay_ram_pages(&left, &right).unwrap();
    let pages = &report.machines["workload"];
    assert_eq!(pages.differing_pfns, 2);
    assert_eq!(pages.first_pfns, [1, 0x4000_1000 / 4096]);
    assert_eq!(pages.first_gpas, [4096, 0x4000_1000]);
    let diff = sb::diff::diff(&left, &right).unwrap();
    assert_eq!(diff.machines[0].ram_pages.as_ref().unwrap(), pages);
    assert!(report.limitations.is_empty());
}
#[test]
fn region_digest_and_mapped_byte_count_are_checked_before_comparison() {
    let root = tempfile::tempdir().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    write(&left, capture(b"left RAM", b"left combined"), &[1, 2, 3, 4]);
    write(&right, capture(b"right RAM", b"right combined"), &[1, 2, 3]);
    assert!(sb::diff::replay_ram_pages(&left, &right)
        .unwrap_err()
        .to_string()
        .contains("page hash length must be 32 bytes"));
    let mut malformed = capture(b"right RAM", b"right combined");
    malformed.regions[1].base = 0x5000_0000;
    write(&right, malformed, &[1, 2, 3, 4]);
    assert!(sb::diff::replay_ram_pages(&left, &right)
        .unwrap_err()
        .to_string()
        .contains("RAM-region digest mismatch"));
}
#[test]
fn ended_identity_round_trips() {
    let original = capture(b"RAM", b"combined");
    assert_eq!(original.page_bytes().unwrap(), 32);
    let decoded: StateCapture =
        serde_json::from_slice(&serde_json::to_vec(&original).unwrap()).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(decoded.boot, 3);
    assert_eq!(decoded.disposition, StateDisposition::Crashed);
}
#[test]
fn invalid_regions_fail_without_inventing_hole_or_partial_page_bytes() {
    for regions in [
        vec![],
        vec![RamRegion {
            base: 1,
            length: 4096,
        }],
        vec![RamRegion { base: 0, length: 0 }],
        vec![
            RamRegion {
                base: 0,
                length: 8192,
            },
            RamRegion {
                base: 4096,
                length: 4096,
            },
        ],
        vec![RamRegion {
            base: u64::MAX - 4095,
            length: 4096,
        }],
    ] {
        let mut malformed = capture(b"RAM", b"combined");
        malformed.regions = regions;
        assert!(malformed.page_bytes().is_err());
    }
}
