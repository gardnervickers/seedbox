//! Projection between portable definitions and the private prepared engine ABI.
use crate::{
    execution::{ExecutionProfile, PreparedImage, PreparedInputs},
    image::Launch,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_spec::{definition::*, Simulation};
use std::collections::BTreeMap;
fn asset(d: &ContentDigest) -> Asset {
    Asset(format!("sha256:{}", d.hex()))
}
fn digest(a: &Asset) -> Result<ContentDigest> {
    ContentDigest::try_from(
        a.0.strip_prefix("sha256:")
            .ok_or_else(|| Error::Integrity("invalid asset reference".into()))?
            .to_owned(),
    )
}

pub fn resolve(
    configuration: &Simulation,
    inputs: &PreparedInputs,
    seed: u64,
    overrides: Vec<Override>,
) -> Result<Definition> {
    let oracle_name = &configuration.check.service;
    let machine = |name: &String, boot: &ContentDigest| -> Result<Machine> {
        let image = inputs
            .images
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("missing resolved launch for {name}")))?;
        Ok(Machine {
            image: Some(Image {
                tag: configuration
                    .services
                    .get(name)
                    .and_then(|service| match &service.source {
                        seedbox_spec::ImageSource::Registry { reference } => {
                            Some(reference.clone())
                        }
                        _ => None,
                    }),
                digest: format!("sha256:{}", image.manifest.hex()),
            }),
            anonymous_volumes: image.launch.volumes.clone(),
            standby: configuration
                .services
                .get(name)
                .is_some_and(|service| service.standby),
            name: (name == oracle_name).then(|| name.clone()),
            boot_archive: asset(boot),
            memory_bytes: u64::from(inputs.memory_mb(name)?) * 1024 * 1024,
            process: Process {
                argv: image.launch.argv.clone(),
                environment: image.launch.environment.clone(),
                working_directory: image.launch.working_dir.clone(),
                user: image.launch.user.clone(),
            },
            volumes: image
                .launch
                .disk_volumes
                .iter()
                .map(|v| {
                    (
                        v.source.clone(),
                        Volume {
                            target: v.target.clone(),
                            size_bytes: crate::execution::VOLUME_DISK_MIB * 1024 * 1024,
                        },
                    )
                })
                .collect(),
        })
    };
    let mut machines = BTreeMap::new();
    for (name, boot) in &inputs.guests {
        machines.insert(name.clone(), machine(name, boot)?);
    }
    let oracle = machines
        .remove(oracle_name)
        .ok_or_else(|| Error::Integrity("prepared inputs omit oracle".into()))?;
    Ok(Definition {
        architecture: match crate::guest::Architecture::from_profile(&inputs.profile)? {
            crate::guest::Architecture::Amd64 => Architecture::X86_64,
            crate::guest::Architecture::Arm64 => Architecture::Aarch64,
        },
        seed,
        clock: Clock {
            epoch_seconds: inputs
                .wall_clock_epoch
                .ok_or_else(|| Error::Integrity("prepared clock was not resolved".into()))?,
        },
        runtime: Runtime {
            execution: execution_settings_with_inputs(inputs)?,
        },
        machines,
        oracle,
        network: configuration.network.clone(),
        simulator: Simulator {
            progress_budget: configuration.simulator.progress_budget,
            quantum_ns: configuration.simulator.quantum_ns,
            schedule: configuration.schedule.clone(),
            disk: configuration.disk.clone(),
            oracle_control: configuration.control.is_some(),
        },
        start: Start::Fresh,
        overrides,
    })
}

