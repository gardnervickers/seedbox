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
            cpu: configuration
                .services
                .get(name)
                .ok_or_else(|| Error::Integrity(format!("missing CPU configuration for {name}")))?
                .cpu
                .clone(),
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
            name: (name == oracle_name).then(|| name.clone()),
            boot_archive: asset(boot),
            memory_bytes: u64::from(inputs.memory_mb(name)) * 1024 * 1024,
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
                            size_bytes: crate::engine_adapter::VOLUME_DISK_MIB * 1024 * 1024,
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
        architecture: match inputs.profile.backend {
            crate::execution::Backend::Hvf => Architecture::Aarch64,
            _ => Architecture::X86_64,
        },
        seed,
        clock: Clock {
            epoch_seconds: inputs
                .wall_clock_epoch
                .ok_or_else(|| Error::Integrity("prepared clock was not resolved".into()))?,
        },
        runtime: Runtime {
            kernel: asset(&inputs.kernel),
            engine: asset(&inputs.profile.engine),
            execution: execution_settings(&inputs.profile)?,
        },
        machines,
        oracle,
        network: configuration.network.clone(),
        simulator: Simulator {
            schedule: configuration.schedule.clone(),
            disk: configuration.disk.clone(),
            oracle_control: configuration.control.is_some(),
        },
        start: Start::Fresh,
        overrides,
    })
}

pub fn prepared(definition: &Definition, store: &ContentStore) -> Result<PreparedInputs> {
    definition
        .start
        .validate_overrides(&definition.overrides)
        .map_err(Error::Setup)?;
    let mut settings = definition.runtime.execution.clone();
    let settings = settings
        .as_object_mut()
        .ok_or_else(|| Error::Integrity("runtime.execution must be an inline object".into()))?;
    settings.insert(
        "engine".into(),
        serde_json::to_value(digest(&definition.runtime.engine)?)?,
    );
    if let Some(parameters) = settings
        .get_mut("semantic_parameters")
        .and_then(|v| v.as_object_mut())
    {
        parameters.insert(
            "memory_mb".into(),
            serde_json::json!(definition.oracle.memory_bytes / (1024 * 1024)),
        );
        parameters.insert(
            "torn_writes".into(),
            definition.simulator.disk.torn_writes.into(),
        );
        if let Some(hvf) = parameters.get_mut("hvf").and_then(|v| v.as_object_mut()) {
            hvf.insert(
                "guest_kernel".into(),
                serde_json::to_value(digest(&definition.runtime.kernel)?)?,
            );
        }
    }
    let mut profile: ExecutionProfile =
        serde_json::from_value(serde_json::Value::Object(settings.clone()))?;
    match (&profile.backend, definition.architecture) {
        (crate::execution::Backend::Hvf, Architecture::X86_64)
        | (crate::execution::Backend::Kvm, Architecture::Aarch64) => {
            return Err(Error::Setup(
                "architecture differs from runtime execution backend".into(),
            ))
        }
        _ => {}
    }

    profile.engine = digest(&definition.runtime.engine)?;
    if definition.oracle.memory_bytes == 0
        || !definition.oracle.memory_bytes.is_multiple_of(1024 * 1024)
    {
        return Err(Error::Setup(
            "memoryBytes must be a positive whole number of MiB".into(),
        ));
    }
    let memory = u32::try_from(definition.oracle.memory_bytes / (1024 * 1024))
        .map_err(|_| Error::Setup("oracle memoryBytes exceeds supported memory".into()))?;
    profile.semantic_parameters.memory_mb = memory;
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
        if memory_mb != profile.semantic_parameters.memory_mb {
            machine_memory_mb.insert(name.into(), memory_mb);
        }
        for volume in machine.volumes.values() {
            if volume.size_bytes != crate::engine_adapter::VOLUME_DISK_MIB * 1024 * 1024 {
                return Err(Error::Setup(
                    "volume sizeBytes must equal the supported 1 GiB disk size".into(),
                ));
            }
        }
        let boot = if matches!(profile.backend, crate::execution::Backend::Other(_)) {
            digest(&machine.boot_archive)?
        } else {
            resolved_boot(definition, name, machine, store)?
        };
        if !matches!(profile.backend, crate::execution::Backend::Other(_)) {
            let archive_bytes = store.size(&boot)?;
            let minimum = crate::guest::minimum_memory_mb(archive_bytes)?;
            if memory_mb < minimum {
                return Err(Error::Setup(format!("machine {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase memoryBytes (application memory may require more)")));
            }
        }
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
    let inputs = PreparedInputs {
        machine_memory_mb,
        wall_clock_epoch: Some(definition.clock.epoch_seconds),
        kernel: digest(&definition.runtime.kernel)?,
        images,
        placement,
        guests,
        profile,
        auxiliary: BTreeMap::new(),
        provenance: BTreeMap::new(),
    };
    for reference in inputs.digests() {
        store.verify(reference)?;
    }
    Ok(inputs)
}

