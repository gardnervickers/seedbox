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
            name: (name == oracle_name).then(|| name.clone()),
            boot_archive: asset(boot),
            memory_bytes: u64::from(inputs.profile.semantic_parameters.memory_mb) * 1024 * 1024,
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
        // The retained engine descriptor names the guest ISA of Driver
        // profiles (aarch64 on HVF hosts); legacy profiles keep their backend.
        architecture: match inputs.profile.backend {
            crate::execution::Backend::Hvf => Architecture::Aarch64,
            _ => match crate::guest::Architecture::from_profile(&inputs.profile)? {
                crate::guest::Architecture::Amd64 => Architecture::X86_64,
                crate::guest::Architecture::Arm64 => Architecture::Aarch64,
            },
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
            execution: execution_settings_with_inputs(inputs)?,
        },
        machines,
        oracle,
        network: configuration.network.clone(),
        simulator: Simulator {
            blocks_per_progress_call: configuration.simulator.blocks_per_progress_call,
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
    let driver_closure: Option<crate::driver_world::retained::Closure> = settings
        .remove("driver_world")
        .map(serde_json::from_value)
        .transpose()?;
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

    if profile.semantic_parameters.launch() == crate::engine::ExecutableKind::DriverWorld {
        if let Start::Checkpoint { manifest, .. } = &definition.start {
            crate::driver_world::preflight::restore_manifest(
                manifest,
                definition
                    .simulator
                    .oracle_control
                    .then(|| definition.oracle_name()),
            )?;
        }
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
    for (name, machine) in definition
        .machines
        .iter()
        .map(|(n, m)| (n.as_str(), m))
        .chain(std::iter::once((
            definition.oracle_name(),
            &definition.oracle,
        )))
    {
        if machine.memory_bytes != definition.oracle.memory_bytes {
            return Err(Error::Setup(
                "this backend requires equal memoryBytes for every machine".into(),
            ));
        }
        for volume in machine.volumes.values() {
            if volume.size_bytes != crate::execution::VOLUME_DISK_MIB * 1024 * 1024 {
                return Err(Error::Setup(
                    "volume sizeBytes must equal the supported 1 GiB disk size".into(),
                ));
            }
        }
        let boot = if matches!(profile.backend, crate::execution::Backend::Other(_))
            || profile.semantic_parameters.launch() == crate::engine::ExecutableKind::DriverWorld
        {
            digest(&machine.boot_archive)?
        } else {
            resolved_boot(definition, name, machine, store)?
        };
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
        && profile.semantic_parameters.launch() != crate::engine::ExecutableKind::DriverWorld
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
        wall_clock_epoch: Some(definition.clock.epoch_seconds),
        kernel: digest(&definition.runtime.kernel)?,
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
pub fn capture_start(
    path: &std::path::Path,
    owned: &std::path::Path,
    store: &ContentStore,
) -> Result<Start> {
    capture_start_observed(path, owned, store, &mut |_| Ok(()))
}

/// Capture with cooperative byte-work observation, including verification passes.
/// An observer error stops capture without returning a partial starting state.
/// Already retained immutable assets remain reusable in the shared store.
pub fn capture_start_observed(
    path: &std::path::Path,
    owned: &std::path::Path,
    store: &ContentStore,
    progress: &mut impl FnMut(u64) -> std::io::Result<()>,
) -> Result<Start> {
    progress(0)?;
    let mut manifest: serde_json::Value = serde_json::from_reader(std::fs::File::open(path)?)?;
    let owned = owned.canonicalize()?;
    let mut assets = BTreeMap::new();
    fn visit(
        value: &mut serde_json::Value,
        owned: &std::path::Path,
        store: &ContentStore,
        assets: &mut BTreeMap<String, Asset>,
        progress: &mut impl FnMut(u64) -> std::io::Result<()>,
    ) -> Result<()> {
        progress(0)?;
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
                assets.insert(
                    name.clone(),
                    asset(&store.put_file_observed(&path, progress)?),
                );
                values.insert(key.into(), format!("asset:{name}").into());
                for value in values.values_mut() {
                    visit(value, owned, store, assets, progress)?;
                }
            }
            serde_json::Value::Array(values) => {
                for v in values {
                    visit(v, owned, store, assets, progress)?;
                }
            }
            serde_json::Value::Object(values) => {
                for v in values.values_mut() {
                    visit(v, owned, store, assets, progress)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    visit(&mut manifest, &owned, store, &mut assets, progress)?;
    Ok(Start::Checkpoint { manifest, assets })
}

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
        crate::driver_world::preflight::restore_manifest(
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
    let mut hosts = String::from("127.0.0.1 localhost\n::1 localhost\n");
    for (index, name) in names.iter().enumerate() {
        hosts.push_str(&format!("10.0.0.{} {name}\n", index + 1));
    }
    let index = names
        .iter()
        .position(|n| *n == name)
        .ok_or_else(|| Error::Integrity("missing machine address".into()))?;
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
    let mut recipe = crate::guest::launch_recipe_with_progress(
        &launch,
        &format!("10.0.0.{}/24", index + 1),
        &hosts,
        name,
        &BTreeMap::new(),
        definition.clock.epoch_seconds,
        definition.simulator.blocks_per_progress_call,
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
        .checked_add(12)
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
/// Inline the retained launch profile and Driver's explicit binary input closure.
/// Host paths and source provenance are never retained as factory authority.
// TODO: Re-evaluate: the server round-trips preparation through Definition before
// execution; an auxiliary-only manifest otherwise disappears during that step.
pub fn execution_settings_with_inputs(inputs: &PreparedInputs) -> Result<serde_json::Value> {
    let mut settings = execution_settings(&inputs.profile)?;
    if let Some(closure) = crate::driver_world::retained::Closure::from_inputs(inputs)? {
        settings
            .as_object_mut()
            .ok_or_else(|| Error::Integrity("runtime execution settings are not an object".into()))?
            .insert("driver_world".into(), serde_json::to_value(closure)?);
    }
    Ok(settings)
}

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
    fn backend_authority_rejects_before_resolved_asset_materialization() {
        for (top, actual) in [("driver-pv", "driver-tcg"), ("driver-tcg", "driver-pv")] {
            let (root, store, mut definition, _) = fixture();
            definition.runtime.execution["backend"] = top.into();
            definition.runtime.execution["semantic_parameters"]["engine"] =
                serde_json::json!({"kind":"driver-world-v1", "engine_backend":actual});
            definition.runtime.kernel = Asset(format!("sha256:{}", "0".repeat(64)));
            let error = prepared(&definition, &store).err().unwrap().to_string();
            assert!(
                error.contains("inconsistent Driver backend metadata"),
                "{error}"
            );
            assert!(definition
                .configuration("")
                .unwrap_err()
                .contains("inconsistent Driver backend metadata"));
            let output = root.path().join("not-materialized");
            assert!(materialize_start(&definition, &store, &output)
                .err()
                .unwrap()
                .to_string()
                .contains("inconsistent Driver backend metadata"));
            assert!(!output.exists());
        }
    }
    #[test]
    fn unsupported_driver_resume_refuses_before_any_start_asset_materialization() {
        let (root, store, mut definition, _) = fixture();
        let policy = seedbox_engine_protocol::driver_world::BackendPolicy {
            timer_jitter_ns: u64::MAX,
            ..Default::default()
        };
        let manifest = crate::driver_world::preflight::tests::manifest(Some(policy));
        let expected = crate::driver_world::preflight::restore_manifest(&manifest, None)
            .unwrap_err()
            .to_string();
        definition.start = Start::Checkpoint {
            manifest,
            assets: BTreeMap::from([("body".into(), Asset(format!("sha256:{}", "11".repeat(32))))]),
        };
        let mut semantic = crate::execution::SemanticParameters::new(1024);
        semantic.engine = Some(crate::driver_world::DriverExecutor::descriptor(
            crate::driver_world::DriverBackend::Pv,
            "x86_64",
        ));
        let profile = ExecutionProfile {
            backend: crate::execution::Backend::Other("driver-pv".into()),
            engine: ContentDigest::of(b"engine"),
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
            manifest: crate::driver_world::preflight::tests::manifest(None),
            assets: BTreeMap::from([("body".into(), asset(&body))]),
        };
        let output = root.path().join("allowed");
        assert!(materialize_start(&definition, &store, &output)
            .unwrap()
            .unwrap()
            .is_file());
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
        use crate::driver_world::{DriverBackend, DriverExecutor};
        use seedbox_engine_protocol::driver_world as wire;
        let (temporary, store, mut definition, _) = fixture();
        let mut inputs = prepared(&definition, &store).unwrap();
        inputs.profile.backend = crate::execution::Backend::Other("driver-pv".into());
        inputs.profile.semantic_parameters.engine =
            Some(DriverExecutor::descriptor(DriverBackend::Pv, "aarch64"));
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
                blocks_per_progress_call: std::num::NonZeroU32::new(32).unwrap(),
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
        crate::driver_world::retain_prepared(&mut inputs, &prepared_world, &store).unwrap();
        definition.runtime.execution = execution_settings_with_inputs(&inputs).unwrap();
        definition.oracle.boot_archive = asset(&rootfs);
        let roundtrip = prepared(&definition, &store).unwrap();
        assert_eq!(roundtrip.auxiliary, inputs.auxiliary);
        assert_eq!(roundtrip.guests["oracle"], rootfs);
        assert_eq!(
            crate::driver_world::runtime_digest(&roundtrip, &store).unwrap(),
            ContentDigest::of(b"fixture runtime")
        );
        let mut missing_runtime = roundtrip.clone();
        missing_runtime.auxiliary.retain(|key, digest| {
            !key.starts_with("driver-world/object/")
                || digest != &ContentDigest::of(b"fixture runtime")
        });
        assert!(
            crate::driver_world::runtime_digest(&missing_runtime, &store)
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
        let request = crate::execution::ExecutionRequest {
            start_paused: true,
            resume_checkpoint: None,
            intervention_history: None,
            replay_interventions: None,
            observations: None,
            seed: 42,
            program: &[],
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
        let launch = crate::worker_process::WorkerLaunch::prepare(&request).unwrap();
        let args: Vec<_> = launch
            .command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            &args[..3],
            ["__engine", crate::engine::DRIVER_WORLD_V1, "--owner-pipe"]
        );
        assert!(
            !output.join("world.json").exists(),
            "new Driver runs must not construct a legacy WorldConfig"
        );
        let worker: wire::Request =
            serde_json::from_slice(&std::fs::read(output.join("worker-request.json")).unwrap())
                .unwrap();
        assert_eq!(
            worker.prepared.oracle.process.argv,
            definition.oracle.process.argv
        );
        assert!(worker.start_paused);
        assert_eq!(worker.machine_workers, 4);
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
        drop(launch);

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
            action: seedbox_control::Action::Stop {
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
            let adapted = crate::driver_world::retained::request(
                &supplied_request,
                &output.join("audit-control"),
                None,
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
        let launch = crate::worker_process::WorkerLaunch::prepare(&boot_request).unwrap();
        let worker: wire::Request =
            serde_json::from_slice(&std::fs::read(output.join("worker-request.json")).unwrap())
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
        drop(launch);

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
            tcg_inputs.profile.backend = crate::execution::Backend::Other("driver-tcg".into());
            tcg_inputs.profile.semantic_parameters.engine =
                Some(DriverExecutor::descriptor(DriverBackend::Tcg, "x86_64"));
            tcg_inputs.profile.semantic_parameters.torn_writes = true;
            tcg_inputs.profile.witness_mode = crate::execution::WitnessMode::FullState;
            tcg_inputs
                .guests
                .insert("oracle".into(), replacement.clone());
            crate::driver_world::retain_prepared(&mut tcg_inputs, &world, &store).unwrap();
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
            let launch = crate::worker_process::WorkerLaunch::prepare(&tcg_request).unwrap();
            let worker: wire::Request =
                serde_json::from_slice(&std::fs::read(output.join("worker-request.json")).unwrap())
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
            drop(launch);
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
