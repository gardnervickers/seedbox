//! Run manifests are strict: unsupported versions and unknown fixed fields are
//! rejected even when their checksums are recomputed. No guest execution.
use seedbox::{
    orchestration::{inspect, RunRecord},
    store::ContentDigest,
};
use std::{fs, path::Path, process::Command};

#[allow(dead_code)]
mod record_fixture {
    use seedbox as sb;
    include!("../../seedbox-artifacts/tests/support/diff_fixture.rs");

    pub(super) fn record() -> sb::orchestration::RunRecord {
        fixture()
    }
}

fn invoke(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_seedbox"))
        .args(args)
        .current_dir(root)
        .env_clear()
        .env("HOME", root)
        .env("PATH", "")
        .output()
        .unwrap()
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
        let output = invoke(root.path(), &["inspect", ".", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            b"error: input integrity: unsupported run format\n"
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
        let output = invoke(root.path(), &["inspect", ".", "--json"]);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(
            error.contains("unknown field `future_fixed_field`"),
            "{error}"
        );
        assert!(!error.contains("checksum mismatch"));
    }
}
