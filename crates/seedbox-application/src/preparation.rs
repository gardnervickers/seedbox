//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::limits::read_bounded;
use crate::{
    config::{Service, Simulation},
    execution::{ExecutionProfile, PreparedInputs},
    guest::minimum_memory_mb,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_driver_execution::driver_world::preparation::{BootArchive, OciBoot, OciBootFactory};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

const CONFIG_LIMIT: u64 = 16 * 1024 * 1024;

// Private dependency boundary: policy tests need no native factory admission.
// The public entrypoint always binds the real validator and boot factory below.
trait BootFactory {
    fn boot(&self, request: OciBoot<'_>, store: &ContentStore) -> Result<BootArchive>;
}
impl BootFactory for OciBootFactory {
    fn boot(&self, request: OciBoot<'_>, store: &ContentStore) -> Result<BootArchive> {
        OciBootFactory::boot(self, request, store)
    }
}

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
        runtime: &seedbox_driver_execution::runtime_assets::Inputs,
    ) -> Result<PreparedInputs> {
        self.prepare_with_factory(
            simulation,
            store,
            context,
            transport,
            runtime,
            (
                || crate::driver_preparation::validate_oci_runtime(self, runtime),
                |epoch, blocks| OciBootFactory::new(&self.engine, runtime, epoch, blocks),
            ),
        )
    }
    fn prepare_with_factory<F: BootFactory>(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn crate::oci::ImageTransport,
        runtime: &seedbox_driver_execution::runtime_assets::Inputs,
        (validate, factory): (
            impl FnOnce() -> Result<()>,
            impl FnOnce(u32, seedbox_spec::progress::ProgressBudget) -> Result<F>,
        ),
    ) -> Result<PreparedInputs> {
        context.check()?;
        validate()?;
        self.prepare_inner(simulation, store, context, transport, runtime, factory)
    }
    fn prepare_inner<F: BootFactory>(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn crate::oci::ImageTransport,
        driver_runtime: &seedbox_driver_execution::runtime_assets::Inputs,
        factory: impl FnOnce(u32, seedbox_spec::progress::ProgressBudget) -> Result<F>,
    ) -> Result<PreparedInputs> {
        let architecture = self.validate_services(simulation, context)?;
        let kernel = store.put_file(&self.kernel)?;
        let engine = store.put_file(&self.engine)?;
        architecture.validate_elf(&self.init)?;
        validate_volume_targets(simulation)?;
        let mut auxiliary =
            self.guest_auxiliary(simulation, store, architecture, driver_runtime)?;
        let configs = read_configs(simulation, store, &mut auxiliary)?;

        context.setup_inputs(&auxiliary, &engine, &kernel)?;

        let hosts = hosts_file(simulation);
        let temporary = tempfile::tempdir()?;
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let launches = import_images(
            simulation,
            store,
            context,
            transport,
            architecture,
            &configs,
            temporary.path(),
        )?;
        // The guest clock starts at the resolved epoch,
        // or else at the latest build time recorded in any prepared image, so
        // certificates minted during a build are already valid and the same
        // images always start at the same time. Linux cannot set a time earlier
        // than guest uptime, so the derived value is at least one day.
        let wall_clock_epoch = match simulation.wall_clock_epoch {
            Some(epoch) => epoch,
            None => latest_image_epoch(store, &launches)?,
        };
        let driver_factory = factory(wall_clock_epoch, simulation.simulator.progress_budget)?;
        for launch in launches {
            let Launch { name, service, .. } = launch;
            context.progress(name, PreparationStage::Guest);
            context.check()?;

            let archive = temporary.path().join(format!("{name}.cpio"));
            let boot = launch.boot(&driver_factory, simulation, store, &hosts)?;
            let imported = launch.imported;
            auxiliary.insert(
                format!(
                    "{}{name}",
                    seedbox_driver_execution::driver_world::preparation::FILESYSTEM
                ),
                boot.filesystem,
            );
            fs::write(&archive, boot.bytes)?;
            let memory_mb = service.memory_mb.unwrap_or(simulation.machine.memory_mb);
            validate_archive_memory(name, memory_mb, &archive)?;

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
        auxiliary.insert("driver-world/executable".into(), engine);
        Ok(PreparedInputs {
            machine_memory_mb: simulation
                .services
                .iter()
                .map(|(name, service)| {
                    (
                        name.clone(),
                        service.memory_mb.unwrap_or(simulation.machine.memory_mb),
                    )
                })
                .collect(),
            wall_clock_epoch: Some(wall_clock_epoch),
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

    /// Check every service's platform and memory against this engine's profile.
    fn validate_services(
        &self,
        simulation: &Simulation,
        context: &PreparationContext,
    ) -> Result<crate::guest::Architecture> {
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

        for (name, service) in &simulation.services {
            validate_machine_memory(
                &self.profile,
                name,
                service.memory_mb.unwrap_or(simulation.machine.memory_mb),
            )?;
        }
        Ok(architecture)
    }

    /// Store the guest runtime binaries every machine boots with.
    fn guest_auxiliary(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        architecture: crate::guest::Architecture,
        driver_runtime: &seedbox_driver_execution::runtime_assets::Inputs,
    ) -> Result<BTreeMap<String, ContentDigest>> {
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
        Ok(auxiliary)
    }
}

/// One service whose image is imported and awaits its guest archive.
struct Launch<'a> {
    name: &'a String,
    service: &'a Service,
    mounts: BTreeMap<String, Vec<u8>>,
    imported: crate::oci::ImportedImage,
}

impl Launch<'_> {
    /// Render this service's Driver boot archive from its imported image.
    fn boot(
        &self,
        factory: &impl BootFactory,
        simulation: &Simulation,
        store: &ContentStore,
        hosts: &str,
    ) -> Result<BootArchive> {
        factory.boot(
            OciBoot {
                name: self.name,
                filesystem: crate::driver_preparation::filesystem(&self.imported.rootfs)?,
                launch: &self.imported.image.launch,
                simulation,
                configs: &self.mounts,
                hosts,
            },
            store,
        )
    }
}

fn validate_volume_targets(simulation: &Simulation) -> Result<()> {
    for (name, service) in &simulation.services {
        for mount in &service.volumes {
            validate_config_target(&mount.target).map_err(|_| Error::Validation {
                path: format!("$.services.{name}.volumes"),
                message: format!("reserved guest runtime path {}", mount.target),
            })?;
        }
    }
    Ok(())
}

/// Read and store each project config, keyed by name for service mounts.
fn read_configs<'a>(
    simulation: &'a Simulation,
    store: &ContentStore,
    auxiliary: &mut BTreeMap<String, ContentDigest>,
) -> Result<BTreeMap<&'a String, Vec<u8>>> {
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
    Ok(configs)
}

