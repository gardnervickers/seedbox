//! Finished-image preparation through the public backend, with no host unpacker.
use seedbox::{
    config,
    execution::{CancellationToken, ExecutionProfile, PreparationBackend, WitnessMode},
    oci::{native::NativeOci, GuestPlatform, OciTools},
    preparation::{
        native::{NativePreparer, NativeSource},
        OciPreparer, PreparationContext,
    },
    store::{ContentDigest, ContentStore},
    Error,
};
use std::{collections::BTreeMap, fs, path::Path};

fn fixture(
    root: &Path,
    platform: GuestPlatform,
) -> (NativePreparer, config::Simulation, ContentStore) {
    let kernel = root.join("kernel");
    let engine = root.join("engine");
    let helper = root.join("helper");
    fs::write(&kernel, b"kernel").unwrap();
    fs::write(&engine, b"engine").unwrap();
    let mut header = [0u8; 20];
    header[..6].copy_from_slice(b"\x7fELF\x02\x01");
    header[18] = if platform == GuestPlatform::LinuxAmd64 {
        62
    } else {
        183
    };
    fs::write(&helper, header).unwrap();
    let runtime = OciPreparer {
        tools: OciTools {
            skopeo: root.join("never-skopeo"),
            umoci: root.join("never-umoci"),
            docker: root.join("never-build"),
            build_epoch: Some(0),
        },
        kernel,
        engine,
        init: helper.clone(),
        busybox: helper,
        mke2fs: None,
        profile: ExecutionProfile {
            backend: "test-only".into(),
            engine: ContentDigest::of(b"engine"),
            witness_mode: WitnessMode::Exits,
            semantic_parameters: BTreeMap::from([("memory_mb".into(), "1024".into())]),
        },
    };
    let mut simulation = config::parse("name: native-preparation\nservices:\n  app: {image: placeholder/app}\nx-seedbox:\n  version: 1\n  seed: 10\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n", &root.join("compose.yaml"), &BTreeMap::new()).unwrap();
    simulation.services.get_mut("app").unwrap().platform = platform.as_str().into();
    let store = ContentStore::new(root.join("store"));
    (
        NativePreparer {
            runtime,
            importer: NativeOci::default(),
            platform,
            sources: BTreeMap::new(),
        },
        simulation,
        store,
    )
}

fn layout(root: &Path, platform: GuestPlatform) -> std::path::PathBuf {
    let path = root.join("layout");
    fs::create_dir_all(path.join("blobs/sha256")).unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_ustar();
    header.set_size(7);
    header.set_mode(0o751);
    header.set_uid(4242);
    header.set_gid(4343);
    header.set_mtime(123);
    header.set_cksum();
    tar.append_data(&mut header, "app", &b"payload"[..])
        .unwrap();
    let layer = tar.into_inner().unwrap();
    let layer_digest = blob(&path, &layer);
    let config = serde_json::to_vec(&serde_json::json!({
        "os":"linux", "architecture":platform.architecture(), "config":{"Cmd":["/app"], "Env":["FROM_IMAGE=present"]},
        "rootfs":{"type":"layers", "diff_ids":[format!("sha256:{}", layer_digest.hex())]}
    })).unwrap();
    let config_digest = blob(&path, &config);
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json",
        "config":{"mediaType":"application/vnd.oci.image.config.v1+json", "digest":format!("sha256:{}",config_digest.hex()),"size":config.len()},
        "layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar", "digest":format!("sha256:{}",layer_digest.hex()),"size":layer.len()}]
    })).unwrap();
    let manifest_digest = blob(&path, &manifest);
    fs::write(
        path.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    fs::write(path.join("index.json"), serde_json::to_vec(&serde_json::json!({
        "schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json", "digest":format!("sha256:{}",manifest_digest.hex()),"size":manifest.len(),
        "platform":{"os":"linux","architecture":platform.architecture()}}]
    })).unwrap()).unwrap();
    path
}
fn blob(layout: &Path, bytes: &[u8]) -> ContentDigest {
    let digest = ContentDigest::of(bytes);
    fs::write(layout.join("blobs/sha256").join(digest.hex()), bytes).unwrap();
    digest
}
fn override_layout(preparer: &mut NativePreparer, layout: &Path) {
    preparer.sources.insert(
        "app".into(),
        NativeSource::OciLayout {
            path: layout.into(),
            reference: None,
        },
    );
}