pub fn prepared(definition: &Definition, store: &ContentStore) -> Result<PreparedInputs> {
    definition.runtime.driver_backend().map_err(Error::Setup)?;
    definition
        .start
        .validate_overrides(&definition.overrides)
        .map_err(Error::Setup)?;
    let mut settings = definition.runtime.execution.clone();
    let settings = settings
        .as_object_mut()
        .ok_or_else(|| Error::Integrity("runtime.execution must be an inline object".into()))?;
    let driver_closure: Option<seedbox_driver_execution::driver_world::retained::Closure> =
        settings
            .remove("driver_world")
            .map(serde_json::from_value)
            .transpose()?;
    if let Some(parameters) = settings
        .get_mut("semantic_parameters")
        .and_then(|v| v.as_object_mut())
    {
        parameters.insert(
            "torn_writes".into(),
            definition.simulator.disk.torn_writes.into(),
        );
    }
    let profile: ExecutionProfile =
        serde_json::from_value(serde_json::Value::Object(settings.clone()))?;
    if profile.semantic_parameters.launch() == Some(crate::engine::ExecutableKind::DriverWorld) {
        let descriptor = profile
            .semantic_parameters
            .engine
            .as_ref()
            .ok_or_else(|| Error::Integrity("Driver execution descriptor missing".into()))?;
        if descriptor.guest_arch
            != seedbox_driver_execution::driver_world::platform::architecture_name(
                definition.architecture,
            )
        {
            return Err(Error::Setup(
                "architecture differs from runtime execution guest ISA".into(),
            ));
        }
        if let Start::Checkpoint { manifest, .. } = &definition.start {
            seedbox_driver_execution::driver_world::preflight::restore_manifest(
                manifest,
                definition
                    .simulator
                    .oracle_control
                    .then(|| definition.oracle_name()),
            )?;
        }
    }
    if definition.oracle.memory_bytes == 0
        || !definition.oracle.memory_bytes.is_multiple_of(1024 * 1024)
    {
        return Err(Error::Setup(
            "memoryBytes must be a positive whole number of MiB".into(),
        ));
    }
    let mut images = BTreeMap::new();
    let mut guests = BTreeMap::new();
    let mut placement = BTreeMap::new();
    let mut machine_memory_mb = BTreeMap::new();
    for (name, machine) in definition
        .machines
        .iter()
        .map(|(n, m)| (n.as_str(), m))
        .chain(std::iter::once((
            definition.oracle_name(),
            &definition.oracle,
        )))
    {
        if machine.memory_bytes == 0 || !machine.memory_bytes.is_multiple_of(1024 * 1024) {
            return Err(Error::Setup(format!(
                "machine {name}: memoryBytes must be a positive whole number of MiB"
            )));
        }
        let memory_mb = u32::try_from(machine.memory_bytes / (1024 * 1024)).map_err(|_| {
            Error::Setup(format!(
                "machine {name}: memoryBytes exceeds supported memory"
            ))
        })?;
        crate::preparation::validate_machine_memory(&profile, name, memory_mb)?;
        machine_memory_mb.insert(name.into(), memory_mb);
        for volume in machine.volumes.values() {
            if volume.size_bytes != crate::execution::VOLUME_DISK_MIB * 1024 * 1024 {
                return Err(Error::Setup(
                    "volume sizeBytes must equal the supported 1 GiB disk size".into(),
                ));
            }
        }
        let boot = digest(&machine.boot_archive)?;
        images.insert(
            name.into(),
            PreparedImage {
                manifest: boot.clone(),
                config: boot.clone(),
                layers: Vec::new(),
                launch: Launch {
                    argv: machine.process.argv.clone(),
                    environment: machine.process.environment.clone(),
                    working_dir: machine.process.working_directory.clone(),
                    user: machine.process.user.clone(),
                    volumes: machine.anonymous_volumes.clone(),
                    disk_volumes: machine
                        .volumes
                        .iter()
                        .map(|(n, v)| seedbox_spec::VolumeMount {
                            source: n.clone(),
                            target: v.target.clone(),
                        })
                        .collect(),
                },
            },
        );
        guests.insert(name.into(), boot);
        placement.insert(name.into(), name.into());
    }
    if driver_closure.is_some()
        && profile.semantic_parameters.launch() != Some(crate::engine::ExecutableKind::DriverWorld)
    {
        return Err(Error::Integrity(
            "Driver input closure has a different executable contract".into(),
        ));
    }
    let auxiliary = driver_closure
        .map(|closure| closure.into_auxiliary())
        .transpose()?
        .unwrap_or_default();
    let inputs = PreparedInputs {
        machine_memory_mb,
        wall_clock_epoch: Some(definition.clock.epoch_seconds),
        images,
        placement,
        guests,
        profile,
        auxiliary,
        provenance: BTreeMap::new(),
    };
    for reference in inputs.digests() {
        store.verify(reference)?;
    }
    Ok(inputs)
}

/// Capture a backend checkpoint's binary files while retaining its structured state inline.
pub use seedbox_artifacts::checkpoint::{capture_start, capture_start_observed};

