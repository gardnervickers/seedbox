//! ISA selection is explicit and retained; native host tools cannot silently
//! choose a different OCI manifest or guest supervisor architecture.
use seedbox::{
    config,
    guest::Architecture,
    store::{ContentDigest, ContentStore},
};
use std::{collections::BTreeMap, fs, path::Path};

const COMPOSE:&str="name: arm-inputs\nservices:\n  app: {image: busybox:1.37.0}\nx-seedbox:\n  version: 1\n  seed: 1\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n";

#[test]
fn arm_default_is_explicit_and_foreign_platform_fails_before_preparation() {
    let arm = config::parse_for_architecture(
        COMPOSE,
        Path::new("/project/compose.yaml"),
        &BTreeMap::new(),
        Architecture::Arm64,
    )
    .unwrap();
    assert_eq!(arm.services["app"].platform, "linux/arm64");
    assert_eq!(
        config::parse(
            COMPOSE,
            Path::new("/project/compose.yaml"),
            &BTreeMap::new()
        )
        .unwrap()
        .services["app"]
            .platform,
        "linux/amd64"
    );
    let foreign = COMPOSE.replace(
        "image: busybox:1.37.0",
        "image: busybox:1.37.0, platform: linux/amd64",
    );
    assert_eq!(
        config::parse_for_architecture(
            &foreign,
            Path::new("/project/compose.yaml"),
            &BTreeMap::new(),
            Architecture::Arm64
        )
        .unwrap_err()
        .to_string(),
        "$.services.app.platform: only linux/arm64 is supported"
    );
}

#[test]
fn platform_manifest_is_verified_against_the_requested_guest_isa() {
    let directory = tempfile::tempdir().unwrap();
    let raw = directory.path().join("raw");
    fs::create_dir(&raw).unwrap();
    let config_bytes = br#"{"os":"linux","architecture":"arm64","config":{"Cmd":["/bin/sh"]}}"#;
    let digest = ContentDigest::of(config_bytes);
    fs::write(raw.join(digest.hex()), config_bytes).unwrap();
    let manifest=serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config_bytes.len()},"layers":[]})).unwrap();
    fs::write(raw.join("manifest.json"), &manifest).unwrap();
    let simulation = config::parse_for_architecture(
        COMPOSE,
        Path::new("/project/compose.yaml"),
        &BTreeMap::new(),
        Architecture::Arm64,
    )
    .unwrap();
    let service = &simulation.services["app"];
    let store = ContentStore::new(directory.path().join("store"));
    let image =
        seedbox::oci::import_metadata_for_arch(&raw, service, &store, Architecture::Arm64).unwrap();
    assert_eq!(image.manifest, ContentDigest::of(&manifest));
    assert_eq!(image.config, digest);
    assert_eq!(
        seedbox::oci::import_metadata(&raw, service, &store)
            .unwrap_err()
            .to_string(),
        "input integrity: image must be linux/amd64"
    );
}
