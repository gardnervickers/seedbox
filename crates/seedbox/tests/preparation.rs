//! Actual cancellable OCI preparation path with transport executables, no image build.
#![cfg(unix)]
use seedbox::{
    config,
    execution::{CancellationToken, ExecutionProfile, PreparationBackend, WitnessMode},
    oci::OciTools,
    preparation::{OciPreparer, PreparationContext},
    store::{ContentDigest, ContentStore},
    Error,
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
fn script(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    fs::write(
        &path,
        format!("#!/bin/sh\nif [ \"$1\" = --version ]; then echo fixture-v1; exit 0; fi\n{body}\n"),
    )
    .unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}
fn fixture(
    root: &Path,
    skopeo: PathBuf,
    umoci: PathBuf,
) -> (OciPreparer, config::Simulation, ContentStore) {
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
        tools: OciTools {
            skopeo,
            umoci,
            docker: root.join("must-not-run"),
            build_epoch: None,
        },
        kernel,
        engine,
        init: helper.clone(),
        busybox: helper,
        mke2fs: None,
        profile,
    };
    let simulation=config::parse("name: preparation-test\nservices:\n  app: {image: example/app}\nx-seedbox:\n  version: 1\n  seed: 10\n  limits: {simulated_time: 1s, wall_time: 1m}\n  check: {service: app}\n",&root.join("compose.yaml"),&BTreeMap::new()).unwrap();
    (preparer, simulation, store)
}
#[test]
fn failed_copy_retains_raw_phase_evidence_after_preparer_and_scratch_drop() {
    let root = tempfile::tempdir().unwrap();
    let skopeo = script(
        root.path(),
        "skopeo",
        "printf 'copy failed\\377' >&2; exit 7",
    );
    let umoci = script(root.path(), "umoci", "exit 0");
    let (preparer, simulation, store) = fixture(root.path(), skopeo, umoci);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    assert!(matches!(
        preparer.prepare_cancellable(&simulation, &store, &context),
        Err(Error::Execution(_))
    ));
    drop(preparer);
    drop(context);
    assert_eq!(
        fs::read(root.path().join("evidence/app-copy.log")).unwrap(),
        b"copy failed\xff"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("evidence/app-copy.json")).unwrap())
            .unwrap();
    assert!(receipt["status"].as_str().unwrap().contains('7'));
    assert_eq!(receipt["reaped"], true);
    assert_eq!(receipt["output_complete"], true);
}
#[test]
fn unpack_failure_retains_versions_copy_and_unpack_after_scratch_drop() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("raw");
    fs::create_dir(&raw).unwrap();
    let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
    let digest = ContentDigest::of(config);
    fs::write(raw.join(digest.hex()), config).unwrap();
    fs::write(raw.join("manifest.json"),serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]})).unwrap()).unwrap();
    let skopeo=script(root.path(),"skopeo",&format!("for arg do last=$arg; done; case \"$last\" in dir:*) out=${{last#dir:}}; mkdir -p \"$out\"; cp '{}'/manifest.json '{}'/{} \"$out\";; esac; echo transport-opaque",raw.display(),raw.display(),digest.hex()));
    let umoci = script(
        root.path(),
        "umoci",
        "printf 'unpack failure\\376' >&2; exit 9",
    );
    let (preparer, simulation, store) = fixture(root.path(), skopeo, umoci);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    assert!(matches!(
        preparer.prepare_cancellable(&simulation, &store, &context),
        Err(Error::Execution(_))
    ));
    drop(preparer);
    drop(context);
    for phase in [
        "app-skopeo-version",
        "app-umoci-version",
        "app-copy",
        "app-normalize",
        "app-unpack",
    ] {
        assert!(root.path().join(format!("evidence/{phase}.json")).is_file());
        assert!(root.path().join(format!("evidence/{phase}.log")).is_file());
    }
    assert_eq!(
        fs::read(root.path().join("evidence/app-unpack.log")).unwrap(),
        b"unpack failure\xfe"
    );
}
#[test]
fn cancelled_copy_retains_command_and_cleanup_receipts_after_drop() {
    let root = tempfile::tempdir().unwrap();
    let ready = root.path().join("ready");
    let skopeo = script(
        root.path(),
        "skopeo",
        &format!(
            "echo $$ > '{}'; echo raw-cancel-diagnostic; while :; do :; done",
            ready.display()
        ),
    );
    let umoci = script(root.path(), "umoci", "exit 0");
    let (preparer, simulation, store) = fixture(root.path(), skopeo, umoci);
    let token = CancellationToken::default();
    let context = PreparationContext::new(root.path().join("evidence"), token.clone()).unwrap();
    std::thread::scope(|scope| {
        let task = scope.spawn(|| preparer.prepare_cancellable(&simulation, &store, &context));
        let deadline = Instant::now() + Duration::from_secs(30);
        while fs::read_to_string(&ready)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_none()
        {
            let within_deadline = Instant::now() < deadline;
            if !within_deadline {
                token.cancel();
            }
            assert!(within_deadline);
            std::thread::yield_now();
        }
        token.cancel();
        assert!(matches!(
            task.join().unwrap(),
            Err(Error::PreparationCancelled)
        ));
    });
    drop(preparer);
    drop(context);
    assert_eq!(
        fs::read(root.path().join("evidence/app-copy.log")).unwrap(),
        b"raw-cancel-diagnostic\n"
    );
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("evidence/app-copy.json")).unwrap())
            .unwrap();
    assert_eq!(receipt["cancelled"], true);
    assert_eq!(receipt["reaped"], true);
}

