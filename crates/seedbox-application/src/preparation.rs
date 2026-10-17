//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::limits::read_bounded;
use crate::{
    config::Simulation,
    execution::{ExecutionProfile, PreparedInputs},
    guest::minimum_memory_mb,
    store::ContentStore,
    Error, Result,
};
use std::{collections::BTreeMap, fs, path::PathBuf};

const CONFIG_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct OciPreparer {
    pub kernel: PathBuf,
    pub engine: PathBuf,
    pub init: PathBuf,
    /// Architecture-specific automatic application instrumentation bundle.
    pub progress: PathBuf,
    /// Static mke2fs, needed only when a service mounts a named volume: the
    /// guest formats its blank retained disk with it on first boot.
    pub mke2fs: Option<PathBuf>,
    pub profile: ExecutionProfile,
    pub docker: bollard::Docker,
}

pub use crate::preparation_command::{PreparationContext, PreparationStage};

impl OciPreparer {
    /// Share normal image resolution/retention while the selected native SDK
    /// alone packages Driver boot bytes. Runtime formats are not interchangeable.
    pub fn prepare_driver_with_transport(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn crate::oci::ImageTransport,
        runtime: &crate::runtime_assets::Inputs,
    ) -> Result<PreparedInputs> {
        context.check()?;
        crate::driver_world::validate_oci_runtime(self, runtime)?;
        self.prepare_inner(simulation, store, context, transport, runtime)
    }
    fn prepare_inner(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn crate::oci::ImageTransport,
        driver_runtime: &crate::runtime_assets::Inputs,
    ) -> Result<PreparedInputs> {
        let architecture = crate::guest::Architecture::from_profile(&self.profile)?;
        for (name, service) in &simulation.services {
            if service.platform != architecture.platform() {
                return Err(Error::Setup(format!(
                    "service {name} requests {}; this engine requires {}",
                    service.platform,
                    architecture.platform()
                )));
            }
        }

        context.check()?;

        let kernel = store.put_file(&self.kernel)?;
        let engine = store.put_file(&self.engine)?;
        if engine != self.profile.engine {
            return Err(Error::Integrity(
                "VMM changed after profile construction".into(),
            ));
        }
        architecture.validate_elf(&self.init)?;
        for (name, service) in &simulation.services {
            for mount in &service.volumes {
                validate_config_target(&mount.target).map_err(|_| Error::Validation {
                    path: format!("$.services.{name}.volumes"),
                    message: format!("reserved guest runtime path {}", mount.target),
                })?;
            }
        }
        let volumes = simulation.services.values().any(|s| !s.volumes.is_empty());
        let mut auxiliary = BTreeMap::from([("guest-init".into(), store.put_file(&self.init)?)]);
        if driver_runtime.qemu.is_some() {
            auxiliary.insert(
                "guest-firmware".into(),
                store.put_file(&driver_runtime.guest_assets.join("firmware"))?,
            );
        } else {
            auxiliary.insert("guest-progress".into(), store.put_file(&self.progress)?);
        }
        if volumes {
            let mke2fs = self.mke2fs.as_ref().ok_or_else(|| {
                Error::Setup(
                    "named volumes need the formatter from a complete Driver runtime package; rebuild the SDK"
                        .into(),
                )
            })?;
            architecture.validate_elf(mke2fs)?;
            auxiliary.insert("guest-mke2fs".into(), store.put_file(mke2fs)?);
        }
        let mut configs = BTreeMap::new();
        for (name, path) in &simulation.configs {
            let data = read_bounded(
                fs::File::open(path)?,
                CONFIG_LIMIT,
                format_args!("config {name}"),
            )
            .map_err(|e| e.or_too_large(Error::Execution))?;
            auxiliary.insert(format!("config/{name}"), store.put(&data)?);
            configs.insert(name, data);
        }

        context.setup_inputs(&auxiliary, &engine, &kernel)?;

        let addresses: BTreeMap<_, _> = simulation
            .services
            .keys()
            .enumerate()
            .map(|(index, name)| (name, format!("10.0.0.{}", index + 1)))
            .collect();
        let mut hosts = "127.0.0.1 localhost\n::1 localhost\n".to_owned();
        for (name, address) in &addresses {
            hosts.push_str(&format!("{address} {name}\n"));
        }
        let temporary = tempfile::tempdir()?;
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let mut imported_sources = BTreeMap::new();
        let mut launches = Vec::new();
        for (name, service) in &simulation.services {
            context.progress(name, PreparationStage::Image);

            context.check()?;

            let mounts = service
                .configs
                .iter()
                .map(|mount| {
                    validate_config_target(&mount.target)?;
                    Ok((
                        mount.target.clone(),
                        configs
                            .get(&mount.source)
                            .ok_or_else(|| Error::Integrity("unknown config source".into()))?
                            .clone(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let source = serde_json::to_string(&service.source)?;
            let imported = successful_source(&mut imported_sources, source, || {
                transport.import(
                    service,
                    &temporary.path().join(name),
                    store,
                    Some((context, name.as_str())),
                    architecture,
                )
            })?
            .for_service(service, store)?;

            context.check()?;

            launches.push((name, service, mounts, imported));
        }
        // The guest clock starts at the Compose file's x-seedbox.wall_clock_epoch,
        // or else at the latest build time recorded in any prepared image, so
        // certificates minted during a build are already valid and the same
        // images always start at the same time. Linux cannot set a time earlier
        // than guest uptime, so the derived value is at least one day.
        let wall_clock_epoch = match simulation.wall_clock_epoch {
            Some(epoch) => epoch,
            None => {
                let mut latest = i64::from(crate::guest::MIN_WALL_CLOCK_EPOCH);
                for (_, _, _, imported) in &launches {
                    let config = store.read(&imported.image.config)?;
                    if let Some(created) = crate::oci::image_created_epoch(&config)? {
                        latest = latest.max(created);
                    }
                }
                u32::try_from(latest).map_err(|_| {
                    Error::Integrity(
                        "image created time exceeds the supported wall-clock epoch; set x-seedbox.wall_clock_epoch".into(),
                    )
                })?
            }
        };
        let driver_factory = crate::driver_world::preparation::OciBootFactory::new(
            &self.engine,
            driver_runtime,
            wall_clock_epoch,
            simulation.simulator.blocks_per_progress_call,
        )?;
        for (name, _service, mounts, imported) in launches {
            context.progress(name, PreparationStage::Guest);
            context.check()?;

            let archive = temporary.path().join(format!("{name}.cpio"));
            let boot = driver_factory.boot(
                crate::driver_world::preparation::OciBoot {
                    name,
                    image: &imported,
                    simulation,
                    configs: &mounts,
                    hosts: &hosts,
                },
                store,
            )?;
            auxiliary.insert(
                format!("{}{name}", crate::driver_world::preparation::FILESYSTEM),
                boot.filesystem,
            );
            fs::write(&archive, boot.bytes)?;
            let memory_mb = self.profile.semantic_parameters.memory_mb;
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory_mb < minimum {
                return Err(Error::Execution(format!("service {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase x-seedbox.machine.memory_mb (application memory may require more)")));
            }

            context.check()?;

            guests.insert(name.clone(), store.put_file(&archive)?);
            images.insert(name.clone(), imported.image);
            auxiliary.extend(
                imported
                    .auxiliary
                    .into_iter()
                    .map(|(key, value)| (format!("image/{name}/{key}"), value)),
            );
            provenance.extend(
                imported
                    .provenance
                    .into_iter()
                    .map(|(key, value)| (format!("image/{name}/{key}"), value)),
            );

            context.progress(name, PreparationStage::Ready);
        }
        Ok(PreparedInputs {
            wall_clock_epoch: Some(wall_clock_epoch),
            kernel,
            images,
            guests,
            placement: simulation
                .services
                .keys()
                .map(|name| (name.clone(), name.clone()))
                .collect(),
            profile: self.profile.clone(),
            auxiliary,
            provenance,
        })
    }
}

fn successful_source<K: Ord + Clone, V: Clone>(
    sources: &mut BTreeMap<K, V>,
    key: K,
    import: impl FnOnce() -> Result<V>,
) -> Result<V> {
    if let Some(value) = sources.get(&key) {
        return Ok(value.clone());
    }
    let value = import()?;
    sources.insert(key, value.clone());
    Ok(value)
}
fn validate_config_target(path: &str) -> Result<()> {
    // Mounting over the runtime's devices or name resolution would invalidate
    // its own contract. Other paths remain inside the guest chroot.
    if path == "/"
        || path == "/etc/hosts"
        || ["/dev", "/proc", "/sys"]
            .iter()
            .any(|reserved| path == *reserved || path.starts_with(&format!("{reserved}/")))
    {
        return Err(Error::Validation {
            path: "configs.target".into(),
            message: format!("reserved guest runtime path {path}"),
        });
    }
    Ok(())
}