fn cpio(bytes: &[u8]) -> BTreeMap<String, (u32, u32, u32, Vec<u8>)> {
    let mut entries = BTreeMap::new();
    let mut at = 0;
    loop {
        assert_eq!(&bytes[at..at + 6], b"070701");
        let field = |index: usize| {
            u32::from_str_radix(
                std::str::from_utf8(&bytes[at + 6 + 8 * index..at + 14 + 8 * index]).unwrap(),
                16,
            )
            .unwrap()
        };
        let (mode, uid, gid, size, name_size) = (
            field(1),
            field(2),
            field(3),
            field(6) as usize,
            field(11) as usize,
        );
        let name = std::str::from_utf8(&bytes[at + 110..at + 110 + name_size - 1])
            .unwrap()
            .to_owned();
        let start = (at + 110 + name_size).next_multiple_of(4);
        if name == "TRAILER!!!" {
            break;
        }
        entries.insert(name, (mode, uid, gid, bytes[start..start + size].to_vec()));
        at = (start + size).next_multiple_of(4);
    }
    entries
}

#[test]
fn completed_layout_retains_linux_metadata_helpers_configs_and_repeatable_archive() {
    for platform in [GuestPlatform::LinuxAmd64, GuestPlatform::LinuxArm64] {
        let root = tempfile::tempdir().unwrap();
        let (mut preparer, mut simulation, store) = fixture(root.path(), platform);
        let input = layout(root.path(), platform);
        override_layout(&mut preparer, &input);
        let config = root.path().join("config");
        fs::write(&config, b"literal\x00config").unwrap();
        simulation.configs.insert("settings".into(), config);
        simulation
            .services
            .get_mut("app")
            .unwrap()
            .configs
            .push(config::ConfigMount {
                source: "settings".into(),
                target: "/settings".into(),
            });
        let context =
            PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
                .unwrap();
        let first = preparer
            .prepare_cancellable(&simulation, &store, &context)
            .unwrap();
        let second = preparer.prepare(&simulation, &store).unwrap();
        assert_eq!(first, second);
        let archive = store.read(&first.guests["app"]).unwrap();
        let files = cpio(&archive);
        assert_eq!(
            files["rootfs/app"],
            (0o100751, 4242, 4343, b"payload".to_vec())
        );
        assert_eq!(files["init"].3, fs::read(&preparer.runtime.init).unwrap());
        assert!(files["seedbox/launch.bin"]
            .3
            .windows(14)
            .any(|bytes| bytes == b"literal\x00config"));
        assert_eq!(first.placement["app"], "app");
        assert_eq!(first.provenance["guest-platform"], platform.as_str());
        assert_eq!(store.read(&first.kernel).unwrap(), b"kernel");
        assert_eq!(
            store.read(&first.auxiliary["config/settings"]).unwrap(),
            b"literal\x00config"
        );
        let setup: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join("evidence/setup.json")).unwrap())
                .unwrap();
        assert_eq!(
            setup["kernel"],
            serde_json::to_value(&first.kernel).unwrap()
        );
        fs::remove_dir_all(input).unwrap();
        for digest in first.digests() {
            store.verify(digest).unwrap();
        }
        assert!(!root.path().join("evidence/builder.json").exists());
    }
}

#[test]
fn build_source_is_rejected_even_with_finished_image_override_and_epoch() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    override_layout(&mut preparer, &root.path().join("must-not-open"));
    simulation.services.get_mut("app").unwrap().source = config::ImageSource::Build {
        context: root.path().into(),
        dockerfile: root.path().join("Dockerfile"),
    };
    let error = preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string();
    assert!(error.contains("build externally and supply an image:"));
    assert!(!root.path().join("store/sha256").exists());
}