/// Capture a backend checkpoint's binary files while retaining its structured state inline.
pub fn capture_start(
    path: &std::path::Path,
    owned: &std::path::Path,
    store: &ContentStore,
) -> Result<Start> {
    let mut manifest: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let owned = owned.canonicalize()?;
    let mut assets = BTreeMap::new();
    fn visit(
        value: &mut serde_json::Value,
        owned: &std::path::Path,
        store: &ContentStore,
        assets: &mut BTreeMap<String, Asset>,
    ) -> Result<()> {
        match value {
            serde_json::Value::Object(values)
                if values.get("sha256").and_then(|v| v.as_str()).is_some()
                    && (values.get("artifact").and_then(|v| v.as_str()).is_some()
                        || values.get("path").and_then(|v| v.as_str()).is_some()) =>
            {
                let key = if values.contains_key("artifact") {
                    "artifact"
                } else {
                    "path"
                };
                let text = values[key].as_str().ok_or_else(|| {
                    Error::Integrity("checkpoint binary path is not a string".into())
                })?;
                let path = std::path::Path::new(text).canonicalize()?;
                if !path.starts_with(owned) {
                    return Err(Error::Integrity(
                        "checkpoint binary refers outside its owned execution directory".into(),
                    ));
                }
                let name = format!("state-{}", assets.len() + 1);
                assets.insert(name.clone(), asset(&store.put_file(&path)?));
                values.insert(key.into(), format!("asset:{name}").into());
                for value in values.values_mut() {
                    visit(value, owned, store, assets)?;
                }
            }
            serde_json::Value::Array(values) => {
                for v in values {
                    visit(v, owned, store, assets)?;
                }
            }
            serde_json::Value::Object(values) => {
                for v in values.values_mut() {
                    visit(v, owned, store, assets)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut manifest, &owned, store, &mut assets)?;
    Ok(Start::Checkpoint { manifest, assets })
}

pub fn materialize_start(
    definition: &Definition,
    store: &ContentStore,
    directory: &std::path::Path,
) -> Result<Option<std::path::PathBuf>> {
    let Start::Checkpoint { manifest, assets } = &definition.start else {
        return Ok(None);
    };
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

/// Rebuild the launch record from inline fields, preserving prepared filesystem bytes.
/// This makes edits to process, clock and progress budget effective without an image engine.
fn resolved_boot(
    definition: &Definition,
    name: &str,
    machine: &Machine,
    store: &ContentStore,
) -> Result<ContentDigest> {
    use std::io::{Read, Seek, Write};
    let mut names: Vec<_> = definition
        .machines
        .keys()
        .map(String::as_str)
        .chain(std::iter::once(definition.oracle_name()))
        .collect();
    names.sort_unstable();
    let index = names
        .iter()
        .position(|n| *n == name)
        .ok_or_else(|| Error::Integrity("missing machine address".into()))?;
    let network = crate::guest::Network::new(index, name)?;
    let launch = Launch {
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
    };
    let mut recipe = crate::guest::launch_recipe(
        &launch,
        &network.cidr,
        &network.hosts,
        name,
        &BTreeMap::new(),
        definition.clock.epoch_seconds,
    )?;
    let mut input = tempfile::tempfile()?;
    store.copy_to(&digest(&machine.boot_archive)?, &mut input)?;
    input.rewind()?;
    let mut output = tempfile::tempfile()?;
    let mut found = false;
    loop {
        let mut header = [0u8; 110];
        input.read_exact(&mut header)?;
        if &header[..6] != b"070701" {
            return Err(Error::Integrity(
                "bootArchive must be an uncompressed newc archive".into(),
            ));
        }
        let number = |bytes: &[u8]| -> Result<usize> {
            usize::from_str_radix(
                std::str::from_utf8(bytes)
                    .map_err(|_| Error::Integrity("invalid boot archive header".into()))?,
                16,
            )
            .map_err(|_| Error::Integrity("invalid boot archive size".into()))
        };
        let size = number(&header[54..62])?;
        let length = number(&header[94..102])?;
        if length == 0 || length > 4096 {
            return Err(Error::Integrity("invalid boot archive path length".into()));
        }
        let mut path = vec![0; length];
        input.read_exact(&mut path)?;
        let name_pad = (4 - (110 + length) % 4) % 4;
        let mut padding = [0u8; 3];
        input.read_exact(&mut padding[..name_pad])?;
        if path == b"seedbox/launch.bin\0" {
            if size > 16 * 1024 * 1024 {
                return Err(Error::Integrity("launch record exceeds 16 MiB".into()));
            }
            let mut old = vec![0; size];
            input.read_exact(&mut old)?;
            let tail = launch_configs(&old)?;
            recipe.truncate(recipe.len() - 4);
            recipe.extend_from_slice(tail);
            header[54..62].copy_from_slice(format!("{:08x}", recipe.len()).as_bytes());
            output.write_all(&header)?;
            output.write_all(&path)?;
            output.write_all(&padding[..name_pad])?;
            output.write_all(&recipe)?;
            output.write_all(&[0; 3][..(4 - recipe.len() % 4) % 4])?;
            found = true;
        } else {
            output.write_all(&header)?;
            output.write_all(&path)?;
            output.write_all(&padding[..name_pad])?;
            let copied = std::io::copy(&mut (&mut input).take(size as u64), &mut output)?;
            if copied != size as u64 {
                return Err(Error::Integrity("truncated boot archive".into()));
            }
            output.write_all(&[0; 3][..(4 - size % 4) % 4])?;
        }
        input.read_exact(&mut padding[..(4 - size % 4) % 4])?;
        if path == b"TRAILER!!!\0" {
            break;
        }
    }
    if !found {
        return Err(Error::Integrity(
            "bootArchive omits seedbox/launch.bin".into(),
        ));
    }
    // newc permits final zero padding; retaining it preserves byte identity for unchanged recipes.
    std::io::copy(&mut input, &mut output)?;
    output.rewind()?;
    store.put_reader(output)
}
fn launch_configs(bytes: &[u8]) -> Result<&[u8]> {
    let fail = || Error::Integrity("invalid retained launch record".into());
    if !bytes.starts_with(crate::guest::LAUNCH_MAGIC) {
        return Err(fail());
    }
    let mut cursor = crate::guest::LAUNCH_MAGIC.len();
    fn number(bytes: &[u8], cursor: &mut usize) -> Option<usize> {
        let n = u32::from_le_bytes(bytes.get(*cursor..*cursor + 4)?.try_into().ok()?) as usize;
        *cursor += 4;
        Some(n)
    }
    for _ in 0..5 {
        let n = number(bytes, &mut cursor).ok_or_else(fail)?;
        cursor = cursor
            .checked_add(n)
            .filter(|n| *n <= bytes.len())
            .ok_or_else(fail)?;
    }
    cursor = cursor
        .checked_add(16)
        .filter(|n| *n <= bytes.len())
        .ok_or_else(fail)?;
    for _ in 0..4 {
        let count = number(bytes, &mut cursor).ok_or_else(fail)?;
        if count > 65536 {
            return Err(fail());
        }
        for _ in 0..count {
            let n = number(bytes, &mut cursor).ok_or_else(fail)?;
            cursor = cursor
                .checked_add(n)
                .filter(|n| *n <= bytes.len())
                .ok_or_else(fail)?;
        }
    }
    bytes
        .get(cursor..)
        .filter(|tail| tail.len() >= 4)
        .ok_or_else(fail)
}

/// Inline settings omit values already defined by canonical binary or machine fields.
pub fn execution_settings(profile: &ExecutionProfile) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(profile)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("engine");
    }
    if let Some(parameters) = value
        .get_mut("semantic_parameters")
        .and_then(|v| v.as_object_mut())
    {
        parameters.remove("memory_mb");
        parameters.remove("torn_writes");
        if let Some(hvf) = parameters.get_mut("hvf").and_then(|v| v.as_object_mut()) {
            hvf.remove("guest_kernel");
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let recipe = crate::guest::launch_recipe(
            &launch,
            "10.0.0.1/24",
            "127.0.0.1 localhost\n::1 localhost\n10.0.0.1 oracle\n",
            "oracle",
            &configs,
            86400,
        )
        .unwrap();
        let archive = temporary.path().join("boot.cpio");
        crate::guest::build_initramfs(&root, &init, &recipe, &archive).unwrap();
        let boot = store.put_file(&archive).unwrap();
        let kernel = store.put(b"kernel").unwrap();
        let engine = store.put(b"engine").unwrap();
        let profile = ExecutionProfile {
            backend: crate::execution::Backend::Hvf,
            engine,
            witness_mode: crate::execution::WitnessMode::Exits,
            semantic_parameters: crate::execution::SemanticParameters::new(1024),
        };
        let configuration = crate::config::parse_for_architecture(
            "name: test\nservices:\n  oracle:\n    image: example/oracle\n",
            std::path::Path::new("/project/compose.yaml"),
            &Default::default(),
            crate::guest::Architecture::Arm64,
        )
        .unwrap();
        let inputs = PreparedInputs {
            machine_memory_mb: Default::default(),
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
        assert_eq!(inputs.memory_mb("app"), 2048);
        assert_eq!(inputs.memory_mb("oracle"), 256);
        assert_eq!(
            inputs.machine_memory_mb,
            BTreeMap::from([("app".into(), 2048)])
        );
        let configuration = saved.configuration("mixed");
        assert_eq!(configuration.services["app"].memory_mb, Some(2048));
        assert_eq!(configuration.services["oracle"].memory_mb, Some(256));
        let resolved =
            resolve(&configuration, &inputs, saved.seed, saved.overrides.clone()).unwrap();
        assert_eq!(
            resolved.machines["app"].memory_bytes,
            saved.machines["app"].memory_bytes
        );
        assert_eq!(resolved.oracle.memory_bytes, saved.oracle.memory_bytes);
        for memory in [0, 64 * 1024 * 1024 + 1, 3073 * 1024 * 1024] {
            definition.machines.get_mut("app").unwrap().memory_bytes = memory;
            assert!(prepared(&definition, &store).is_err(), "{memory}");
        }
        definition.machines.get_mut("app").unwrap().memory_bytes = 64 * 1024 * 1024;
        assert!(prepared(&definition, &store)
            .unwrap_err()
            .to_string()
            .contains("archive boot floor"));
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
        // The CPU is the VMM's input, not the guest's: the recipe and the
        // prepared guests do not change with it.
        definition.oracle.cpu.speed_hz = 800_000_000;
        definition.oracle.cpu.jitter_percent = 5;
        let mut worker = definition.oracle.clone();
        worker.cpu = Default::default();
        definition.machines.insert("worker".into(), worker);
        let expanded = prepared(&definition, &store).unwrap();
        for (name, own, peer) in [
            ("oracle", "10.0.0.1 oracle\n", "10.0.0.2 worker\n"),
            ("worker", "10.0.0.2 worker\n", "10.0.0.1 oracle\n"),
        ] {
            let archive = store.read(&expanded.guests[name]).unwrap();
            let hosts = format!("127.0.0.1 localhost\n::1 localhost\n{own}");
            assert!(archive
                .windows(hosts.len())
                .any(|bytes| bytes == hosts.as_bytes()));
            assert!(!archive
                .windows(peer.len())
                .any(|bytes| bytes == peer.as_bytes()));
        }
        let restored: Definition =
            serde_json::from_value(serde_json::to_value(&definition).unwrap()).unwrap();
        let configuration = restored.configuration("roundtrip");
        assert_eq!(configuration.services["oracle"].cpu, definition.oracle.cpu);
        assert_eq!(configuration.services["worker"].cpu.speed_hz, 1_000_000_000);
        assert_eq!(configuration.services["worker"].cpu.jitter_percent, 0);
        definition.machines.remove("worker");
        assert_eq!(
            prepared(&definition, &store).unwrap().guests,
            changed.guests
        );
        definition.oracle.cpu = Default::default();
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
