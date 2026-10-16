//! Offline manifest, report and retained-log integration.
mod physical_ram;
use sb::diff::*;
use sb::store::ContentDigest;
use seedbox_artifacts as sb;
use serde_json::Value;
use std::collections::BTreeMap;
include!("support/diff_fixture.rs");

fn arm_fixture() -> sb::record::RunRecord {
    let mut record = fixture();
    record.identity.inputs.profile.backend = sb::execution::Backend::Hvf;
    let parameters = &mut record.identity.inputs.profile.semantic_parameters;
    parameters.engine = Some(sb::engine::arm_descriptor());
    record
}

fn arm_report() -> Value {
    let mut report = raw_report();
    report["report_version"] = 2.into();
    report["timebase"] = sb::engine::ARM_REPORT_TIMEBASE.into();
    report
}

fn arm_report_error(record: &mut sb::record::RunRecord, report: &Value) -> String {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("arm"), record, Some(report), None);
    diff(&dir.path().join("arm"), &dir.path().join("arm"))
        .unwrap_err()
        .to_string()
}

#[test]
fn native_hvf_reports_require_the_arm_descriptor_and_matching_header() {
    let dir = tempfile::tempdir().unwrap();
    let report = arm_report();
    write_fixture(
        &dir.path().join("a"),
        &mut arm_fixture(),
        Some(&report),
        None,
    );
    write_fixture(
        &dir.path().join("b"),
        &mut arm_fixture(),
        Some(&report),
        None,
    );
    assert_eq!(
        diff(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap()
            .status,
        Status::Match
    );

    let mut record = arm_fixture();
    record.identity.inputs.profile.backend = sb::execution::Backend::Kvm;
    let error = arm_report_error(&mut record, &report);
    assert!(
        error.contains("HVF backend and ARM executable descriptor disagree"),
        "{error}"
    );

    let mut record = arm_fixture();
    record.identity.inputs.profile.semantic_parameters =
        sb::execution::SemanticParameters::new(512);
    let error = arm_report_error(&mut record, &report);
    assert!(
        error.contains("HVF backend and ARM executable descriptor disagree"),
        "{error}"
    );

    // Descriptive clock capability metadata does not gate report decoding.
    let mut descriptive = arm_fixture();
    if let Some(engine) = &mut descriptive
        .identity
        .inputs
        .profile
        .semantic_parameters
        .engine
    {
        engine.engine_timebase = "future-capabilities".into();
    }
    write_fixture(
        &dir.path().join("descriptive"),
        &mut descriptive,
        Some(&report),
        None,
    );
    assert_eq!(
        diff(
            &dir.path().join("descriptive"),
            &dir.path().join("descriptive")
        )
        .unwrap()
        .status,
        Status::Match
    );

    let mut wrong_version = report.clone();
    wrong_version["report_version"] = 1.into();
    let error = arm_report_error(&mut arm_fixture(), &wrong_version);
    assert!(error.contains("unsupported VMM report version"), "{error}");

    let mut wrong_timebase = report.clone();
    wrong_timebase["timebase"] = "pv".into();
    let error = arm_report_error(&mut arm_fixture(), &wrong_timebase);
    assert!(
        error.contains("HVF report timebase disagrees with executable profile"),
        "{error}"
    );

    let mut wrong_seed = report.clone();
    wrong_seed["seed"] = 1.into();
    let error = arm_report_error(&mut arm_fixture(), &wrong_seed);
    assert!(
        error.contains("VMM report seed disagrees with manifest"),
        "{error}"
    );
}

#[test]
fn ram_pages_are_read_where_the_vmm_writes_them() {
    // vmm.rs passes --out output/machines, so NAME.pages sits beside NAME.trace.
    let (a, b) = ram_pair();
    let dir = tempfile::tempdir().unwrap();
    for (name, raw, pfns) in [("a", &a, vec![]), ("b", &b, vec![3usize])] {
        let path = dir.path().join(name);
        let mut record = fixture();
        write_fixture(&path, &mut record, Some(raw), None);
        let mut bytes = vec![0u8; 1024 * 1024];
        for pfn in pfns {
            bytes[pfn * 8] = 1;
        }
        std::fs::write(path.join("output/machines/server.pages"), &bytes).unwrap();
        record
            .logs
            .insert("machines/server.pages".into(), ContentDigest::of(&bytes));
        write_fixture(&path, &mut record, Some(raw), None);
    }
    let report = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
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
    write_fixture(&left, &mut ra, Some(&a), None);
    write_fixture(&right, &mut rb, Some(&b), None);
    write_pages(&left, &mut ra, &[]);
    write_pages(&right, &mut rb, &[0, 3, 9, 10, 11, 12, 13, 14, 15, 131071]);
    write_fixture(&left, &mut ra, Some(&a), None);
    write_fixture(&right, &mut rb, Some(&b), None);
    let result = diff(&left, &right).unwrap();
    let pages = result.machines[0].ram_pages.as_ref().unwrap();
    assert_eq!(pages.differing_pfns, 10);
    assert_eq!(pages.first_pfns, [0, 3, 9, 10, 11, 12, 13, 14]);
    assert_eq!(ra.run_id, rb.run_id);
    assert!(result.identity_differences.is_empty());

    // Even another textual part changing rules out a RAM-only diagnostic.
    let mut mixed = b.clone();
    mixed["machines"][1]["state_digest_parts"]["events"] = "different events".into();
    write_fixture(&right, &mut rb, Some(&mixed), None);
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
    std::fs::write(left.join("output/machines/server.pages"), b"junk").unwrap();
    assert!(diff(&left, &right).unwrap().machines[0].ram_pages.is_none());
    write_pages(&left, &mut ra, &[]);
    write_pages(&right, &mut rb, &[1]);
    write_fixture(&left, &mut ra, Some(&a), None);
    write_fixture(&right, &mut rb, Some(&b), None);
    std::fs::write(right.join("output/machines/server.pages"), b"corrupt").unwrap();
    assert!(diff(&left, &right)
        .unwrap_err()
        .to_string()
        .contains("evidence checksum mismatch"));
    for length in [7, 8, 1024 * 1024 - 8] {
        let bytes = vec![0; length];
        std::fs::write(right.join("output/machines/server.pages"), &bytes).unwrap();
        rb.logs
            .insert("machines/server.pages".into(), ContentDigest::of(&bytes));
        write_fixture(&right, &mut rb, Some(&b), None);
        assert!(diff(&left, &right)
            .unwrap_err()
            .to_string()
            .contains("page hash length must be 1048576 bytes"));
    }
    rb.logs.remove("machines/server.pages");
    rb.omitted_logs
        .insert("machines/server.pages".into(), "retention cap".into());
    write_fixture(&right, &mut rb, Some(&b), None);
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
        write_fixture(&left, &mut record, Some(&a), None);
        let after = diff(&left, &dir.path().join("b")).unwrap();
        assert_eq!(
            serde_json::to_vec(&before).unwrap(),
            serde_json::to_vec(&after).unwrap()
        );
    }
}

