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
        wall_clock_epoch: None,
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
fn preparation_records_one_epoch_after_import_and_retains_it() {
    struct Transport;
    impl seedbox::oci::ImageTransport for Transport {
        fn import(
            &self,
            _service: &seedbox::config::Service,
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
                        br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#,
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
    let (mut preparer, mut simulation, store) = fixture(root.path());
    simulation
        .services
        .insert("other".into(), simulation.services["app"].clone());
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    };
    let before = now();
    let prepared = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport)
        .unwrap();
    let epoch = u64::from(prepared.wall_clock_epoch.unwrap());
    assert!((before..=now()).contains(&epoch));
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
    for requested in [0, 1_800_000_000, u32::MAX] {
        preparer.wall_clock_epoch = Some(requested);
        let context = PreparationContext::new(
            root.path().join(format!("epoch-{requested}")),
            Default::default(),
        )
        .unwrap();
        let prepared = preparer
            .prepare_with_transport(&simulation, &store, &context, &Transport)
            .unwrap();
        let epoch = u64::from(requested.max(seedbox::guest::MIN_WALL_CLOCK_EPOCH));
        assert_eq!(prepared.wall_clock_epoch, Some(epoch as u32));
        let second_context = PreparationContext::new(
            root.path().join(format!("epoch-{requested}-second")),
            Default::default(),
        )
        .unwrap();
        let second = preparer
            .prepare_with_transport(&simulation, &store, &second_context, &Transport)
            .unwrap();
        assert_eq!(prepared, second);
        for digest in prepared.guests.values() {
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
                u32::from_le_bytes(archive[cursor..cursor + 4].try_into().unwrap()) as u64,
                epoch
            );
        }
    }
}
