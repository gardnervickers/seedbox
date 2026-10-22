//! Private adapter tests: artifact conversion and cold native snapshot admission.
//! No QEMU process, guest execution or privileged setup.
use super::*;
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
mod pv_policy;
use seedbox_engine_protocol::driver_world as wire;
use std::{collections::BTreeMap, num::NonZeroU32};

#[test]
fn tcg_codec_three_policy_and_name_refuse_before_native_reconstruction() {
    use seedbox_machine::{
        definition::BackendPolicy,
        test_support::{assets, factory_definition},
    };
    let root = tempfile::tempdir().unwrap();
    for (name, bytes) in [
        ("kernel", assets::bzimage_header()),
        ("init", assets::image(62)),
        ("command-line", b"console=ttyS0".to_vec()),
        ("firmware", vec![1; 64 * 1024]),
    ] {
        std::fs::write(root.path().join(name), bytes).unwrap();
    }
    let definition = factory_definition::definition(0);
    let policy = BackendPolicy::default();
    let mut factory =
        tcg::Factory::new(&std::env::current_exe().unwrap(), root.path(), 946_684_800)
            .unwrap()
            .with_policy(policy.clone())
            .unwrap();
    let mut machine = factory.create(&definition).unwrap();
    let snapshot = machine.snapshot().unwrap();
    let mut encoded = vec![];
    snapshot.write_to(&mut encoded).unwrap();
    assert_eq!(&encoded[..8], b"SBXTCG01");
    assert_eq!(&encoded[8..12], &3u32.to_le_bytes());
    let missing = root.path().join("must-not-be-created");
    let artifact = wire::Artifact {
        sha256: "00".repeat(32),
        length: 0,
        artifact: missing.to_str().unwrap().into(),
    };
    let mut inventory = wire::Inventory::Tcg {
        policy: Some(policy.clone()),
        executable: artifact.clone(),
        kernel: artifact.clone(),
        init: artifact.clone(),
        command_line: artifact.clone(),
        firmware: artifact.clone(),
        runtime: artifact,
        formatter: None,
        environments: BTreeMap::new(),
        boot_archives: BTreeMap::new(),
        epoch_seconds: 946_684_800,
    };
    let limits = seedbox_machine::snapshot::DecodeLimits {
        encoded_bytes: 16 << 20,
        allocation_bytes: 64 << 20,
        collection_items: 1 << 20,
        total_items: 1 << 20,
        string_bytes: 1 << 20,
        blob_bytes: 16 << 20,
    };
    for version in [2u32, 4u32] {
        let mut refused = encoded.clone();
        refused[8..12].copy_from_slice(&version.to_le_bytes());
        assert!(
            Snapshot::read_from(&inventory, &mut std::io::Cursor::new(refused), &limits).is_err()
        );
    }
    let snapshot =
        Snapshot::read_from(&inventory, &mut std::io::Cursor::new(encoded), &limits).unwrap();
    let error = Native::restore(
        "wrong-name".into(),
        &inventory,
        &missing,
        &snapshot,
        &limits,
        &missing,
    )
    .err()
    .expect("closed name must refuse");
    assert!(error.contains("name"), "{error}");
    let mut invalid = inventory.clone();
    let wire::Inventory::Tcg { policy, .. } = &mut invalid else {
        unreachable!()
    };
    *policy = Some(wire::BackendPolicy {
        stop_tolerance_ns: u64::MAX,
        ..Default::default()
    });
    let error = Native::restore(
        definition.name.clone(),
        &invalid,
        &missing,
        &snapshot,
        &limits,
        &missing,
    )
    .err()
    .expect("invalid wire policy must fail before native reconstruction");
    assert!(error.contains("stop_tolerance_ns"), "{error}");
    let error = Snapshot::read_from(&invalid, &mut std::io::empty(), &limits)
        .err()
        .expect("native policy validation must precede decoding");
    assert!(error.contains("stop_tolerance_ns"), "{error}");
    let wire::Inventory::Tcg { policy, .. } = &mut inventory else {
        unreachable!()
    };
    *policy = None;
    let error = Native::restore(
        definition.name,
        &inventory,
        &missing,
        &snapshot,
        &limits,
        &missing,
    )
    .err()
    .expect("None is not explicit default");
    assert!(error.contains("policy"), "{error}");
    assert!(
        !missing.exists(),
        "refusal precedes scratch authority or executable access"
    );
    let wire::Inventory::Tcg { policy, .. } = &mut inventory else {
        unreachable!()
    };
    *policy = Some(wire::BackendPolicy {
        controller: Some("unknown-controller".into()),
        ..Default::default()
    });
    let artifact = wire::Artifact {
        sha256: "00".repeat(32),
        length: 0,
        artifact: missing.to_str().unwrap().into(),
    };
    let output = root.path().join("refused-output");
    let request = wire::Request {
        prepared: wire::Prepared {
            inventory,
            oracle: wire::Machine {
                name: "oracle".into(),
                memory_bytes: 256 << 20,
                filesystem: artifact,
                process: wire::Process {
                    argv: vec!["/program".into()],
                    environment: BTreeMap::new(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
                disk: None,
                interface: None,
            },
            machines: vec![],
            spawnable: vec![],
            quantum_ns: 1,
            network: Default::default(),
        },
        runtime_binding: None,
        seed: 0,
        program: vec![],
        simulated_time_ns: 0,
        output: output.clone(),
        machine_workers: 1,
        start_paused: false,
        full_state: false,
        host_control: missing,
        observations: None,
        resume_checkpoint: None,
    };
    let error = crate::driver_world::build::open(&request)
        .err()
        .expect("unknown controller must refuse before any worker construction")
        .to_string();
    assert!(error.contains("controller"), "{error}");
    assert!(
        !output.exists(),
        "unknown controller is refused before filesystem/native effects"
    );
}
fn setup(configs: BTreeMap<String, wire::Artifact>) -> wire::Environment {
    wire::Environment {
        hostname: "node".into(),
        hosts: "127.0.0.1 localhost\n10.0.0.1 node\n".into(),
        anonymous_volumes: vec!["/anonymous".into()],
        named_volumes: vec![wire::NamedVolume {
            name: "data".into(),
            target: "/data".into(),
        }],
        configs,
        limits: wire::GuestLimits {
            max_map_count: NonZeroU32::new(262_144).unwrap(),
            nofile: NonZeroU32::new(1_048_576).unwrap(),
        },
        control_socket: true,
    }
}
#[test]
fn binary_config_refs_are_not_limited_by_the_metadata_envelope_budget() {
    let root = tempfile::tempdir().unwrap();
    let mut refs = BTreeMap::new();
    for (name, byte) in [("first", 0), ("second", 0xff)] {
        let data = vec![byte; 16 << 20];
        let path = root.path().join(name);
        std::fs::write(&path, &data).unwrap();
        refs.insert(
            format!("/configs/{name}"),
            wire::Artifact {
                sha256: crate::store::ContentDigest::of(&data).hex().into(),
                length: data.len() as u64,
                artifact: path.to_str().unwrap().into(),
            },
        );
    }
    let retained = setup(refs);
    assert!(serde_json::to_vec(&retained).unwrap().len() < 4096);
    let actual = environment(&retained).unwrap();
    std::fs::remove_dir_all(root.path()).unwrap();
    assert_eq!(actual.hostname, retained.hostname);
    assert_eq!(actual.hosts, retained.hosts);
    assert_eq!(actual.anonymous_volumes, ["/anonymous"]);
    assert_eq!(actual.named_volumes[0].name, "data");
    assert_eq!(actual.named_volumes[0].target, "/data");
    assert_eq!(actual.limits.max_map_count.get(), 262_144);
    assert_eq!(actual.limits.nofile.get(), 1_048_576);
    assert!(actual.control_socket);
    for (name, byte) in [("first", 0), ("second", 0xff)] {
        let data = &actual.configs[&format!("/configs/{name}")];
        assert_eq!(data.len(), 16 << 20);
        assert!(data.iter().all(|value| *value == byte));
    }
}
#[test]
fn changed_config_is_rejected_at_artifact_conversion_before_native_creation() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("changed");
    std::fs::write(&path, b"changed").unwrap();
    let retained = setup(BTreeMap::from([(
        "/configs/a".into(),
        wire::Artifact {
            sha256: crate::store::ContentDigest::of(b"original").hex().into(),
            length: 8,
            artifact: path.to_str().unwrap().into(),
        },
    )]));
    assert!(environment(&retained)
        .unwrap_err()
        .contains("Driver artifact"));
}
