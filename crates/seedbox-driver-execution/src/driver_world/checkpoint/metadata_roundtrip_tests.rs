//! Writer/read resource budgets must agree without a new semantic handle cap.
use super::*;

#[test]
fn tcg_policy_option_survives_envelope_and_descriptor_roundtrip_without_defaulting() {
    use wire::BackendPolicy;
    for selected in [
        None,
        Some(BackendPolicy::default()),
        Some(BackendPolicy {
            seed: 73,
            timer_jitter_ns: 127,
            controller: Some("oracle".into()),
            ..Default::default()
        }),
    ] {
        let root = tempfile::tempdir().unwrap();
        let pv = fixture_inventory(&root.path().join("authority"));
        let mut value = serde_json::to_value(&pv).unwrap();
        value["backend"] = "tcg".into();
        value["firmware"] = value["kernel"].clone();
        value.as_object_mut().unwrap().remove("progress");
        value
            .as_object_mut()
            .unwrap()
            .remove("blocks_per_progress_call");
        value["policy"] = serde_json::to_value(&selected).unwrap();
        let inventory: wire::Inventory = serde_json::from_value(value).unwrap();
        assert_eq!(inventory.tcg_policy(), selected.as_ref());
        if selected.is_none() {
            let mut older = serde_json::to_value(&inventory).unwrap();
            older.as_object_mut().unwrap().remove("policy");
            let older: wire::Inventory = serde_json::from_value(older).unwrap();
            assert!(older.tcg_policy().is_none());
        }
        let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
        let mut state = state(1);
        let path = capture(
            &mut driver,
            Capture {
                inventory: &inventory,
                state: &mut state,
                prepared: false,
            },
            root.path(),
            "policy-option",
            publish,
            encode,
        )
        .unwrap();
        let envelope = read(&path).unwrap();
        assert_eq!(envelope.version, wire::ENVELOPE_VERSION);
        assert_eq!(envelope.inventory.tcg_policy(), selected.as_ref());
        super::super::open(&envelope, limits()).unwrap();
        crate::driver_world::build::validate_policy_binding(&inventory, &envelope.inventory)
            .unwrap();
        let mut opposite = inventory.clone();
        let wire::Inventory::Tcg { policy, .. } = &mut opposite else {
            unreachable!()
        };
        *policy = if selected.is_none() {
            Some(BackendPolicy::default())
        } else {
            None
        };
        assert!(crate::driver_world::build::validate_policy_binding(
            &opposite,
            &envelope.inventory
        )
        .unwrap_err()
        .to_string()
        .contains("None is not Some(default)"));
    }
}
#[test]
fn pv_optional_policy_roundtrips_envelope_and_driver_metadata_without_defaulting() {
    for selected in [
        None,
        Some(wire::BackendPolicy::default()),
        Some(wire::BackendPolicy {
            seed: 73,
            timer_jitter_ns: 127,
            seeded_pv_speed: true,
            controller: Some("oracle".into()),
            ..Default::default()
        }),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut inventory = fixture_inventory(&root.path().join("authority"));
        let wire::Inventory::Pv { policy, .. } = &mut inventory else {
            unreachable!()
        };
        *policy = selected.clone();
        let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
        let mut state = state(1);
        let path = capture(
            &mut driver,
            Capture {
                inventory: &inventory,
                state: &mut state,
                prepared: false,
            },
            root.path(),
            "pv-policy",
            publish,
            encode,
        )
        .unwrap();
        let envelope = read(&path).unwrap();
        assert_eq!(envelope.inventory.backend_policy(), selected.as_ref());
        super::super::open(&envelope, limits()).unwrap();
        crate::driver_world::build::validate_policy_binding(&inventory, &envelope.inventory)
            .unwrap();
        let mut opposite = inventory.clone();
        let wire::Inventory::Pv { policy, .. } = &mut opposite else {
            unreachable!()
        };
        *policy = if selected.is_none() {
            Some(wire::BackendPolicy::default())
        } else {
            None
        };
        assert!(crate::driver_world::build::validate_policy_binding(
            &opposite,
            &envelope.inventory
        )
        .is_err());
        if selected.is_none() {
            let mut old = serde_json::to_value(&inventory).unwrap();
            old.as_object_mut().unwrap().remove("policy");
            let old: wire::Inventory = serde_json::from_value(old).unwrap();
            assert!(old.backend_policy().is_none());
        }
    }
}
#[test]
fn complete_boot_factory_authority_survives_checkpoint_source_deletion() {
    let root = tempfile::tempdir().unwrap();
    let authority = root.path().join("authority");
    let mut inventory = fixture_inventory(&authority);
    let source = authority.join("complete-boot-fixture");
    let bytes = b"synthetic opaque complete-boot authority; no native execution";
    fs::write(&source, bytes).unwrap();
    let artifact = describe(&source, &source).unwrap();
    let wire::Inventory::Pv { boot_archives, .. } = &mut inventory else {
        unreachable!()
    };
    boot_archives.insert("future-machine".into(), artifact.clone());
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        root.path(),
        "complete-boot",
        publish,
        encode,
    )
    .unwrap();
    fs::remove_dir_all(&authority).unwrap();
    let envelope = read(&path).unwrap();
    let wire::Inventory::Pv { boot_archives, .. } = &envelope.inventory else {
        unreachable!()
    };
    let retained = &boot_archives["future-machine"];
    assert_eq!(retained.sha256, artifact.sha256);
    assert_ne!(retained.artifact, artifact.artifact);
    assert_eq!(fs::read(&retained.artifact).unwrap(), bytes);
    super::super::open(&envelope, limits()).unwrap();
}
#[test]
fn distinct_runtime_closure_bytes_are_retained_and_exported_independently() {
    let root = tempfile::tempdir().unwrap();
    let authority = root.path().join("authority");
    let mut inventory = fixture_inventory(&authority);
    let source = authority.join("distinct-runtime");
    let contents = b"distinct complete runtime closure fixture";
    fs::write(&source, contents).unwrap();
    let reference = describe(&source, &source).unwrap();
    let wire::Inventory::Pv { runtime, .. } = &mut inventory else {
        unreachable!()
    };
    *runtime = reference.clone();
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 1, Arc::default(), Arc::default(), None);
    let mut state = state(1);
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "runtime-closure",
        publish,
        encode,
    )
    .unwrap();
    let store = crate::store::ContentStore::new(root.path().join("store"));
    let start = seedbox_artifacts::checkpoint::capture_start(&path, &output, &store).unwrap();
    let seedbox_spec::definition::Start::Checkpoint { manifest, assets } = start else {
        panic!("checkpoint expected")
    };
    assert_eq!(
        assets.len(),
        8,
        "six distinct factory bodies plus native and Driver metadata"
    );
    assert_eq!(
        manifest
            .pointer("/inventory/runtime/sha256")
            .unwrap()
            .as_str(),
        Some(reference.sha256.as_str())
    );
    fs::remove_dir_all(output).unwrap();
    fs::remove_dir_all(authority).unwrap();
    let digest = crate::store::ContentDigest::try_from(reference.sha256).unwrap();
    store.verify(&digest).unwrap();
    assert_eq!(store.read(&digest).unwrap(), contents);
}

