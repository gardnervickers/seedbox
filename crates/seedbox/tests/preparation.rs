//! Native preparation retains immutable inputs and honors cancellation.
use seedbox::{
    config,
    execution::{CancellationToken, ExecutionProfile, PreparationBackend, WitnessMode},
    oci::{ImageTransport, ImportedImage, RootFilesystem},
    preparation::{OciPreparer, PreparationContext},
    store::{ContentDigest, ContentStore},
    Error,
};
use std::{collections::BTreeMap, fs, path::Path};
fn fixture(root: &Path) -> (OciPreparer, config::Simulation, ContentStore) {
    let kernel = root.join("kernel");
    let engine = root.join("engine");
    let helper = root.join("helper");
    fs::write(&kernel, b"kernel").unwrap();
    fs::write(&engine, b"engine").unwrap();
    let mut header = [0u8; 20];
    header[..6].copy_from_slice(b"\x7fELF\x02\x01");
    header[18] = 62;
    fs::write(&helper, header).unwrap();
    let store = ContentStore::new(root.join("store"));
    let profile = ExecutionProfile {
        backend: "test-only".into(),
        engine: ContentDigest::of(b"engine"),
        witness_mode: WitnessMode::Exits,
        semantic_parameters: BTreeMap::from([("memory_mb".into(), "1024".into())]),
    };
    let preparer = OciPreparer {
        kernel,
        engine,
        init: helper,
        mke2fs: None,
        profile,
        docker: bollard::Docker::connect_with_http(
            "http://127.0.0.1:9",
            10,
            bollard::API_DEFAULT_VERSION,
        )
        .unwrap(),
    };
    let simulation=config::parse("name: preparation-test\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 10\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",&root.join("compose.yaml"),&BTreeMap::new()).unwrap();
    (preparer, simulation, store)
}

#[test]
fn cancelled_preparation_never_imports_images() {
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let token = CancellationToken::default();
    token.cancel();
    let context = PreparationContext::new(root.path().join("evidence"), token).unwrap();
    assert!(matches!(
        preparer.prepare_cancellable(&simulation, &store, &context),
        Err(Error::PreparationCancelled)
    ));
    assert!(!context.directory.join("setup.json").exists());
}
#[test]
fn invalid_profile_fails_before_image_import() {
    let root = tempfile::tempdir().unwrap();
    let (mut preparer, simulation, store) = fixture(root.path());
    preparer.profile.backend = "seedbox-kvm".into();
    assert!(preparer
        .prepare(&simulation, &store)
        .unwrap_err()
        .to_string()
        .contains("unsupported guest_abi"));
}
#[test]
fn preparation_retains_kernel_before_transport_changes_source() {
    struct Transport<'a>(&'a Path);
    impl ImageTransport for Transport<'_> {
        fn import(
            &self,
            service: &config::Service,
            directory: &Path,
            store: &ContentStore,
            _: Option<(&PreparationContext, &str)>,
            _: seedbox::guest::Architecture,
        ) -> seedbox::Result<ImportedImage> {
            fs::create_dir_all(directory)?;
            let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
            let digest = ContentDigest::of(config);
            fs::write(directory.join(digest.hex()), config)?;
            fs::write(
                directory.join("manifest.json"),
                serde_json::to_vec(
                    &serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]}),
                )?,
            )?;
            let image = seedbox::oci::import_metadata(directory, service, store)?;
            let root = directory.join("root");
            fs::create_dir(&root)?;
            fs::write(root.join("app"), b"application")?;
            fs::write(self.0, b"changed")?;
            Ok(ImportedImage {
                image,
                rootfs: RootFilesystem::Directory(root),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (preparer, simulation, store) = fixture(root.path());
    let context =
        PreparationContext::new(root.path().join("evidence"), Default::default()).unwrap();
    let inputs = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport(&preparer.kernel))
        .unwrap();
    assert_eq!(fs::read(&preparer.kernel).unwrap(), b"changed");
    assert_eq!(store.read(&inputs.kernel).unwrap(), b"kernel");
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(context.directory.join("setup.json")).unwrap()).unwrap();
    assert_eq!(
        receipt["kernel"],
        serde_json::to_value(inputs.kernel).unwrap()
    );
}

