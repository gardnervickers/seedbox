//! Frozen writer fixtures exercise readers; no guest execution is implied.
use seedbox::{
    bundle::{self, Options},
    orchestration::{inspect, RunRecord},
    store::{ContentDigest, ContentStore},
};
use std::{fs, path::Path, process::Command};

const DEMO: &[u8] = include_bytes!("fixtures/formats/demo41-fail-v2.json");
const SYNTHETIC: &[u8] = include_bytes!("fixtures/formats/synthetic-c84-v2.json");
const BUNDLE: &[u8] = include_bytes!("fixtures/formats/synthetic-c84-v1.seedbox.tar");

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

#[test]
fn frozen_writers_inspect_as_exact_json_without_tools_or_execution() {
    for (manifest, expected) in [
        (
            DEMO,
            include_bytes!("fixtures/formats/demo41-inspect-c84.json").as_slice(),
        ),
        (
            SYNTHETIC,
            include_bytes!("fixtures/formats/synthetic-c84-inspect.json").as_slice(),
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("manifest.json"), manifest).unwrap();
        let output = invoke(root.path(), &["inspect", ".", "--json"]);
        assert_eq!(output.status.code(), Some(0), "{:?}", output.stderr);
        assert!(output.stderr.is_empty());
        assert_eq!(output.stdout, expected);
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["identity"]["version"], 2);
        assert_eq!(record["outcome"], "FAIL");
        assert!(record["report"]["full_state"].is_null());
        assert!(record.get("sha256").is_none());
        assert_eq!(
            fs::read(root.path().join("manifest.json")).unwrap(),
            manifest
        );
    }
}

#[test]
fn recomputed_valid_unsupported_versions_and_unknown_fixed_fields_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("manifest.json"), SYNTHETIC).unwrap();
    let original = inspect(root.path()).unwrap();
    for version in [1, 999] {
        let mut record = original.clone();
        record.identity.version = version;
        record.run_id = ContentDigest::of(&serde_json::to_vec(&record.identity).unwrap());
        let manifest = serde_json::json!({
            "sha256": ContentDigest::of(&serde_json::to_vec(&record).unwrap()),
            "record": record,
        });
        fs::write(
            root.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
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
    ] {
        let mut manifest: serde_json::Value = serde_json::from_slice(SYNTHETIC).unwrap();
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

#[test]
fn frozen_bundle_preserves_original_manifest_and_opaque_bytes_through_transfer() {
    let root = tempfile::tempdir().unwrap();
    let archive = root.path().join("frozen.tar");
    fs::write(&archive, BUNDLE).unwrap();
    let state = root.path().join("state");
    let imported = bundle::import(&archive, &state, &Options::default()).unwrap();
    assert_eq!(
        fs::read(imported.directory.join("manifest.json")).unwrap(),
        SYNTHETIC
    );
    assert_eq!(
        fs::read(imported.directory.join("output/opaque.bin")).unwrap(),
        [0, 255, 13, 10, 128, 0, 42]
    );
    let store = ContentStore::new(state.join("objects"));
    let second = root.path().join("second.tar");
    bundle::export(&imported.directory, &store, &second, &Options::default()).unwrap();
    let final_run =
        bundle::import(&second, &root.path().join("fresh"), &Options::default()).unwrap();
    assert_eq!(
        fs::read(final_run.directory.join("manifest.json")).unwrap(),
        SYNTHETIC
    );
    assert_eq!(final_run.record, imported.record);
    assert_eq!(
        fs::read(final_run.directory.join("output/opaque.bin")).unwrap(),
        [0, 255, 13, 10, 128, 0, 42]
    );
    let _: RunRecord = inspect(&final_run.directory).unwrap();
}

#[test]
fn frozen_bundle_unknown_envelope_field_is_rejected_before_state_publication() {
    let root = tempfile::tempdir().unwrap();
    let mut archive = tar::Archive::new(BUNDLE);
    let mut changed = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut changed);
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let mut header = entry.header().clone();
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
            if entry.path().unwrap().as_ref() == Path::new("bundle.json") {
                let mut envelope: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                envelope["future_fixed_field"] = true.into();
                bytes = serde_json::to_vec(&envelope).unwrap();
                header.set_size(bytes.len() as u64);
                header.set_cksum();
            }
            builder.append(&header, bytes.as_slice()).unwrap();
        }
        builder.finish().unwrap();
    }
    let path = root.path().join("changed.tar");
    fs::write(&path, changed).unwrap();
    let state = root.path().join("unpublished");
    let error = bundle::import(&path, &state, &Options::default())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("unknown field `future_fixed_field`"),
        "{error}"
    );
    assert!(!state.exists());
}