#[test]
fn config_artifact_map_above_64_entries_keeps_large_contents_outside_metadata() {
    let root = tempfile::tempdir().unwrap();
    let authority = root.path().join("authority");
    let mut inventory = fixture_inventory(&authority);
    let data = vec![0x5a; 65_537];
    let source = authority.join("config");
    fs::write(&source, &data).unwrap();
    let reference = describe(&source, &source).unwrap();
    let wire::Inventory::Pv { environments, .. } = &mut inventory else {
        unreachable!()
    };
    environments.insert(
        "oracle".into(),
        wire::Environment {
            hostname: "oracle".into(),
            hosts: "127.0.0.1 localhost\n".into(),
            anonymous_volumes: vec![],
            named_volumes: vec![],
            configs: (0..80)
                .map(|index| (format!("/config-{index}"), reference.clone()))
                .collect(),
            limits: wire::GuestLimits {
                max_map_count: std::num::NonZeroU32::new(262_144).unwrap(),
                nofile: std::num::NonZeroU32::new(1_048_576).unwrap(),
            },
            control_socket: true,
        },
    );
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "config-map",
        publish,
        encode,
    )
    .unwrap();
    fs::remove_dir_all(authority).unwrap();
    let restored = read(&path).unwrap();
    let wire::Inventory::Pv { environments, .. } = restored.inventory else {
        unreachable!()
    };
    let configs = &environments["oracle"].configs;
    assert_eq!(configs.len(), 80);
    assert!(
        fs::metadata(&path).unwrap().len() < 128 * 1024,
        "config contents must remain artifact refs, not byte arrays"
    );
    for config in configs.values() {
        assert_eq!(config.length, data.len() as u64);
        assert_eq!(fs::read(&config.artifact).unwrap(), data);
        verified(config).unwrap();
    }
}
fn future(handle: String) -> seedbox_engine_protocol::input_program::TimedInput {
    seedbox_engine_protocol::input_program::TimedInput {
        key: "future".into(),
        at_ns: 1000,
        timeout_ns: 10,
        action: seedbox_control::Action::Partition {
            handle,
            left: vec!["oracle".into()],
            right: vec!["m1".into()],
        },
    }
}
#[test]
fn valid_future_named_handle_above_64k_captures_and_reads_exactly() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    let input = future("x".repeat(65_537));
    state.world.input_program.inputs.push(input.clone());
    state.world.validate().unwrap();
    let path = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "long-handle",
        publish,
        encode,
    )
    .unwrap();
    let restored = read(&path).unwrap();
    assert_eq!(restored.state.world.input_program.inputs, [input]);
    assert_eq!(restored.state.world.input_program.next, 0);
    let seedbox_control::Action::Partition { handle, .. } =
        &restored.state.world.input_program.inputs[0].action
    else {
        panic!("expected partition")
    };
    assert_eq!(handle.len(), 65_537);
}
#[test]
fn envelope_exceeding_whole_resource_budget_is_rejected_before_manifest_publication() {
    let root = tempfile::tempdir().unwrap();
    let inventory = fixture_inventory(&root.path().join("authority"));
    let output = root.path().join("output");
    fs::create_dir(&output).unwrap();
    let mut driver = driver(1, 2, Arc::default(), Arc::default(), None);
    let mut state = state(2);
    state
        .world
        .input_program
        .inputs
        .push(future("x".repeat(wire::MAX_ENVELOPE_BYTES as usize)));
    state.world.validate().unwrap();
    let error = capture(
        &mut driver,
        Capture {
            inventory: &inventory,
            state: &mut state,
            prepared: false,
        },
        &output,
        "too-large",
        publish,
        encode,
    )
    .unwrap_err();
    assert!(error.to_string().contains("service envelope exceeds limit"));
    assert_eq!(
        fs::read_dir(output).unwrap().count(),
        0,
        "no acknowledged or partial published checkpoint"
    );
}