#[test]
fn preparation_derives_the_epoch_from_image_build_times_unless_compose_sets_it() {
    // Each image config records its own build time; the latest one wins.
    struct Transport;
    impl seedbox::oci::ImageTransport for Transport {
        fn import(
            &self,
            service: &seedbox::config::Service,
            directory: &Path,
            store: &ContentStore,
            _context: Option<(&PreparationContext, &str)>,
            _architecture: seedbox::guest::Architecture,
        ) -> seedbox::Result<seedbox::oci::ImportedImage> {
            fs::create_dir_all(directory)?;
            fs::File::open(directory)?
                .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
            Ok(seedbox::oci::ImportedImage {
                image: seedbox::execution::PreparedImage {
                    manifest: store.put(b"manifest")?,
                    config: store.put(
                        format!(
                            r#"{{"os":"linux","architecture":"amd64","config":{{"Cmd":["/app"]}},"created":"2026-01-01T00:00:00Z","history":[{{"created":"{}"}}]}}"#,
                            match &service.source {
                                config::ImageSource::Registry { reference } if reference == "example/late" => "2026-10-09T04:05:33.5Z",
                                _ => "2026-10-01T00:00:00Z",
                            }
                        )
                        .as_bytes(),
                    )?,
                    layers: vec![],
                    launch: seedbox::image::Launch {
                        argv: vec!["/app".into()],
                        environment: BTreeMap::new(),
                        working_dir: "/".into(),
                        user: "".into(),
                        volumes: vec![],
                        disk_volumes: vec![],
                    },
                },
                rootfs: seedbox::oci::RootFilesystem::Directory(directory.into()),
                auxiliary: BTreeMap::new(),
                provenance: BTreeMap::new(),
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let (preparer, mut simulation, store) = fixture(root.path());
    let mut late = simulation.services["app"].clone();
    late.source = config::ImageSource::Registry {
        reference: "example/late".into(),
    };
    simulation.services.insert("other".into(), late);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let prepared = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport)
        .unwrap();
    assert_eq!(prepared.wall_clock_epoch, Some(1_791_518_733));
    let json = serde_json::to_vec(&prepared).unwrap();
    let retained: seedbox::execution::PreparedInputs = serde_json::from_slice(&json).unwrap();
    assert_eq!(retained, prepared);
    let mut historical = serde_json::to_value(&prepared).unwrap();
    historical
        .as_object_mut()
        .unwrap()
        .remove("wall_clock_epoch");
    let historical: seedbox::execution::PreparedInputs =
        serde_json::from_value(historical).unwrap();
    assert_eq!(historical.wall_clock_epoch, None);
    assert!(!serde_json::to_value(historical)
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("wall_clock_epoch"));
    for requested in [None, Some(86400), Some(1_800_000_000), Some(u32::MAX)] {
        simulation.wall_clock_epoch = requested;
        let label = requested.map_or("derived".into(), |epoch| epoch.to_string());
        let context = PreparationContext::new(
            root.path().join(format!("epoch-{label}")),
            Default::default(),
        )
        .unwrap();
        let first = preparer
            .prepare_with_transport(&simulation, &store, &context, &Transport)
            .unwrap();
        let epoch = requested.unwrap_or(1_791_518_733);
        assert_eq!(first.wall_clock_epoch, Some(epoch));
        assert_eq!(requested.is_none(), first.guests == prepared.guests);
        let second_context = PreparationContext::new(
            root.path().join(format!("epoch-{label}-second")),
            Default::default(),
        )
        .unwrap();
        let second = preparer
            .prepare_with_transport(&simulation, &store, &second_context, &Transport)
            .unwrap();
        assert_eq!(first, second);
        for digest in first.guests.values() {
            let archive = store.read(digest).unwrap();
            let start = archive
                .windows(10)
                .position(|bytes| bytes == b"SBXLAUNCH4")
                .unwrap();
            let mut cursor = start + 10;
            for _ in 0..5 {
                let size =
                    u32::from_le_bytes(archive[cursor..cursor + 4].try_into().unwrap()) as usize;
                cursor += 4 + size;
            }
            cursor += 4; // max_map_count
            assert_eq!(
                u32::from_le_bytes(archive[cursor..cursor + 4].try_into().unwrap()),
                epoch
            );
        }
    }
}