#[test]
fn runtime_admission_precedes_import_and_build_rejection() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    preparer.runtime.profile.backend = "seedbox-kvm".into();
    simulation.services.get_mut("app").unwrap().source = config::ImageSource::Build {
        context: root.path().into(),
        dockerfile: root.path().join("Dockerfile"),
    };
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("host admission proof missing"));
}

#[test]
fn mismatched_helpers_service_platform_and_unknown_override_fail_before_images() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxArm64);
    override_layout(&mut preparer, &root.path().join("must-not-open"));
    simulation.services.get_mut("app").unwrap().platform = "linux/amd64".into();
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("conflicts with guest"));
    simulation.services.get_mut("app").unwrap().platform = "linux/arm64".into();
    preparer
        .sources
        .insert("typo".into(), NativeSource::Registry { reference: None });
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("unknown service typo"));
    preparer.sources.remove("typo");
    let mut helper = fs::read(&preparer.runtime.init).unwrap();
    helper[18] = 62;
    fs::write(&preparer.runtime.init, helper).unwrap();
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("expected an arm64 ELF"));
}

#[test]
fn named_volumes_require_matching_mke2fs_and_package_retained_helper() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxArm64);
    let input = layout(root.path(), GuestPlatform::LinuxArm64);
    override_layout(&mut preparer, &input);
    simulation
        .services
        .get_mut("app")
        .unwrap()
        .volumes
        .push(config::VolumeMount {
            source: "state".into(),
            target: "/data".into(),
        });
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("named volumes need"));
    preparer.runtime.mke2fs = Some(preparer.runtime.init.clone());
    let prepared = preparer.prepare(&simulation, &store).unwrap();
    let files = cpio(&store.read(&prepared.guests["app"]).unwrap());
    assert!(files.contains_key("seedbox/mke2fs"));
    assert!(prepared.auxiliary.contains_key("guest-mke2fs"));
}

#[test]
fn reserved_mount_and_memory_floor_are_enforced() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    let input = layout(root.path(), GuestPlatform::LinuxAmd64);
    override_layout(&mut preparer, &input);
    simulation
        .services
        .get_mut("app")
        .unwrap()
        .volumes
        .push(config::VolumeMount {
            source: "state".into(),
            target: "/dev".into(),
        });
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("reserved guest runtime path"));
    simulation.services.get_mut("app").unwrap().volumes.clear();
    preparer
        .runtime
        .profile
        .semantic_parameters
        .insert("memory_mb".into(), "128".into());
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("archive boot floor"));
}

#[test]
fn cancelled_preparation_does_not_capture_inputs_or_import() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let context = PreparationContext::new(root.path().join("evidence"), cancellation).unwrap();
    assert!(matches!(
        preparer.prepare_cancellable(&simulation, &store, &context),
        Err(Error::PreparationCancelled)
    ));
    assert!(!root.path().join("store/sha256").exists());
}

#[test]
fn local_export_cannot_replace_captured_kernel_or_bootstrap_helpers() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    let input = layout(root.path(), GuestPlatform::LinuxAmd64);
    let archive = root.path().join("saved.tar");
    save_archive(&input, &archive);
    let client = root.path().join("export-client");
    let quote = |path: &Path| format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"));
    fs::write(&client, format!("#!/bin/sh\n[ \"$1\" = image ] && [ \"$2\" = save ] && [ \"$3\" = --output ] || exit 9\nprintf replaced-kernel > {}\nprintf replaced-helper > {}\ncp {} \"$4\"\n", quote(&preparer.runtime.kernel), quote(&preparer.runtime.init), quote(&archive))).unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
    let helper = fs::read(&preparer.runtime.init).unwrap();
    preparer.sources.insert(
        "app".into(),
        NativeSource::Docker {
            client,
            reference: "local/app:finished".into(),
        },
    );
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let prepared = preparer
        .prepare_cancellable(&simulation, &store, &context)
        .unwrap();
    assert_eq!(
        fs::read(&preparer.runtime.kernel).unwrap(),
        b"replaced-kernel"
    );
    assert_eq!(store.read(&prepared.kernel).unwrap(), b"kernel");
    assert_eq!(
        fs::read(&preparer.runtime.init).unwrap(),
        b"replaced-helper"
    );
    assert_eq!(
        store.read(&prepared.auxiliary["guest-init"]).unwrap(),
        helper
    );
    assert_eq!(
        cpio(&store.read(&prepared.guests["app"]).unwrap())["init"].3,
        helper
    );
    assert_eq!(export_receipts(root.path()).len(), 1);
    assert!(!root.path().join("evidence/builder.json").exists());
}

