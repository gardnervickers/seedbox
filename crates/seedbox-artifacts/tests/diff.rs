//! Offline manifest, report and retained-log integration.
mod physical_ram;
use sb::diff::*;
use sb::store::ContentDigest;
use sb::test_support::*;
use seedbox_artifacts as sb;
use serde_json::Value;
use std::collections::BTreeMap;

#[test]
fn ram_pages_are_read_from_the_current_capture_page_path() {
    // The normal Driver worker writes its report beneath output/machines and
    // page_log is relative to that directory (including its own machines/).
    let (a, b) = ram_pair();
    let dir = tempfile::tempdir().unwrap();
    for (name, raw, pfns) in [("a", &a, vec![]), ("b", &b, vec![3usize])] {
        let path = dir.path().join(name);
        let mut record = fixture();
        write_fixture(&path, &mut record, Some(raw));
        let mut bytes = vec![0u8; 1024 * 1024];
        for pfn in pfns {
            bytes[pfn * 8] = 1;
        }
        std::fs::write(path.join("output/machines/machines/server.pages"), &bytes).unwrap();
        record.logs.insert(
            "machines/machines/server.pages".into(),
            ContentDigest::of(&bytes),
        );
        write_fixture(&path, &mut record, Some(raw));
    }
    let report = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert!(report.machines[0]
        .differences
        .iter()
        .any(|difference| difference.field == "/state_digest_parts/ram"));
    assert!(!report
        .limitations
        .iter()
        .any(|message| message.contains("driver-state.json not retained")));
    assert_eq!(
        report.machines[0].ram_pages.as_ref().unwrap().first_pfns,
        [3]
    );
}

#[test]
fn ram_pages_count_all_pfns_and_bound_the_sample() {
    let (a, b) = ram_pair();
    let dir = tempfile::tempdir().unwrap();
    let left = dir.path().join("a");
    let right = dir.path().join("b");
    let mut ra = fixture();
    let mut rb = fixture();
    write_fixture(&left, &mut ra, Some(&a));
    write_fixture(&right, &mut rb, Some(&b));
    write_pages(&left, &mut ra, &[]);
    write_pages(&right, &mut rb, &[0, 3, 9, 10, 11, 12, 13, 14, 15, 131071]);
    write_fixture(&left, &mut ra, Some(&a));
    write_fixture(&right, &mut rb, Some(&b));
    let result = diff(&left, &right).unwrap();
    let pages = result.machines[0].ram_pages.as_ref().unwrap();
    assert_eq!(pages.differing_pfns, 10);
    assert_eq!(pages.first_pfns, [0, 3, 9, 10, 11, 12, 13, 14]);
    assert_eq!(ra.run_id, rb.run_id);
    assert!(result.identity_differences.is_empty());

    // Even another textual part changing rules out a RAM-only diagnostic.
    let mut mixed = b.clone();
    mixed["machines"][1]["state_digest_parts"]["events"] = "different events".into();
    write_fixture(&right, &mut rb, Some(&mixed));
    assert!(diff(&left, &right)
        .unwrap()
        .machines
        .iter()
        .all(|m| m.ram_pages.is_none()));
}