fn pair(a: &Value, b: &Value) -> (tempfile::TempDir, DiffReport) {
    let dir = tempfile::tempdir().unwrap();
    write_fixture(&dir.path().join("a"), &mut fixture(), Some(a), None);
    write_fixture(&dir.path().join("b"), &mut fixture(), Some(b), None);
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
        assert_eq!(machine.witness.matches, Some(true));
        assert_eq!(machine.full_state.matches, None);
        assert!(machine.differences.is_empty());
    }
}

#[test]
fn current_parts_report_ram_and_text_events_with_trace_precedence() {
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
        assert_eq!(first.source, "terminal");
        assert_eq!(
            first.a.unwrap().value["state_digest_parts"][part],
            a["machines"][0]["state_digest_parts"][part]
        );
        assert!(result.machines[1]
            .differences
            .iter()
            .any(|d| d.field == format!("/state_digest_parts/{part}")));

        let dir = tempfile::tempdir().unwrap();
        write_fixture(
            &dir.path().join("a"),
            &mut fixture(),
            Some(&a),
            Some(b"I 1 2 3\n"),
        );
        write_fixture(
            &dir.path().join("b"),
            &mut fixture(),
            Some(&b),
            Some(b"H 1 2 3\n"),
        );
        let first = diff(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap()
            .first_record
            .unwrap();
        assert_eq!(first.source, "trace");
        assert_eq!(first.record, Some(1));
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
    write_fixture(
        &dir.path().join("a"),
        &mut fixture(),
        Some(&raw_report()),
        None,
    );
    write_fixture(&dir.path().join("b"), &mut b, Some(&raw), None);
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(result.world_witness.matches, Some(false));
    assert!(result
        .machines
        .iter()
        .all(|m| m.witness.matches == Some(true)));
    assert!(result.first_record.is_none());
}

#[test]
fn machine_hash_and_full_state_differences_have_no_claimed_exit_time() {
    let a = raw_report();
    let mut b = a.clone();
    b["machines"][0]["witness"] = serde_json::to_value(ContentDigest::of(b"changed")).unwrap();
    b["machines"][0]["state_digest"] = serde_json::to_value(ContentDigest::of(b"ram")).unwrap();
    let (_, result) = pair(&a, &b);
    assert_eq!(result.status, Status::Different);
    assert_eq!(result.machines[0].witness.matches, Some(false));
    assert!(result.first_record.is_none());
}

#[test]
fn terminal_difference_reports_machine_time_and_kind() {
    let a = raw_report();
    let mut b = a.clone();
    b["machines"][0]["guest_exit_code"] = 1.into();
    b["machines"][0]["time_ns"] = 105.into();
    let (_, result) = pair(&a, &b);
    let first = result.first_record.unwrap();
    assert_eq!(first.machine, "workload");
    assert_eq!(first.source, "terminal");
    assert_eq!(first.a.unwrap().time_ns, Some(100));
    assert_eq!(first.b.unwrap().time_ns, Some(105));
}

#[test]
fn identity_lists_semantics_and_ignores_attempt_and_performance() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = fixture();
    b.attempt = "another".into();
    b.performance_parameters
        .insert("SEEDBOX_UNDO".into(), "0".into());
    write_fixture(&dir.path().join("a"), &mut fixture(), None, None);
    write_fixture(&dir.path().join("b"), &mut b, None, None);
    assert_eq!(
        diff(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap()
            .status,
        Status::Match
    );
    b.identity.seed = 2;
    b.identity.inputs.kernel = ContentDigest::of(b"kernel");
    b.identity.inputs.profile.engine = ContentDigest::of(b"engine");
    b.identity.inputs.profile.semantic_parameters.memory_mb = 1024;
    write_fixture(&dir.path().join("b"), &mut b, None, None);
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    let paths: Vec<_> = result
        .identity_differences
        .iter()
        .map(|d| d.field.as_str())
        .collect();
    assert_eq!(
        paths,
        [
            "/inputs/kernel",
            "/inputs/profile/engine",
            "/inputs/profile/semantic_parameters/memory_mb",
            "/seed"
        ]
    );
}

#[test]
fn diff_uses_retained_report_to_decode_legacy_samples() {
    let dir = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = [0u64, 17, 1, 7]
        .into_iter()
        .flat_map(u64::to_le_bytes)
        .collect();
    let mut report = raw_report();
    report["timebase"] = "pv".into();
    for side in ["a", "b"] {
        write_fixture_logs(
            &dir.path().join(side),
            &mut fixture(),
            Some(&report),
            &[("machines/workload.witness-log", Some(&bytes))],
        );
    }
    assert_eq!(
        diff(&dir.path().join("a"), &dir.path().join("b"))
            .unwrap()
            .status,
        Status::Match
    );
}

#[test]
fn diff_reports_witness_log_divergence_and_one_sided_logs() {
    let dir = tempfile::tempdir().unwrap();
    let name = "machines/workload.witness-log";
    let x = witness_log_bytes(&[[0, 1, 7], [1_000_000, 4, 8]]);
    let y = witness_log_bytes(&[[0, 1, 7], [1_000_000, 4, 6]]);
    for (side, log) in [("a", Some(&x[..])), ("b", Some(&y[..])), ("c", None)] {
        write_fixture_logs(
            &dir.path().join(side),
            &mut fixture(),
            Some(&raw_report()),
            &[(name, log)],
        );
    }
    let result = diff(&dir.path().join("a"), &dir.path().join("a")).unwrap();
    assert_eq!(result.status, Status::Match);
    assert!(result.divergence.is_none());
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(result.status, Status::Different);
    let d = result.divergence.unwrap();
    assert_eq!((d.machine.as_str(), d.sample), ("workload", 2));
    assert_eq!(d.last_match.unwrap().time_ns, 0);
    let report = replay_divergence(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(report.divergence.unwrap().sample, 2);
    let result = diff(&dir.path().join("a"), &dir.path().join("c")).unwrap();
    assert_eq!(result.status, Status::Different);
    assert!(result.divergence.is_none());
    assert!(result
        .evidence_differences
        .iter()
        .any(|d| d.field == "/logs/machines~1workload.witness-log"));
    let result = diff(&dir.path().join("c"), &dir.path().join("c")).unwrap();
    assert_eq!(result.status, Status::Match);
    assert!(result
        .limitations
        .iter()
        .any(|l| l == "workload: no retained witness log on either side"));
}

#[test]
fn absent_and_omitted_evidence_stays_explicit_and_is_not_read() {
    let dir = tempfile::tempdir().unwrap();
    let mut record = fixture();
    record.report = None;
    record.outcome = sb::record::Outcome::Incomplete;
    record
        .omitted_logs
        .insert("vmm.json".into(), "over quota".into());
    write_fixture(dir.path(), &mut record, None, None);
    std::fs::write(dir.path().join("output/vmm.json"), "not retained").unwrap();
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
    write_fixture(dir.path(), &mut record, None, None);
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
    write_fixture(&dir.path().join("a"), &mut fixture(), None, None);
    write_fixture(&dir.path().join("b"), &mut b, None, None);
    let result = diff(&dir.path().join("a"), &dir.path().join("b")).unwrap();
    assert_eq!(result.status, Status::Different);
    assert!(result.first_record.is_none());
}