fn save_archive(input: &Path, archive: &Path) {
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(input.join("index.json")).unwrap()).unwrap();
    let manifest_digest = index["manifests"][0]["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(input.join("blobs/sha256").join(manifest_digest)).unwrap(),
    )
    .unwrap();
    let config = manifest["config"]["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let layer = manifest["layers"][0]["digest"]
        .as_str()
        .unwrap()
        .strip_prefix("sha256:")
        .unwrap();
    let mut tar = tar::Builder::new(fs::File::create(archive).unwrap());
    for (name, bytes) in [
        ("config.json", fs::read(input.join("blobs/sha256").join(config)).unwrap()),
        ("layer.tar", fs::read(input.join("blobs/sha256").join(layer)).unwrap()),
        ("manifest.json", serde_json::to_vec(&serde_json::json!([{"Config":"config.json", "RepoTags":["local/app:finished"], "Layers":["layer.tar"]}])).unwrap()),
    ] {
        let mut header = tar::Header::new_ustar(); header.set_size(bytes.len() as u64); header.set_mode(0o644); header.set_cksum();
        tar.append_data(&mut header, name, &bytes[..]).unwrap();
    }
    tar.finish().unwrap();
}

fn export_receipts(root: &Path) -> Vec<(std::path::PathBuf, serde_json::Value)> {
    let mut receipts = fs::read_dir(root.join("evidence"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("local-image-save-")
                && path
                    .extension()
                    .is_some_and(|extension| extension == "json")
        })
        .map(|path| {
            let value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            (path, value)
        })
        .collect::<Vec<_>>();
    receipts.sort_by(|a, b| a.0.cmp(&b.0));
    receipts
}

#[test]
fn multiple_local_exports_keep_distinct_receipts_and_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, mut simulation, store) = fixture(root.path(), GuestPlatform::LinuxAmd64);
    simulation
        .services
        .insert("peer".into(), simulation.services["app"].clone());
    let input = layout(root.path(), GuestPlatform::LinuxAmd64);
    let archive = root.path().join("saved.tar");
    save_archive(&input, &archive);
    let client = root.path().join("export-client");
    let quoted_archive = format!(
        "'{}'",
        archive.to_str().unwrap().replace(char::from(39), "'\\''")
    );
    fs::write(&client, format!("#!/bin/sh\n[ \"$1\" = image ] && [ \"$2\" = save ] && [ \"$3\" = --output ] || exit 9\nprintf 'exported %s\\n' \"$5\"\ncp {} \"$4\"\n", quoted_archive)).unwrap();
    fs::set_permissions(&client, fs::Permissions::from_mode(0o755)).unwrap();
    preparer.sources.insert(
        "app".into(),
        NativeSource::Docker {
            client: client.clone(),
            reference: "local/app:first".into(),
        },
    );
    preparer.sources.insert(
        "peer".into(),
        NativeSource::Podman {
            client,
            reference: "local/peer:second".into(),
        },
    );
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let prepared = preparer
        .prepare_cancellable(&simulation, &store, &context)
        .unwrap();
    assert_eq!(prepared.guests.len(), 2);
    let receipts = export_receipts(root.path());
    assert_eq!(receipts.len(), 2);
    let mut references = Vec::new();
    for (path, receipt) in receipts {
        let reference = receipt["argv"][4].as_str().unwrap();
        references.push(reference.to_owned());
        assert_eq!(
            fs::read(path.with_extension("log")).unwrap(),
            format!("exported {reference}\n").as_bytes()
        );
        assert_eq!(receipt["reaped"], true);
        assert_eq!(receipt["output_complete"], true);
        assert_eq!(
            receipt["phase"].as_str().unwrap(),
            path.file_stem().unwrap().to_str().unwrap()
        );
    }
    references.sort();
    assert_eq!(references, ["local/app:first", "local/peer:second"]);
    assert!(!root.path().join("evidence/local-image-save.json").exists());
}

