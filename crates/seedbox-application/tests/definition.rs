//! Definition resolution, boot archive projection and retained checkpoint composition.
use seedbox_application::{
    definition::{capture_start, materialize_start, prepared, resolve},
    execution::{ExecutionProfile, PreparedImage, PreparedInputs},
    image::Launch,
    store::{ContentDigest, ContentStore},
};
use seedbox_spec::definition::*;
use std::collections::BTreeMap;
fn fixture() -> (tempfile::TempDir, ContentStore, Definition, ContentDigest) {
    let temporary = tempfile::tempdir().unwrap();
    let store = ContentStore::new(temporary.path().join("store"));
    let root = temporary.path().join("rootfs");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::write(root.join("bin/test"), b"application").unwrap();
    let init = temporary.path().join("init");
    std::fs::write(&init, b"init binary").unwrap();
    let launch = Launch {
        argv: vec!["/bin/test".into()],
        environment: BTreeMap::from([("MODE".into(), "original".into())]),
        working_dir: "/".into(),
        user: "0:0".into(),
        volumes: vec!["/tmp/data".into()],
        disk_volumes: Vec::new(),
    };
    let configs = BTreeMap::from([("/etc/test.conf".into(), b"preserve me".to_vec())]);
    let recipe = seedbox_application::guest::launch_recipe(
        &launch,
        "10.0.0.1/24",
        "127.0.0.1 localhost\n::1 localhost\n10.0.0.1 oracle\n",
        "oracle",
        &configs,
        86400,
    )
    .unwrap();
    let archive = temporary.path().join("boot.cpio");
    seedbox_application::guest::build_initramfs(&root, &init, &recipe, &archive).unwrap();
    let boot = store.put_file(&archive).unwrap();
    let kernel = store.put(b"kernel").unwrap();
    let engine = store.put(b"engine").unwrap();
    let profile = ExecutionProfile {
        backend: seedbox_application::execution::Backend::Hvf,
        engine,
        witness_mode: seedbox_application::execution::WitnessMode::Exits,
        semantic_parameters: seedbox_application::execution::SemanticParameters::new(1024),
    };
    let configuration = seedbox_application::config::parse_for_architecture(
        "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
        std::path::Path::new("/project/compose.yaml"),
        &Default::default(),
        seedbox_application::guest::Architecture::Arm64,
    )
    .unwrap();
    let inputs = PreparedInputs {
        wall_clock_epoch: Some(86400),
        kernel,
        images: BTreeMap::from([(
            "oracle".into(),
            PreparedImage {
                manifest: boot.clone(),
                config: boot.clone(),
                layers: Vec::new(),
                launch,
            },
        )]),
        placement: BTreeMap::from([("oracle".into(), "oracle".into())]),
        guests: BTreeMap::from([("oracle".into(), boot.clone())]),
        profile,
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    let definition = resolve(&configuration, &inputs, 1, Vec::new()).unwrap();
    (temporary, store, definition, boot)
}
#[test]
fn resolved_launch_is_identical_until_inline_fields_change() {
    let (_temporary, store, mut definition, original) = fixture();
    let prepared_inputs = prepared(&definition, &store).unwrap();
    assert_eq!(prepared_inputs.guests["oracle"], original);
    assert_eq!(
        prepared_inputs.images["oracle"].launch.volumes,
        vec!["/tmp/data"]
    );
    definition.oracle.process.argv.push("--changed".into());
    definition.clock.epoch_seconds += 1;
    definition.simulator.blocks_per_progress_call = std::num::NonZeroU32::new(5000).unwrap();
    let changed = prepared(&definition, &store).unwrap();
    assert_ne!(changed.guests["oracle"], original);
    assert_eq!(
        prepared(&definition, &store).unwrap().guests,
        changed.guests
    );
    let bytes = store.read(&changed.guests["oracle"]).unwrap();
    assert!(bytes
        .windows(b"preserve me".len())
        .any(|v| v == b"preserve me"));
    assert!(bytes.windows(b"--changed".len()).any(|v| v == b"--changed"));
    store.verify(&original).unwrap();
}
#[test]
fn checkpoint_binary_closure_survives_source_deletion_and_preserves_guest_paths() {
    let (temporary, store, mut definition, _) = fixture();
    let owned = temporary.path().join("attempt");
    std::fs::create_dir(&owned).unwrap();
    let binary = owned.join("memory");
    std::fs::write(&binary, b"checkpoint memory").unwrap();
    let manifest = owned.join("checkpoint.json");
    std::fs::write(&manifest,serde_json::to_vec(&serde_json::json!({"initial_launch":{"argv":["/usr/bin/application", "asset:literal-argument"]},"state":{"snapshot":{"artifact":binary,"sha256":ContentDigest::of(b"checkpoint memory").hex()}}})).unwrap()).unwrap();
    definition.start = capture_start(&manifest, &owned, &store).unwrap();
    std::fs::remove_dir_all(&owned).unwrap();
    let restored = materialize_start(&definition, &store, &temporary.path().join("new-attempt"))
        .unwrap()
        .unwrap();
    let value: serde_json::Value =
        serde_json::from_reader(std::fs::File::open(restored).unwrap()).unwrap();
    assert_eq!(value["initial_launch"]["argv"][0], "/usr/bin/application");
    assert_eq!(value["initial_launch"]["argv"][1], "asset:literal-argument");
    assert_eq!(
        std::fs::read(value["state"]["snapshot"]["artifact"].as_str().unwrap()).unwrap(),
        b"checkpoint memory"
    );
}
#[test]
fn unsupported_machine_knobs_fail_explicitly() {
    let (_temporary, store, mut definition, _) = fixture();
    definition.oracle.memory_bytes += 1;
    assert!(prepared(&definition, &store)
        .unwrap_err()
        .to_string()
        .contains("whole number of MiB"));
    definition.oracle.memory_bytes -= 1;
    definition.oracle.volumes.insert(
        "data".into(),
        Volume {
            target: "/var/lib/data".into(),
            size_bytes: 10,
        },
    );
    assert!(prepared(&definition, &store)
        .unwrap_err()
        .to_string()
        .contains("sizeBytes"));
}
