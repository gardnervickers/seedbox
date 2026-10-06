use seedbox::{
    config,
    oci::{import_metadata, OciTools},
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
        br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/bin/app"],"Env":["KEEP=1"]}}"#,
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

#[test]
#[ignore = "requires Linux root, registry access, skopeo and umoci from the Nix shell"]
fn real_registry_images_use_the_verified_import_path() {
    let directory = tempfile::tempdir().unwrap();
    let store = ContentStore::new(directory.path().join("store"));
    for (name, reference, binary) in [
        (
            "redis",
            "docker.io/library/redis:7.2",
            "usr/local/bin/redis-server",
        ),
        ("etcd", "quay.io/coreos/etcd:v3.5.2", "usr/local/bin/etcd"),
    ] {
        let imported = OciTools::default()
            .import(
                &simulation(reference).services["app"],
                &directory.path().join(name),
                &store,
            )
            .unwrap();
        assert!(imported.rootfs.join(binary).is_file());
        assert!(!imported.image.layers.is_empty());
        for hash in imported.image.layers {
            store.verify(&hash).unwrap();
        }
        println!("{name}: sha256:{}", imported.image.manifest.hex());
    }
}

#[test]
#[ignore = "requires Linux root, Podman, skopeo and umoci from the Nix shell"]
fn dockerfile_layers_apply_whiteouts_and_preserve_launch_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let context = directory.path().join("context");
    fs::create_dir(&context).unwrap();
    fs::write(context.join("Dockerfile"), "FROM docker.io/library/busybox:1.37\nCOPY . /source\nRUN test ! -e /source/ignored && mkdir /data && echo old >/data/remove && echo keep >/data/keep\nRUN rm /data/remove && ln -s keep /data/link\nENV MESSAGE=hello\nWORKDIR /data\nUSER 123:456\nENTRYPOINT [\"/bin/echo\"]\nCMD [\"default\"]\n").unwrap();
    fs::write(context.join(".dockerignore"), "ignored\n").unwrap();
    fs::write(context.join("ignored"), "must not reach COPY").unwrap();
    let mut service = simulation("unused:test").services.remove("app").unwrap();
    service.source = config::ImageSource::Build {
        context: context.clone(),
        dockerfile: context.join("Dockerfile"),
    };
    service.entrypoint = None;
    service.command = None;
    let store = ContentStore::new(directory.path().join("store"));
    let imported = OciTools::default()
        .import(&service, &directory.path().join("image"), &store)
        .unwrap();
    assert!(!imported.rootfs.join("data/remove").exists());
    assert!(!imported.rootfs.join("source/ignored").exists());
    assert_eq!(
        fs::read_to_string(imported.rootfs.join("data/link")).unwrap(),
        "keep\n"
    );
    assert_eq!(imported.image.launch.argv, ["/bin/echo", "default"]);
    assert_eq!(imported.image.launch.user, "123:456");
    assert_eq!(imported.image.launch.working_dir, "/data");
    assert_eq!(imported.image.launch.environment["MESSAGE"], "hello");
    assert!(imported.auxiliary.contains_key("build-context"));
}