#[test]
fn legacy_x86_kvm_profile_cannot_prepare_arm64_without_architecture_metadata() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, simulation, store) = fixture(root.path(), GuestPlatform::LinuxArm64);
    preparer.runtime.profile.backend = "seedbox-kvm".into();
    let policy = seedbox::admission::required_policy().unwrap();
    preparer
        .runtime
        .profile
        .semantic_parameters
        .extend(seedbox::admission::bindings().unwrap());
    preparer
        .runtime
        .profile
        .semantic_parameters
        .extend(BTreeMap::from([
            ("guest_abi".into(), policy.guest_abi),
            ("control_channel".into(), policy.control_channel),
            (
                seedbox::admission::ENGINE_QUALIFICATION.into(),
                seedbox::admission::DEV_UNPINNED.into(),
            ),
        ]));
    // The legacy admission check itself accepts these explicit dev artifacts.
    preparer.runtime.validate_runtime_inputs().unwrap();
    assert!(!preparer
        .runtime
        .profile
        .semantic_parameters
        .contains_key("guest_architecture"));
    assert!(!preparer
        .runtime
        .profile
        .semantic_parameters
        .contains_key("guest_platform"));
    let error = preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("seedbox-kvm requires linux/amd64 guests"),
        "{error}"
    );
    assert!(!root.path().join("store/sha256").exists());
}

#[test]
fn native_arm64_authoring_reaches_preparation_without_platform_rewriting_or_building() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, _, store) = fixture(root.path(), GuestPlatform::LinuxArm64);
    let source = root.path().join("compose.yaml");
    let yaml = "name: native-arm64\nservices:\n  app:\n    image: example/app:finished\n    build:\n      context: ./absent-external-context\n      target: externally-built\n      args: {EXTERNAL: '${EXTERNAL}'}\nx-seedbox:\n  version: 1\n  seed: 10\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n";
    let simulation =
        config::parse_native(yaml, &source, &BTreeMap::new(), GuestPlatform::LinuxArm64).unwrap();
    assert_eq!(simulation.services["app"].platform, "linux/arm64");
    assert_eq!(
        simulation.services["app"].source,
        config::ImageSource::Registry {
            reference: "example/app:finished".into()
        }
    );
    let input = layout(root.path(), GuestPlatform::LinuxArm64);
    override_layout(&mut preparer, &input);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let prepared = preparer
        .prepare_cancellable(&simulation, &store, &context)
        .unwrap();
    assert_eq!(prepared.images["app"].launch.argv, ["/app"]);
    let captured_config: serde_json::Value =
        serde_json::from_slice(&store.read(&prepared.images["app"].config).unwrap()).unwrap();
    assert_eq!(captured_config["os"], "linux");
    assert_eq!(captured_config["architecture"], "arm64");
    let captured_index: serde_json::Value = serde_json::from_slice(
        &store
            .read(&prepared.auxiliary["image/app/source-index"])
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        captured_index["manifests"][0]["platform"]["architecture"],
        "arm64"
    );
    assert_eq!(
        captured_index["manifests"][0]["digest"],
        format!("sha256:{}", prepared.images["app"].manifest.hex())
    );
    assert_eq!(prepared.provenance["guest-platform"], "linux/arm64");
    assert!(prepared.guests.contains_key("app"));
    assert!(!root.path().join("absent-external-context").exists());
    assert!(!root.path().join("evidence/builder.json").exists());
    assert!(export_receipts(root.path()).is_empty());
}