#[test]
fn ram_page_evidence_is_optional_but_declared_bytes_are_validated() {
    let (a, b) = ram_pair();
    let (dir, result) = pair(&a, &b);
    assert!(result
        .limitations
        .iter()
        .any(|l| l.contains("page hashes unavailable")));
    let left = dir.path().join("a");
    let right = dir.path().join("b");
    let mut ra = fixture();
    let mut rb = fixture();
    // Unregistered files on disk are ignored.
    std::fs::write(left.join("output/machines/machines/server.pages"), b"junk").unwrap();
    assert!(diff(&left, &right).unwrap().machines[0].ram_pages.is_none());
    write_pages(&left, &mut ra, &[]);
    write_pages(&right, &mut rb, &[1]);
    write_fixture(&left, &mut ra, Some(&a));
    write_fixture(&right, &mut rb, Some(&b));
    std::fs::write(
        right.join("output/machines/machines/server.pages"),
        b"corrupt",
    )
    .unwrap();
    assert!(diff(&left, &right)
        .unwrap_err()
        .to_string()
        .contains("evidence checksum mismatch"));
    for length in [7, 8, 1024 * 1024 - 8] {
        let bytes = vec![0; length];
        std::fs::write(right.join("output/machines/machines/server.pages"), &bytes).unwrap();
        rb.logs.insert(
            "machines/machines/server.pages".into(),
            ContentDigest::of(&bytes),
        );
        write_fixture(&right, &mut rb, Some(&b));
        assert!(diff(&left, &right)
            .unwrap_err()
            .to_string()
            .contains("page hash length must be 1048576 bytes"));
    }
    rb.logs.remove("machines/machines/server.pages");
    rb.omitted_logs.insert(
        "machines/machines/server.pages".into(),
        "retention cap".into(),
    );
    write_fixture(&right, &mut rb, Some(&b));
    let result = diff(&left, &right).unwrap();
    assert!(result.machines[0].ram_pages.is_none());
    assert!(result
        .limitations
        .iter()
        .any(|l| l.contains("page hashes unavailable")));
}

#[test]
fn page_capture_presence_alone_does_not_change_diff_or_witnesses() {
    for full_state in [false, true] {
        let a = current_report(full_state);
        let (dir, before) = pair(&a, &a);
        let mut record = fixture();
        let left = dir.path().join("a");
        write_pages(&left, &mut record, &[1]);
        write_fixture(&left, &mut record, Some(&a));
        let after = diff(&left, &dir.path().join("b")).unwrap();
        assert_eq!(
            serde_json::to_vec(&before).unwrap(),
            serde_json::to_vec(&after).unwrap()
        );
    }
}

fn pair(a: &Value, b: &Value) -> (tempfile::TempDir, DiffReport) {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("a"), &mut fixture(), Some(a));
    write_fixture(&dir.path().join("b"), &mut fixture(), Some(b));
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    (dir, result)
}

#[test]
fn current_reports_without_full_state_preserve_witnesses() {
    let a = current_report(false);
    let (_, result) = pair(&a, &a);
    assert_eq!(result.status, Status::Match);
    assert_eq!(result.world_witness.matches, Some(true));
    assert!(result.first_record.is_none());
    for machine in result.machines {
        assert_eq!(machine.witness.matches, None);
        assert_eq!(machine.full_state.matches, None);
        assert!(machine.differences.is_empty());
    }
}

#[test]
fn current_parts_report_ram_and_event_digests() {
    for part in ["ram", "events"] {
        let a = current_report(true);
        let mut b = a.clone();
        b["machines"][0]["state_digest_parts"][part] = if part == "events" {
            "exc:none int:none shadow:1 nmi:0/0/0".into()
        } else {
            serde_json::to_value(ContentDigest::of(b"changed ram")).unwrap()
        };
        let (_, result) = pair(&a, &b);
        let first = result.first_record.unwrap();
        assert_eq!(first.machine, "workload");
        assert_eq!(
            first.a.unwrap()["state_digest_parts"][part],
            if part == "events" {
                serde_json::to_value(ContentDigest::of(
                    a["machines"][0]["state_digest_parts"][part]
                        .as_str()
                        .unwrap()
                        .as_bytes(),
                ))
                .unwrap()
            } else {
                a["machines"][0]["state_digest_parts"][part].clone()
            }
        );
        assert!(result.machines[1]
            .differences
            .iter()
            .any(|d| d.field == format!("/state_digest_parts/{part}")));
    }
}

#[test]
fn performance_statistics_and_machine_order_do_not_diverge() {
    let mut a = raw_report();
    let mut second = a["machines"][0].clone();
    second["name"] = "other".into();
    a["machines"].as_array_mut().unwrap().push(second);
    let mut b = a.clone();
    b["machines"][0]["run_ms"] = 999.into();
    b["machines"][0]["shadow"] = serde_json::json!({"synced_pages":999, "restored_pages":1});
    b["machines"].as_array_mut().unwrap().reverse();
    assert_eq!(pair(&a, &b).1.status, Status::Match);
}

