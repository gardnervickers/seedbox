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
        },
        kernel,
        engine,
        init: helper.clone(),
        busybox: helper,
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
        let deadline = Instant::now() + Duration::from_secs(3);
        while fs::read_to_string(&ready)
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .is_none()
        {
            assert!(Instant::now() < deadline);
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
