//! Run manifests are strict: unsupported versions and unknown fixed fields are
//! rejected even when their checksums are recomputed. No guest execution.
use seedbox_artifacts::{
    record::{inspect, RunRecord},
    store::ContentDigest,
};
use std::fs;

#[allow(dead_code)]
mod record_fixture {
    use seedbox_artifacts as sb;
    include!("support/diff_fixture.rs");

    pub(super) fn record() -> sb::record::RunRecord {
        fixture()
    }
}

fn manifest(record: &RunRecord) -> serde_json::Value {
    serde_json::json!({
        "sha256": ContentDigest::of(&serde_json::to_vec(record).unwrap()),
        "record": record,
    })
}

#[test]
fn recomputed_valid_unsupported_versions_and_unknown_fixed_fields_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let mut original = record_fixture::record();
    original.run_id = ContentDigest::of(&serde_json::to_vec(&original.identity).unwrap());
    fs::write(
        root.path().join("manifest.json"),
        serde_json::to_vec(&manifest(&original)).unwrap(),
    )
    .unwrap();
    assert_eq!(inspect(root.path()).unwrap(), original);
    for version in [1, 999] {
        let mut record = original.clone();
        record.identity.version = version;
        record.run_id = ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
        fs::write(
            root.path().join("manifest.json"),
            serde_json::to_vec(&manifest(&record)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            inspect(root.path()).unwrap_err().to_string(),
            "input integrity: unsupported run format"
        );
    }
    for pointer in [
        "",
        "/record",
        "/record/identity",
        "/record/identity/inputs/profile",
        "/record/identity/inputs/profile/semantic_parameters",
    ] {
        let mut manifest = manifest(&original);
        manifest
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("future_fixed_field".into(), true.into());
        fs::write(
            root.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let error = inspect(root.path()).unwrap_err().to_string();
        assert!(
            error.contains("unknown field `future_fixed_field`"),
            "{error}"
        );
        assert!(!error.contains("checksum mismatch"));
    }
}
