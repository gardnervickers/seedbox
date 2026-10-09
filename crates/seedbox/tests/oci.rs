use seedbox::{
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

#[cfg(all(target_os = "linux", feature = "oci-tests"))]
mod engine {
    use super::*;
    use seedbox::{
        container_images::ContainerTransport,
        execution::CancellationToken,
        guest::Architecture,
        oci::{ImageTransport, ImportedImage, RootFilesystem},
    };
    fn import(service: &config::Service, directory: &Path, store: &ContentStore) -> ImportedImage {
        ContainerTransport {
            docker: bollard::Docker::connect_with_defaults().unwrap(),
            cancellation: CancellationToken::default(),
            progress: |line| eprintln!("{line}"),
        }
        .import(service, directory, store, None, Architecture::Amd64)
        .unwrap()
    }
    fn entries(image: &ImportedImage) -> BTreeMap<std::path::PathBuf, (tar::EntryType, Vec<u8>)> {
        use std::io::Read;
        let RootFilesystem::Archive(path) = &image.rootfs else {
            panic!("expected export archive")
        };
        tar::Archive::new(fs::File::open(path).unwrap())
            .entries()
            .unwrap()
            .map(|entry| {
                let mut entry = entry.unwrap();
                let path = entry.path().unwrap().into_owned();
                let kind = entry.header().entry_type();
                let mut data = Vec::new();
                entry.read_to_end(&mut data).unwrap();
                (path, (kind, data))
            })
            .collect()
    }
    #[test]
    fn registry_images_export_the_expected_files() {
        let directory = tempfile::tempdir().unwrap();
        let store = ContentStore::new(directory.path().join("store"));
        for (name, reference, binary) in [
            (
                "redis",
                "docker.io/library/redis:7.2",
                "rootfs/usr/local/bin/redis-server",
            ),
            (
                "etcd",
                "quay.io/coreos/etcd:v3.5.2",
                "rootfs/usr/local/bin/etcd",
            ),
        ] {
            let image = import(
                &simulation(reference).services["app"],
                &directory.path().join(name),
                &store,
            );
            assert!(entries(&image).contains_key(Path::new(binary)));
            for digest in image.image.layers {
                store.verify(&digest).unwrap();
            }
        }
    }
    #[test]
    fn build_honors_ignore_whiteouts_and_launch_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let context = directory.path().join("context");
        fs::create_dir(&context).unwrap();
        fs::write(context.join("Dockerfile"), "FROM docker.io/library/alpine:3.22\nCOPY . /source\nRUN test ! -e /source/ignored && mkdir /data && echo old >/data/remove && echo keep >/data/keep\nRUN rm /data/remove && ln -s keep /data/link\nENV MESSAGE=hello\nWORKDIR /data\nUSER 123:456\nENTRYPOINT [\"/bin/echo\"]\nCMD [\"default\"]\n").unwrap();
        fs::write(context.join(".dockerignore"), "ignored\n").unwrap();
        fs::write(context.join("ignored"), "must not upload").unwrap();
        let mut service = simulation("unused:test").services.remove("app").unwrap();
        service.source = config::ImageSource::Build {
            context: context.clone(),
            dockerfile: context.join("Dockerfile"),
        };
        service.entrypoint = None;
        service.command = None;
        let store = ContentStore::new(directory.path().join("store"));
        let image = import(&service, &directory.path().join("image"), &store);
        let files = entries(&image);
        assert!(!files.contains_key(Path::new("rootfs/data/remove")));
        assert!(!files.contains_key(Path::new("rootfs/source/ignored")));
        assert_eq!(files[Path::new("rootfs/data/keep")].1, b"keep\n");
        assert!(files[Path::new("rootfs/data/link")].0.is_symlink());
        assert_eq!(image.image.launch.argv, ["/bin/echo", "default"]);
        assert_eq!(image.image.launch.user, "123:456");
        assert_eq!(image.image.launch.working_dir, "/data");
        assert_eq!(image.image.launch.environment["MESSAGE"], "hello");
    }
}
