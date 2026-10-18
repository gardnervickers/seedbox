use seedbox_application::{
    config,
    oci::import_metadata,
    store::{ContentDigest, ContentStore},
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};

fn simulation(image: &str) -> config::Simulation {
    config::parse(&format!("name: image-test\nservices:\n  app:\n    image: {image}\n    entrypoint: [/bin/sh]\n    command: [-c, 'exit 0']\nx-seedbox:\n  version: 1\n  limits: {{simulated_time: 1s, wall_time: 1m}}\n  check: {{service: app}}\n"), Path::new("/fixture/compose.yaml"), &BTreeMap::new()).unwrap()
}

fn blob(raw: &Path, data: &[u8], media: &str) -> serde_json::Value {
    let hash = ContentDigest::of(data);
    fs::write(raw.join(hash.hex()), data).unwrap();
    json!({"digest":format!("sha256:{}",hash.hex()),"size":data.len(),"mediaType":media})
}

#[test]
fn metadata_verifies_platform_descriptors_and_retains_original_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let raw = directory.path();
    let config = blob(
        raw,
        br#"{"os":"linux","architecture":"amd64","config":{"ArgsEscaped":true,"Cmd":["/bin/app"],"Env":["KEEP=1"]}}"#,
        "application/vnd.oci.image.config.v1+json",
    );
    let layer = blob(
        raw,
        b"test layer bytes",
        "application/vnd.oci.image.layer.v1.tar",
    );
    let manifest =
        serde_json::to_vec(&json!({"schemaVersion":2,"config":config,"layers":[layer]})).unwrap();
    fs::write(raw.join("manifest.json"), &manifest).unwrap();
    let store = ContentStore::new(raw.join("store"));
    let service = simulation("example:test").services.remove("app").unwrap();
    let imported = import_metadata(raw, &service, &store).unwrap();
    assert_eq!(imported.manifest, ContentDigest::of(&manifest));
    assert_eq!(
        store.read(&imported.layers[0]).unwrap(),
        b"test layer bytes"
    );
    assert_eq!(imported.launch.argv, ["/bin/sh", "-c", "exit 0"]);
    let layer_path = raw.join(imported.layers[0].hex());
    fs::write(&layer_path, b"corrupt").unwrap();
    assert!(import_metadata(raw, &service, &store)
        .unwrap_err()
        .to_string()
        .contains("descriptor size"));
}

#[test]
fn foreign_platform_and_image_lifecycle_hooks_fail_before_execution() {
    for metadata in [
        json!({"os":"linux","architecture":"arm64","config":{}}),
        json!({"os":"windows","architecture":"amd64","config":{"ArgsEscaped":true}}),
        json!({"os":"linux","architecture":"amd64","config":{"Healthcheck":{"Test":["CMD","false"]}}}),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let raw = directory.path();
        let config = blob(
            raw,
            &serde_json::to_vec(&metadata).unwrap(),
            "application/vnd.oci.image.config.v1+json",
        );
        fs::write(
            raw.join("manifest.json"),
            serde_json::to_vec(&json!({"schemaVersion":2,"config":config,"layers":[]})).unwrap(),
        )
        .unwrap();
        assert!(import_metadata(
            raw,
            &simulation("example:test").services["app"],
            &ContentStore::new(raw.join("store"))
        )
        .is_err());
    }
}