pub fn materialize_start(
    definition: &Definition,
    store: &ContentStore,
    directory: &std::path::Path,
) -> Result<Option<std::path::PathBuf>> {
    definition.runtime.driver_backend().map_err(Error::Setup)?;
    let Start::Checkpoint { manifest, assets } = &definition.start else {
        return Ok(None);
    };
    if manifest.get("contract").and_then(|value| value.as_str())
        == Some(seedbox_engine_protocol::driver_world::CONTRACT)
    {
        seedbox_driver_execution::driver_world::preflight::restore_manifest(
            manifest,
            definition
                .simulator
                .oracle_control
                .then(|| definition.oracle_name()),
        )?;
    }
    fn validate_references(value: &serde_json::Value) -> Result<()> {
        match value {
            serde_json::Value::Object(values) => {
                if values.get("sha256").and_then(|v| v.as_str()).is_some() {
                    for key in ["artifact", "path"] {
                        if let Some(path) = values.get(key).and_then(|v| v.as_str()) {
                            if !path.starts_with("asset:") {
                                return Err(Error::Integrity("checkpoint binary must reference a captured asset, not a host path".into()));
                            }
                        }
                    }
                }
                for value in values.values() {
                    validate_references(value)?;
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    validate_references(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    validate_references(manifest)?;
    let mut manifest = manifest.clone();
    let mut paths = BTreeMap::new();
    for (name, reference) in assets {
        // Names are logical keys; never use them as host paths.
        let destination = directory
            .join("checkpoint-assets")
            .join(paths.len().to_string());
        let file = store.materialize(&digest(reference)?, &destination)?;
        paths.insert(
            format!("asset:{name}"),
            std::path::absolute(file)?.to_string_lossy().into_owned(),
        );
    }
    fn visit(value: &mut serde_json::Value, paths: &BTreeMap<String, String>) -> Result<()> {
        match value {
            serde_json::Value::Array(values) => {
                for v in values {
                    visit(v, paths)?;
                }
            }
            serde_json::Value::Object(values) => {
                if values.get("sha256").and_then(|v| v.as_str()).is_some() {
                    for key in ["artifact", "path"] {
                        if let Some(serde_json::Value::String(text)) = values.get_mut(key) {
                            *text = paths
                                .get(text)
                                .ok_or_else(|| {
                                    Error::Integrity(format!("missing checkpoint binary {text}"))
                                })?
                                .clone();
                        }
                    }
                }
                for v in values.values_mut() {
                    visit(v, paths)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut manifest, &paths)?;
    let path = directory.join("checkpoint.json");
    std::fs::write(&path, serde_json::to_vec(&manifest)?)?;
    Ok(Some(path))
}

/// Inline settings omit values already defined by canonical binary or machine fields.
/// Inline the retained launch profile and Driver's explicit binary input closure.
/// Host paths and source provenance are never retained as factory authority.
// Inlined because the server round-trips preparation through Definition before
// execution; a separate auxiliary manifest would be lost in that step.
pub fn execution_settings_with_inputs(inputs: &PreparedInputs) -> Result<serde_json::Value> {
    let mut settings = execution_settings(&inputs.profile)?;
    if let Some(closure) =
        seedbox_driver_execution::driver_world::retained::Closure::from_inputs(inputs)?
    {
        settings
            .as_object_mut()
            .ok_or_else(|| Error::Integrity("runtime execution settings are not an object".into()))?
            .insert("driver_world".into(), serde_json::to_value(closure)?);
    }
    Ok(settings)
}

pub fn execution_settings(profile: &ExecutionProfile) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(profile)?;
    if let Some(parameters) = value
        .get_mut("semantic_parameters")
        .and_then(|v| v.as_object_mut())
    {
        parameters.remove("torn_writes");
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_protocol::driver_world as wire;
    fn resume_manifest(policy: Option<wire::BackendPolicy>) -> serde_json::Value {
        let artifact = wire::Artifact {
            sha256: "00".repeat(32),
            length: 0,
            artifact: "asset:body".into(),
        };
        serde_json::to_value(wire::Checkpoint {
            contract: wire::CONTRACT.into(),
            version: wire::ENVELOPE_VERSION,
            inventory: wire::Inventory::Pv {
                policy,
                executable: artifact.clone(),
                kernel: artifact.clone(),
                init: artifact.clone(),
                command_line: artifact.clone(),
                progress: artifact.clone(),
                runtime: artifact.clone(),
                formatter: None,
                environments: Default::default(),
                boot_archives: Default::default(),
                progress_budget: std::num::NonZeroU32::new(32).unwrap().into(),
                epoch_seconds: 86400,
            },
            driver: artifact.clone(),
            machines: vec![artifact],
            state: wire::State {
                world: wire::World {
                    seed: 123,
                    machine_names: vec!["oracle".into()],
                    input_program: Default::default(),
                    intervention_next: 0,
                    readiness: wire::Readiness {
                        initial: vec!["oracle".into()],
                        seen: vec!["oracle".into()],
                        latched: true,
                    },
                    publication: Default::default(),
                    position: seedbox_engine_protocol::host_control::BoundaryPosition {
                        boundary_index: 1,
                        completed_rounds: 1,
                        simulated_time_ns: 1_000_000,
                    },
                },
            },
        })
        .unwrap()
    }

    fn fixture() -> (tempfile::TempDir, ContentStore, Definition, ContentDigest) {
        let temporary = tempfile::tempdir().unwrap();
        let store = ContentStore::new(temporary.path().join("store"));
        let boot = store
            .put(b"opaque already-prepared unit-fixture boot bytes")
            .unwrap();
        let profile = ExecutionProfile {
            backend: "fixture".into(),
            witness_mode: crate::execution::WitnessMode::Exits,
            semantic_parameters: crate::execution::SemanticParameters::new(),
        };
        let configuration = crate::config::parse(
            "name: test\nmachines: {}\noracle:\n  image: example/oracle\n",
            std::path::Path::new("/project/seedbox.yaml"),
            &Default::default(),
        )
        .unwrap();
        let inputs = PreparedInputs {
            machine_memory_mb: BTreeMap::from([("oracle".into(), 1024)]),
            wall_clock_epoch: Some(86400),
            images: BTreeMap::from([(
                "oracle".into(),
                PreparedImage {
                    manifest: boot.clone(),
                    config: boot.clone(),
                    layers: vec![],
                    launch: Launch {
                        argv: vec!["/bin/test".into()],
                        environment: BTreeMap::from([("MODE".into(), "original".into())]),
                        working_dir: "/".into(),
                        user: "0:0".into(),
                        volumes: vec!["/tmp/data".into()],
                        disk_volumes: vec![],
                    },
                },
            )]),
            placement: BTreeMap::from([("oracle".into(), "oracle".into())]),
            guests: BTreeMap::from([("oracle".into(), boot.clone())]),
            profile,
            auxiliary: BTreeMap::new(),
            provenance: BTreeMap::new(),
        };
        let definition = resolve(&configuration, &inputs, 1, vec![]).unwrap();
        (temporary, store, definition, boot)
    }

    #[test]
    fn backend_authority_rejects_before_resolved_asset_materialization() {
        for backend in ["driver-unknown", "driver-invalid"] {
            let (root, store, mut definition, _) = fixture();
            definition.runtime.execution["backend"] = backend.into();
            let error = prepared(&definition, &store).err().unwrap().to_string();
            assert!(error.contains("unknown Driver backend"), "{error}");
            assert!(definition
                .configuration("")
                .unwrap_err()
                .contains("unknown Driver backend"));
            let output = root.path().join("not-materialized");
            assert!(materialize_start(&definition, &store, &output)
                .err()
                .unwrap()
                .to_string()
                .contains("unknown Driver backend"));
            assert!(!output.exists());
        }
    }

    #[test]
    fn identity_has_no_placeholder_or_duplicate_runtime_and_memory_fields() {
        let (_root, store, definition, _) = fixture();
        let mut inputs = prepared(&definition, &store).unwrap();
        let runtime = serde_json::to_value(&definition.runtime).unwrap();
        assert_eq!(runtime.as_object().unwrap().len(), 1);
        assert!(runtime.get("execution").is_some());
        let identity = serde_json::to_value(&inputs).unwrap();
        assert!(identity.get("kernel").is_none());
        assert!(identity["profile"].get("engine").is_none());
        for obsolete in ["guest_abi", "control_channel", "memory_mb"] {
            assert!(identity["profile"]["semantic_parameters"]
                .get(obsolete)
                .is_none());
        }
        inputs.machine_memory_mb.clear();
        assert!(resolve(
            &definition.configuration("missing-memory").unwrap(),
            &inputs,
            1,
            vec![]
        )
        .unwrap_err()
        .to_string()
        .contains("prepared memory missing for machine oracle"));
    }

    #[test]
    fn mixed_memory_roundtrips_through_saved_definitions_and_prepared_inputs() {
        let (_temporary, store, mut definition, _) = fixture();
        let mut app = definition.oracle.clone();
        app.name = None;
        app.memory_bytes = 2048 * 1024 * 1024;
        definition.machines.insert("app".into(), app);
        definition.oracle.memory_bytes = 256 * 1024 * 1024;
        let saved: Definition =
            serde_yaml_ng::from_str(&serde_yaml_ng::to_string(&definition).unwrap()).unwrap();
        let inputs = prepared(&saved, &store).unwrap();
        assert_eq!(inputs.memory_mb("app").unwrap(), 2048);
        assert_eq!(inputs.memory_mb("oracle").unwrap(), 256);
        assert_eq!(
            inputs.machine_memory_mb,
            BTreeMap::from([("app".into(), 2048), ("oracle".into(), 256)])
        );
        let configuration = saved.configuration("mixed").unwrap();
        assert_eq!(configuration.services["app"].memory_mb, Some(2048));
        assert_eq!(configuration.services["oracle"].memory_mb, Some(256));
        let resolved =
            resolve(&configuration, &inputs, saved.seed, saved.overrides.clone()).unwrap();
        assert_eq!(
            resolved.machines["app"].memory_bytes,
            saved.machines["app"].memory_bytes
        );
        assert_eq!(resolved.oracle.memory_bytes, saved.oracle.memory_bytes);
        for memory in [0, 64 * 1024 * 1024 + 1, 65537 * 1024 * 1024] {
            definition.machines.get_mut("app").unwrap().memory_bytes = memory;
            assert!(prepared(&definition, &store).is_err(), "{memory}");
        }
    }
    #[test]
    fn unsupported_driver_resume_refuses_before_any_start_asset_materialization() {
        let (root, store, mut definition, _) = fixture();
        let policy = seedbox_engine_protocol::driver_world::BackendPolicy {
            timer_jitter_ns: u64::MAX,
            ..Default::default()
        };
        let manifest = resume_manifest(Some(policy));
        let expected =
            seedbox_driver_execution::driver_world::preflight::restore_manifest(&manifest, None)
                .unwrap_err()
                .to_string();
        definition.start = Start::Checkpoint {
            manifest,
            assets: BTreeMap::from([("body".into(), Asset(format!("sha256:{}", "11".repeat(32))))]),
        };
        let mut semantic = crate::execution::SemanticParameters::new();
        semantic.engine =
            Some(seedbox_driver_execution::driver_world::DriverExecutor::descriptor("x86_64"));
        let profile = ExecutionProfile {
            backend: "driver-pv".into(),
            witness_mode: crate::execution::WitnessMode::Exits,
            semantic_parameters: semantic,
        };
        definition.runtime.execution = serde_json::to_value(profile).unwrap();
        definition.architecture = Architecture::X86_64;
        assert_eq!(
            prepared(&definition, &store).unwrap_err().to_string(),
            expected
        );
        let output = root.path().join("not-materialized");
        assert_eq!(
            materialize_start(&definition, &store, &output)
                .unwrap_err()
                .to_string(),
            expected
        );
        assert!(!output.exists());
    }

    #[test]
    fn supported_non_controller_driver_resume_metadata_still_materializes() {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            return;
        }
        let (root, store, mut definition, body) = fixture();
        definition.start = Start::Checkpoint {
            manifest: resume_manifest(None),
            assets: BTreeMap::from([("body".into(), asset(&body))]),
        };
        let output = root.path().join("allowed");
        assert!(materialize_start(&definition, &store, &output)
            .unwrap()
            .unwrap()
            .is_file());
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
        let restored =
            materialize_start(&definition, &store, &temporary.path().join("new-attempt"))
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
    fn driver_closure_survives_definition_roundtrip_and_actual_launch_request_adapts_inline_edits()
    {
        use seedbox_driver_execution::driver_world::DriverExecutor;
        use seedbox_engine_protocol::driver_world as wire;
        let (temporary, store, mut definition, _) = fixture();
        let mut inputs = prepared(&definition, &store).unwrap();
        inputs.auxiliary.insert(
            "driver-world/executable".into(),
            store.put(b"engine").unwrap(),
        );
        inputs.profile.backend = "driver-pv".into();
        inputs.profile.semantic_parameters.engine = Some(DriverExecutor::descriptor("aarch64"));
        // Fixture bytes exercise retention/launch adaptation only. They are not
        // native boot inputs and this test never spawns an executable or guest.
        let directory = temporary.path().join("driver-fixture");
        std::fs::create_dir(&directory).unwrap();
        let artifact = |name: &str, bytes: &[u8]| {
            let digest = store.put(bytes).unwrap();
            let path = store.materialize(&digest, &directory.join(name)).unwrap();
            wire::Artifact {
                sha256: digest.hex().into(),
                length: bytes.len() as u64,
                artifact: path.to_str().unwrap().into(),
            }
        };
        let filesystem = artifact(
            "rootfs",
            &tar::Builder::new(Vec::new()).into_inner().unwrap(),
        );
        let rootfs = ContentDigest::try_from(filesystem.sha256.clone()).unwrap();
        inputs.guests.insert("oracle".into(), rootfs.clone());
        let prepared_world = wire::Prepared {
            inventory: wire::Inventory::Pv {
                // An explicit retained false is configuration, not an invitation
                // for service adaptation to enable speed diversity.
                policy: Some(wire::BackendPolicy {
                    seeded_pv_speed: false,
                    ..Default::default()
                }),
                executable: artifact("worker", b"fixture executable"),
                kernel: artifact("kernel", b"fixture kernel"),
                init: artifact("init", b"fixture init"),
                command_line: artifact("command-line", b"fixture command line"),
                progress: artifact("progress", b"fixture progress"),
                runtime: artifact("runtime", b"fixture runtime"),
                formatter: None,
                environments: Default::default(),
                boot_archives: Default::default(),
                progress_budget: std::num::NonZeroU32::new(32).unwrap().into(),
                epoch_seconds: 86400,
            },
            oracle: wire::Machine {
                name: "oracle".into(),
                memory_bytes: 1024 << 20,
                filesystem,
                process: wire::Process {
                    argv: vec!["/old".into()],
                    environment: Default::default(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
                disk: None,
                interface: None,
            },
            machines: vec![],
            spawnable: vec![],
            quantum_ns: 1000,
            network: wire::Network {
                latency_ns: 1000,
                jitter_ns: 0,
                loss_ppm: 0,
                ..Default::default()
            },
        };
        seedbox_driver_execution::driver_world::retain_prepared(
            &mut inputs,
            &prepared_world,
            &store,
        )
        .unwrap();
        definition.runtime.execution = execution_settings_with_inputs(&inputs).unwrap();
        definition.oracle.boot_archive = asset(&rootfs);
        assert!(prepared(&definition, &store)
            .unwrap_err()
            .to_string()
            .contains("architecture differs"));
        definition.architecture = Architecture::Aarch64;
        let roundtrip = prepared(&definition, &store).unwrap();
        assert_eq!(roundtrip.auxiliary, inputs.auxiliary);
        assert_eq!(
            roundtrip.executable().unwrap(),
            &ContentDigest::of(b"engine")
        );
        assert!(roundtrip
            .profile
            .semantic_parameters
            .engine
            .as_ref()
            .is_some_and(|descriptor| serde_json::to_value(descriptor)
                .unwrap()
                .get("engine_backend")
                .is_none()));
        assert_eq!(roundtrip.guests["oracle"], rootfs);
        assert_eq!(
            seedbox_driver_execution::driver_world::runtime_digest(&roundtrip, &store).unwrap(),
            ContentDigest::of(b"fixture runtime")
        );
        let mut missing_runtime = roundtrip.clone();
        missing_runtime.auxiliary.retain(|key, digest| {
            !key.starts_with("driver-world/object/")
                || digest != &ContentDigest::of(b"fixture runtime")
        });
        assert!(
            seedbox_driver_execution::driver_world::runtime_digest(&missing_runtime, &store)
                .unwrap_err()
                .to_string()
                .contains("retained input closure")
        );
        let retained_manifest = store
            .read(&roundtrip.auxiliary["driver-world/manifest"])
            .unwrap();
        assert!(!String::from_utf8(retained_manifest)
            .unwrap()
            .contains(directory.to_str().unwrap()));
        definition.oracle.process.argv.push("--changed".into());
        definition.clock.epoch_seconds += 1;
        let edited = prepared(&definition, &store).unwrap();
        let settings = execution_settings_with_inputs(&edited).unwrap();
        assert!(settings.get("driver_world").is_some());
        let simulation = definition.configuration("").unwrap();
        let mut artifacts = BTreeMap::new();
        for (index, digest) in edited.digests().enumerate() {
            artifacts.entry(digest.clone()).or_insert_with(|| {
                store
                    .materialize(
                        digest,
                        &temporary
                            .path()
                            .join("materialized")
                            .join(index.to_string()),
                    )
                    .unwrap()
            });
        }
        let output = temporary.path().join("worker-output");
        std::fs::create_dir(&output).unwrap();
        let cancellation = crate::execution::CancellationToken::default();
        let performance = BTreeMap::from([("SEEDBOX_MACHINE_WORKERS".into(), "4".into())]);
        let mut simulation = simulation;
        simulation.simulator.quantum_ns = std::num::NonZeroU64::new(2_500_000).unwrap();
        let request = crate::execution::ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            observations: None,
            seed: 42,
            program: &[],
            quantum_ns: simulation.simulator.quantum_ns,
            limits: &simulation.limits,
            network: &simulation.network,
            schedule: &simulation.schedule,
            control: simulation.control.as_ref(),
            check: &simulation.check,
            prepared: &edited,
            artifacts: &artifacts,
            output: &output,
            cancellation: &cancellation,
            performance_parameters: &performance,
        };
        let worker = seedbox_driver_execution::driver_world::retained::adapt_request(
            &request,
            &output.join("control"),
        )
        .unwrap();
        assert_eq!(
            worker.prepared.oracle.process.argv,
            definition.oracle.process.argv
        );
        assert!(worker.start_paused);
        assert_eq!(worker.machine_workers, 4);
        assert_eq!(worker.prepared.quantum_ns, 2_500_000);
        let wire::Inventory::Pv {
            epoch_seconds,
            policy,
            ..
        } = worker.prepared.inventory
        else {
            panic!("expected PV fixture")
        };
        assert_eq!(epoch_seconds, definition.clock.epoch_seconds);
        let policy = policy.expect("service adaptation retains resolved policy authority");
        assert!(!policy.seeded_pv_speed, "explicit false remains disabled");
        assert!(
            !policy.boot_end_evidence,
            "service does not request eager boot-end reports"
        );

        // Audit regression: adapt the actual retained manifest with each existing
        // policy authority. No new public speed setting or normalization of the
        // source Option is introduced. This never starts a native executable.
        let manifest_key = "driver-world/manifest";
        let manifest_digest = edited.auxiliary.get(manifest_key).unwrap();
        let original_manifest: wire::Prepared = serde_json::from_slice(
            &std::fs::read(artifacts.get(manifest_digest).unwrap()).unwrap(),
        )
        .unwrap();
        let mut pass_schedule = simulation.schedule.clone();
        pass_schedule.timer_jitter_ns = 127;
        pass_schedule.stop_tolerance_ns = 91;
        let pass_program = vec![seedbox_engine_protocol::input_program::TimedInput {
            key: "audit-stop".into(),
            at_ns: 37,
            action: seedbox_engine_protocol::control::Action::Stop {
                machine: "oracle".into(),
            },
            timeout_ns: seedbox_engine_protocol::input_program::default_timeout_ns(),
        }];
        for incoming in [None, Some(false), Some(true)] {
            let mut manifest = original_manifest.clone();
            let selected = incoming.map(|seeded_pv_speed| wire::BackendPolicy {
                seeded_pv_speed,
                ..Default::default()
            });
            let wire::Inventory::Pv { policy, .. } = &mut manifest.inventory else {
                unreachable!()
            };
            *policy = selected.clone();
            let digest = store.put(&serde_json::to_vec(&manifest).unwrap()).unwrap();
            let path = store
                .materialize(
                    &digest,
                    &temporary.path().join(format!("audit-policy-{incoming:?}")),
                )
                .unwrap();
            let mut supplied = edited.clone();
            supplied
                .auxiliary
                .insert(manifest_key.into(), digest.clone());
            let mut supplied_artifacts = artifacts.clone();
            supplied_artifacts.insert(digest, path);
            let supplied_request = crate::execution::ExecutionRequest {
                prepared: &supplied,
                artifacts: &supplied_artifacts,
                schedule: &pass_schedule,
                program: &pass_program,
                ..request
            };
            let adapted = seedbox_driver_execution::driver_world::retained::adapt_request(
                &supplied_request,
                &output.join("audit-control"),
            )
            .unwrap();
            let resolved = adapted.prepared.inventory.backend_policy().unwrap();
            assert_eq!(resolved.seeded_pv_speed, incoming.unwrap_or(false));
            assert!(!resolved.boot_end_evidence);
            assert_eq!(resolved.timer_jitter_ns, 127);
            assert_eq!(resolved.stop_tolerance_ns, 91);
            assert_eq!(
                adapted.program, pass_program,
                "tolerance adds no input delivery delay"
            );
            assert_eq!(adapted.program[0].at_ns, 37);
            assert_eq!(
                manifest.inventory.backend_policy(),
                selected.as_ref(),
                "source None/Some remains unchanged"
            );
        }

        // Boot-ref-only replacement of an older Driver manifest: there is no
        // paired boot/application field. Do not reuse its old tar as the new
        // filesystem or attempt to pass a Linux cpio archive to the tar parser.
        fn newc(out: &mut Vec<u8>, name: &str, bytes: &[u8]) {
            out.extend_from_slice(b"070701");
            for value in [
                1,
                0o100644,
                0,
                0,
                1,
                0,
                bytes.len() as u32,
                0,
                0,
                0,
                0,
                name.len() as u32 + 1,
                0,
            ] {
                out.extend_from_slice(format!("{value:08x}").as_bytes());
            }
            out.extend_from_slice(name.as_bytes());
            out.push(0);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
            out.extend_from_slice(bytes);
            while !out.len().is_multiple_of(4) {
                out.push(0);
            }
        }
        let mut boot = Vec::new();
        newc(&mut boot, "application", b"replacement application bytes");
        newc(&mut boot, "TRAILER!!!", &[]);
        let replacement = store.put(&boot).unwrap();
        definition.oracle.boot_archive = asset(&replacement);
        let boot_only = prepared(&definition, &store).unwrap();
        let mut artifacts = BTreeMap::new();
        for (index, digest) in boot_only.digests().enumerate() {
            artifacts.entry(digest.clone()).or_insert_with(|| {
                store
                    .materialize(
                        digest,
                        &temporary
                            .path()
                            .join("boot-replacement-materialized")
                            .join(index.to_string()),
                    )
                    .unwrap()
            });
        }
        let output = temporary.path().join("boot-replacement-output");
        std::fs::create_dir(&output).unwrap();
        let boot_request = crate::execution::ExecutionRequest {
            prepared: &boot_only,
            artifacts: &artifacts,
            output: &output,
            ..request
        };
        let worker = seedbox_driver_execution::driver_world::retained::adapt_request(
            &boot_request,
            &output.join("control"),
        )
        .unwrap();
        let wire::Inventory::Pv { boot_archives, .. } = &worker.prepared.inventory else {
            panic!("expected PV fixture")
        };
        let complete = &boot_archives["oracle"];
        assert_eq!(complete.sha256, replacement.hex());
        assert_eq!(std::fs::read(&complete.artifact).unwrap(), boot);
        assert_ne!(worker.prepared.oracle.filesystem.sha256, rootfs.hex());
        let mut archive = tar::Archive::new(
            std::fs::File::open(&worker.prepared.oracle.filesystem.artifact).unwrap(),
        );
        let mut entries = archive.entries().unwrap();
        let mut entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), std::path::Path::new("application"));
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert_eq!(bytes, b"replacement application bytes");
        assert!(entries.next().is_none());

        // TCG launch adaptation also preserves paired complete boot authority.
        // This serializes actual worker requests only: no executable/VM starts.
        for process_changed in [false, true] {
            let mut world = prepared_world.clone();
            world.inventory = wire::Inventory::Tcg {
                policy: None,
                executable: artifact(
                    &format!("tcg-worker-{process_changed}"),
                    b"fixture executable",
                ),
                kernel: artifact(&format!("tcg-kernel-{process_changed}"), b"fixture kernel"),
                init: artifact(&format!("tcg-init-{process_changed}"), b"fixture init"),
                command_line: artifact(
                    &format!("tcg-command-line-{process_changed}"),
                    b"fixture command line",
                ),
                firmware: artifact(
                    &format!("tcg-firmware-{process_changed}"),
                    b"fixture firmware",
                ),
                runtime: artifact(
                    &format!("tcg-runtime-{process_changed}"),
                    b"fixture runtime",
                ),
                formatter: None,
                environments: Default::default(),
                boot_archives: BTreeMap::from([(
                    "oracle".into(),
                    artifact(&format!("tcg-paired-boot-{process_changed}"), &boot),
                )]),
                epoch_seconds: 86400,
            };
            if !process_changed {
                world.oracle.process = wire::Process {
                    argv: definition.oracle.process.argv.clone(),
                    environment: definition.oracle.process.environment.clone(),
                    working_directory: definition.oracle.process.working_directory.clone(),
                    user: definition.oracle.process.user.clone(),
                };
            }
            let mut tcg_inputs = inputs.clone();
            tcg_inputs.profile.backend = "driver-tcg".into();
            tcg_inputs.profile.semantic_parameters.engine =
                Some(DriverExecutor::descriptor("x86_64"));
            tcg_inputs.profile.semantic_parameters.torn_writes = true;
            tcg_inputs.profile.witness_mode = crate::execution::WitnessMode::FullState;
            tcg_inputs
                .guests
                .insert("oracle".into(), replacement.clone());
            seedbox_driver_execution::driver_world::retain_prepared(
                &mut tcg_inputs,
                &world,
                &store,
            )
            .unwrap();
            let mut tcg_definition = definition.clone();
            tcg_definition.architecture = seedbox_spec::definition::Architecture::X86_64;
            tcg_definition.simulator.disk.torn_writes = true;
            tcg_definition.simulator.oracle_control = true;
            tcg_definition.runtime.execution = execution_settings_with_inputs(&tcg_inputs).unwrap();
            let tcg_inputs = prepared(&tcg_definition, &store).unwrap();
            let mut simulation = tcg_definition.configuration("").unwrap();
            simulation.schedule.timer_jitter_ns = 127;
            simulation.schedule.stop_tolerance_ns = 91;
            let output = temporary
                .path()
                .join(format!("tcg-paired-output-{process_changed}"));
            std::fs::create_dir(&output).unwrap();
            let mut artifacts = BTreeMap::new();
            for (index, digest) in tcg_inputs.digests().enumerate() {
                artifacts.entry(digest.clone()).or_insert_with(|| {
                    store
                        .materialize(digest, &output.join("inputs").join(index.to_string()))
                        .unwrap()
                });
            }
            let tcg_request = crate::execution::ExecutionRequest {
                seed: 73,
                prepared: &tcg_inputs,
                artifacts: &artifacts,
                output: &output,
                schedule: &simulation.schedule,
                control: simulation.control.as_ref(),
                ..request
            };
            let worker = seedbox_driver_execution::driver_world::retained::adapt_request(
                &tcg_request,
                &output.join("control"),
            )
            .unwrap();
            let wire::Inventory::Tcg {
                policy,
                boot_archives,
                ..
            } = &worker.prepared.inventory
            else {
                panic!("expected TCG launch data")
            };
            assert_eq!(
                policy.as_ref(),
                Some(&wire::BackendPolicy {
                    seed: 73,
                    timer_jitter_ns: 127,
                    stop_tolerance_ns: 91,
                    torn_writes: true,
                    full_state: true,
                    seeded_pv_speed: false,
                    boot_end_evidence: false,
                    controller: simulation.control.as_ref().map(|control| tcg_inputs
                        .placement
                        .get(&control.service)
                        .cloned()
                        .unwrap_or_else(|| control.service.clone())),
                })
            );
            assert_eq!(
                worker.prepared.oracle.process.argv,
                tcg_definition.oracle.process.argv
            );
            assert_eq!(worker.prepared.oracle.filesystem.sha256, rootfs.hex());
            if process_changed {
                assert!(!boot_archives.contains_key("oracle"), "same paired boot plus inline process edit rebuilds from the retained application, not stale complete bytes");
            } else {
                let complete = &boot_archives["oracle"];
                assert_eq!(complete.sha256, replacement.hex());
                assert_eq!(std::fs::read(&complete.artifact).unwrap(), boot);
            }
            assert!(!output.join("world.json").exists());
        }
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
}