/// The guest hosts file: loopback plus one address per service, in name order.
fn hosts_file(simulation: &Simulation) -> String {
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
    hosts
}

/// Import each service's image, sharing verified bytes between identical sources.
fn import_images<'a>(
    simulation: &'a Simulation,
    store: &ContentStore,
    context: &PreparationContext,
    transport: &dyn crate::oci::ImageTransport,
    architecture: crate::guest::Architecture,
    configs: &BTreeMap<&String, Vec<u8>>,
    temporary: &Path,
) -> Result<Vec<Launch<'a>>> {
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
                &temporary.join(name),
                store,
                Some((context, name.as_str())),
                architecture,
            )
        })?
        .for_service(service, store)?;

        context.check()?;

        launches.push(Launch {
            name,
            service,
            mounts,
            imported,
        });
    }
    Ok(launches)
}

/// The latest image build time, never earlier than the minimum guest epoch.
fn latest_image_epoch(store: &ContentStore, launches: &[Launch<'_>]) -> Result<u32> {
    let mut latest = i64::from(crate::guest::MIN_WALL_CLOCK_EPOCH);
    for launch in launches {
        let config = store.read(&launch.imported.image.config)?;
        if let Some(created) = crate::oci::image_created_epoch(&config)? {
            latest = latest.max(created);
        }
    }
    u32::try_from(latest).map_err(|_| {
        Error::Integrity("image created time exceeds the supported wall-clock epoch".into())
    })
}

fn validate_archive_memory(name: &str, memory_mb: u32, archive: &Path) -> Result<()> {
    let minimum = minimum_memory_mb(fs::metadata(archive)?.len())?;
    if memory_mb < minimum {
        return Err(Error::Execution(format!("service {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase memoryMiB for this machine (application memory may require more)")));
    }
    Ok(())
}

pub(crate) fn validate_machine_memory(
    profile: &ExecutionProfile,
    name: &str,
    memory_mb: u32,
) -> Result<()> {
    // The same per-architecture range the project and submission readers check.
    let maximum = match crate::guest::Architecture::from_profile(profile)? {
        crate::guest::Architecture::Arm64 => 3072,
        crate::guest::Architecture::Amd64 => 65536,
    };
    if !(64..=maximum).contains(&memory_mb) {
        return Err(Error::Setup(format!(
            "machine {name}: memoryMiB must be 64..{maximum}"
        )));
    }
    Ok(())
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

pub use crate::driver_preparation::validate_oci_runtime;

#[cfg(test)]
mod test_sdk;
#[cfg(test)]
mod tests;
