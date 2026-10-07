use seedbox::{
    config,
    oci::{import_metadata, import_metadata_for_platform, GuestPlatform, OciTools},
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
fn selected_guest_platform_controls_validation_and_preserves_artifact_bytes() {
    let service = simulation("example:test").services.remove("app").unwrap();
    for (architecture, platform) in [
        ("amd64", GuestPlatform::LinuxAmd64),
        ("arm64", GuestPlatform::LinuxArm64),
    ] {
        for os in ["linux", "windows"] {
            let directory = tempfile::tempdir().unwrap();
            let raw = directory.path();
            let config_bytes = serde_json::to_vec(&json!({
                "os": os, "architecture": architecture,
                "config": {"Cmd": ["/app"], "User": "123:456"}
            }))
            .unwrap();
            let config = blob(
                raw,
                &config_bytes,
                "application/vnd.oci.image.config.v1+json",
            );
            let layer = blob(
                raw,
                b"original layer",
                "application/vnd.oci.image.layer.v1.tar",
            );
            let manifest = serde_json::to_vec(&json!({
                "schemaVersion": 2, "config": config, "layers": [layer]
            }))
            .unwrap();
            fs::write(raw.join("manifest.json"), &manifest).unwrap();
            let store = ContentStore::new(raw.join("store"));

            for requested in [GuestPlatform::LinuxAmd64, GuestPlatform::LinuxArm64] {
                let result = import_metadata_for_platform(raw, &service, &store, requested);
                if os != "linux" || requested != platform {
                    assert_eq!(
                        result.unwrap_err().to_string(),
                        format!("input integrity: image must be {}", requested.as_str())
                    );
                    continue;
                }
                let imported = result.unwrap();
                assert_eq!(store.read(&imported.manifest).unwrap(), manifest);
                assert_eq!(store.read(&imported.config).unwrap(), config_bytes);
                assert_eq!(store.read(&imported.layers[0]).unwrap(), b"original layer");
                assert_eq!(imported.launch.user, "123:456");
                if requested == GuestPlatform::LinuxAmd64 {
                    assert_eq!(imported, import_metadata(raw, &service, &store).unwrap());
                }
                // Same size, wrong content: choosing ARM64 must not weaken
                // descriptor verification or trust an existing cached blob.
                fs::write(raw.join(imported.layers[0].hex()), b"modified layer").unwrap();
                assert!(
                    import_metadata_for_platform(raw, &service, &store, requested)
                        .unwrap_err()
                        .to_string()
                        .contains("descriptor hash mismatch")
                );
            }
        }
    }
}

#[test]
fn guest_platform_is_forwarded_to_registry_copy_and_dockerfile_build() {
    use std::os::unix::fs::PermissionsExt;

    for platform in [GuestPlatform::LinuxAmd64, GuestPlatform::LinuxArm64] {
        for build in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let write_tool = |name: &str| {
                let path = root.join(name);
                // Process fixtures stop at the first operation and record its
                // arguments. They neither build nor pull an actual image.
                fs::write(
                    &path,
                    concat!(
                        "#!/bin/sh\n",
                        "if [ \"$1\" = --version ]; then echo fixture-v1; exit 0; fi\n",
                        "if [ \"$1\" = image ]; then exit 0; fi\n",
                        "printf '%s\\n' \"$@\" > \"$0.argv\"\n",
                        "exit 7\n",
                    ),
                )
                .unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let tools = OciTools {
                skopeo: write_tool("skopeo"),
                umoci: write_tool("umoci"),
                docker: write_tool("builder"),
                ..OciTools::default()
            };
            let mut service = simulation("example:test").services.remove("app").unwrap();
            if build {
                let context = root.join("context");
                fs::create_dir(&context).unwrap();
                let dockerfile = context.join("Dockerfile");
                fs::write(&dockerfile, "FROM scratch\n").unwrap();
                service.source = config::ImageSource::Build {
                    context,
                    dockerfile,
                };
            }
            let error = tools
                .import_for_platform(
                    &service,
                    &root.join("import"),
                    &ContentStore::new(root.join("store")),
                    platform,
                )
                .err()
                .unwrap();
            assert!(error.to_string().contains('7'));
            let args =
                fs::read_to_string(root.join(if build { "builder.argv" } else { "skopeo.argv" }))
                    .unwrap();
            let args: Vec<_> = args.lines().collect();
            if build {
                assert_eq!(&args[..3], ["build", "--platform", platform.as_str()]);
            } else {
                assert_eq!(
                    &args[..7],
                    [
                        "--insecure-policy",
                        "--override-os",
                        "linux",
                        "--override-arch",
                        platform.architecture(),
                        "copy",
                        "--preserve-digests",
                    ]
                );
            }
            assert!(!root.join("umoci.argv").exists());
        }
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