#[test]
fn direct_oci_preparation_refuses_invalid_profile_before_builder_admission() {
    let root = tempfile::tempdir().unwrap();
    let sentinel = root.path().join("tool-invoked");
    let tool = script(
        root.path(),
        "sentinel",
        &format!("touch '{}'; exit 9", sentinel.display()),
    );
    let (mut preparer, mut simulation, store) = fixture(root.path(), tool.clone(), tool.clone());
    preparer.tools.docker = tool;
    preparer.profile.backend = "seedbox-kvm".into();
    simulation.services.get_mut("app").unwrap().source = seedbox::config::ImageSource::Build {
        context: root.path().to_path_buf(),
        dockerfile: root.path().join("Dockerfile"),
    };
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    for epoch in [None, Some(0)] {
        preparer.tools.build_epoch = epoch;
        for result in [
            preparer.prepare(&simulation, &store),
            preparer.prepare_cancellable(&simulation, &store, &context),
        ] {
            let e = result.unwrap_err().to_string();
            assert!(e.contains("unsupported guest_abi"), "{e}");
        }
    }
    assert!(!sentinel.exists());
    assert!(!root.path().join("evidence/builder.json").exists());
    assert!(!root.path().join("evidence/setup.json").exists());
}

#[test]
fn preparation_retains_one_frozen_kernel_when_transport_changes_the_source_path() {
    let root = tempfile::tempdir().unwrap();
    let raw = root.path().join("raw");
    fs::create_dir(&raw).unwrap();
    let config = br#"{"os":"linux","architecture":"amd64","config":{"Cmd":["/app"]}}"#;
    let digest = ContentDigest::of(config);
    fs::write(raw.join(digest.hex()), config).unwrap();
    fs::write(
        raw.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion":2,"config":{"mediaType":"application/vnd.oci.image.config.v1+json",
            "digest":format!("sha256:{}",digest.hex()),"size":config.len()},"layers":[]
        }))
        .unwrap(),
    )
    .unwrap();
    // Fake transport/extraction commands, not a builder or guest execution.
    let skopeo = script(root.path(), "skopeo", &format!(
        "for arg do last=$arg; done; case \"$last\" in dir:*) out=${{last#dir:}}; mkdir -p \"$out\"; cp '{}'/manifest.json '{}'/{} \"$out\";; esac",
        raw.display(), raw.display(), digest.hex()));
    let umoci = script(root.path(), "umoci", &format!(
        "for arg do last=$arg; done; mkdir -p \"$last/rootfs\"; printf application > \"$last/rootfs/app\"; printf changed-after-freeze > '{}'",
        root.path().join("kernel").display()));
    let (preparer, simulation, store) = fixture(root.path(), skopeo, umoci);
    let context =
        PreparationContext::new(root.path().join("evidence"), CancellationToken::default())
            .unwrap();
    let inputs = preparer
        .prepare_cancellable(&simulation, &store, &context)
        .unwrap();
    assert_eq!(fs::read(&preparer.kernel).unwrap(), b"changed-after-freeze");
    assert_eq!(inputs.kernel, ContentDigest::of(b"kernel"));
    assert_eq!(store.read(&inputs.kernel).unwrap(), b"kernel");
    let receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(root.path().join("evidence/setup.json")).unwrap())
            .unwrap();
    assert_eq!(
        receipt["kernel"],
        serde_json::to_value(&inputs.kernel).unwrap()
    );
    assert_eq!(inputs.placement["app"], "app");
    assert!(!root.path().join("evidence/builder.json").exists());
}

#[test]
fn preparation_records_one_epoch_after_import_and_retains_explicit_epoch() {
    struct Transport;
    impl seedbox::oci::ImageTransport for Transport {
        fn import(
            &self,
            _service: &seedbox::config::Service,
            directory: &Path,
            _store: &ContentStore,
            _context: Option<(&PreparationContext, &str)>,
            _frozen: Option<&seedbox::oci::FrozenContext>,
            _architecture: seedbox::guest::Architecture,
        ) -> seedbox::Result<seedbox::oci::ImportedImage> {
            fs::create_dir_all(directory)?;
            fs::File::open(directory)?
                .set_times(fs::FileTimes::new().set_modified(std::time::UNIX_EPOCH))?;
            Ok(seedbox::oci::ImportedImage {
                image: seedbox::execution::PreparedImage {
                    manifest: ContentDigest::of(b"manifest"),
                    config: ContentDigest::of(b"config"),
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
    let (mut preparer, mut simulation, store) =
        fixture(root.path(), "unused".into(), "unused".into());
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
    for digest in prepared.guests.values() {
        let archive = store.read(digest).unwrap();
        let start = archive
            .windows(10)
            .position(|bytes| bytes == b"SBXLAUNCH4")
            .unwrap();
        let mut cursor = start + 10;
        for _ in 0..5 {
            let size = u32::from_le_bytes(archive[cursor..cursor + 4].try_into().unwrap()) as usize;
            cursor += 4 + size;
        }
        cursor += 4; // max_map_count
        assert_eq!(
            u32::from_le_bytes(archive[cursor..cursor + 4].try_into().unwrap()) as u64,
            epoch
        );
    }
    preparer.tools.build_epoch = Some(0);
    let explicit = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport)
        .unwrap();
    let again = preparer
        .prepare_with_transport(&simulation, &store, &context, &Transport)
        .unwrap();
    assert_eq!(explicit.wall_clock_epoch, Some(0));
    assert_eq!(explicit, again);
    assert_ne!(prepared.guests, explicit.guests);
}