#[test]
fn world_difference_does_not_invent_a_machine_or_time() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = fixture();
    let mut raw = raw_report();
    let digest = ContentDigest::of(b"world changed, machines equal");
    b.report.as_mut().unwrap().witness = Some(digest.clone());
    raw["world_witness"] = serde_json::to_value(digest).unwrap();
    write_fixture(&dir.path().join("a"), &mut fixture(), Some(&raw_report()));
    write_fixture(&dir.path().join("b"), &mut b, Some(&raw));
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(result.world_witness.matches, Some(false));
    assert!(result.machines.iter().all(|m| m.witness.matches.is_none()));
    assert!(result.first_record.is_none());
}

#[test]
fn full_state_digest_differences_do_not_claim_an_exit_time() {
    let a = current_report(true);
    let mut b = a.clone();
    b["machines"][0]["state_digest"] = serde_json::to_value(ContentDigest::of(b"changed")).unwrap();
    let (_, result) = pair(&a, &b);
    assert_eq!(result.status, Status::Different);
    assert_eq!(result.machines[1].witness.matches, None);
    assert_eq!(result.machines[1].full_state.matches, Some(false));
    assert!(result.first_record.is_none());
}

#[test]
fn stopped_disposition_difference_reports_the_terminal_state() {
    let a = current_report(true);
    let mut b = a.clone();
    b["machines"][0]["guest_exit_code"] = 1.into();
    let (_, result) = pair(&a, &b);
    let first = result.first_record.unwrap();
    assert_eq!(first.machine, "workload");
    assert_ne!(first.a, first.b);
}

#[test]
fn identity_lists_semantics_and_ignores_attempt_and_performance() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = fixture();
    b.attempt = "another".into();
    b.performance_parameters
        .insert("SEEDBOX_UNDO".into(), "0".into());
    write_fixture(&dir.path().join("a"), &mut fixture(), None);
    write_fixture(&dir.path().join("b"), &mut b, None);
    assert_eq!(
        diff(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap()
            .status,
        Status::Match
    );
    b.identity.seed = 2;
    b.identity.inputs.auxiliary.insert(
        "driver-world/executable".into(),
        ContentDigest::of(b"engine"),
    );
    b.identity
        .inputs
        .machine_memory_mb
        .insert("workload".into(), 1024);
    write_fixture(&dir.path().join("b"), &mut b, None);
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    let paths: Vec<_> = result
        .identity_differences
        .iter()
        .map(|d| d.field.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "/inputs/auxiliary/driver-world~1executable",
            "/inputs/machine_memory_mb/workload",
            "/seed"
        ]
    );
}

#[test]
fn absent_and_omitted_evidence_stays_explicit_and_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut record = fixture();
    record.report = None;
    record.outcome = sb::record::Outcome::Incomplete;
    record
        .omitted_logs
        .insert("machines/driver-state.json".into(), "over quota".into());
    write_fixture(dir.path(), &mut record, None);
    std::fs::write(
        dir.path().join("output").join("machines/driver-state.json"),
        "not retained",
    )
    .unwrap();
    let result = diff(dir.path(), dir.path()).unwrap();
    assert_eq!(result.world_witness.matches, None);
    assert!(result.first_record.is_none());
    assert!(result.limitations.iter().any(|s| s.contains("over quota")));
}

#[test]
fn full_state_survives_omitted_raw_report() {
    let dir = tempfile::tempdir().unwrap();
    let mut record = fixture();
    record.report.as_mut().unwrap().full_state = Some(BTreeMap::from([(
        "workload".into(),
        ContentDigest::of(b"ram"),
    )]));
    write_fixture(dir.path(), &mut record, None);
    let result = diff(dir.path(), dir.path()).unwrap();
    assert_eq!(result.machines[0].full_state.matches, Some(true));
    assert_eq!(result.machines[0].witness.matches, None);
}

#[test]
fn full_state_only_difference_does_not_fabricate_terminal_record() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = fixture();
    b.report.as_mut().unwrap().full_state = Some(BTreeMap::from([(
        "workload".into(),
        ContentDigest::of(b"ram"),
    )]));
    write_fixture(&dir.path().join("a"), &mut fixture(), None);
    write_fixture(&dir.path().join("b"), &mut b, None);
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(result.status, Status::Different);
    assert!(result.first_record.is_none());
}
