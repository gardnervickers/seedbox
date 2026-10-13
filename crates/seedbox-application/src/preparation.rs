//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::limits::read_bounded;
use crate::{
    config::Simulation,
    execution::{Backend, ExecutionProfile, PreparationBackend, PreparedInputs},
    guest::{build_initramfs_with, launch_recipe, minimum_memory_mb},
    store::ContentStore,
    Error, Result,
};
use std::{collections::BTreeMap, fs, path::PathBuf};

const CONFIG_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct OciPreparer {
    pub kernel: PathBuf,
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

impl PreparationBackend for OciPreparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        crate::guest::validate_progress(
            &self.progress,
            crate::guest::Architecture::from_profile(&self.profile)?,
        )?;
        if self.profile.backend == Backend::Kvm {
            crate::admission::validate_profile(&self.profile)?;
        }
        Ok(())
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        self.validate_runtime_inputs()?;
        self.prepare_inner(
            simulation,
            store,
            None,
            &crate::container_images::ContainerTransport {
                docker: self.docker.clone(),
                cancellation: Default::default(),
                progress: |_| {},
                export_cache: true,
            },
        )
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        context.check()?;
        self.validate_runtime_inputs()?;
        self.prepare_inner(
            simulation,
            store,
            Some(context),
            &crate::container_images::ContainerTransport {
                docker: self.docker.clone(),
                cancellation: context.cancellation.clone(),
                progress: |_| {},
                export_cache: true,
            },
        )
    }
}
impl OciPreparer {
    /// Use an explicit image transport while sharing launch, retention and
    /// boot-archive construction with native Linux preparation.
    pub fn prepare_with_transport(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
        transport: &dyn crate::oci::ImageTransport,
    ) -> Result<PreparedInputs> {
        self.validate_runtime_inputs()?;
        self.prepare_inner(simulation, store, Some(context), transport)
    }
    fn prepare_inner(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: Option<&PreparationContext>,
        transport: &dyn crate::oci::ImageTransport,
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
        if let Some(context) = context {
            context.check()?;
        }
        if self.profile.backend == Backend::Kvm {
            crate::admission::validate_profile(&self.profile)?;
        }
        for (name, service) in &simulation.services {
            validate_machine_memory(
                &self.profile,
                name,
                service
                    .memory_mb
                    .unwrap_or(self.profile.semantic_parameters.memory_mb),
            )?;
        }
        let kernel = store.put_file(&self.kernel)?;
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
        auxiliary.insert("guest-progress".into(), store.put_file(&self.progress)?);
        if volumes {
            let mke2fs = self.mke2fs.as_ref().ok_or_else(|| {
                Error::Setup(
                    "named volumes need a static mke2fs to format the guest disk; enter \
                     `nix develop`, or pass --mke2fs or set SEEDBOX_MKE2FS"
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
        if let Some(context) = context {
            context.setup_inputs(&auxiliary, &kernel)?;
        }
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
        let init = store.materialize(&auxiliary["guest-init"], &temporary.path().join("init"))?;
        architecture.validate_elf(&init)?;
        let progress = store.materialize(
            &auxiliary["guest-progress"],
            &temporary.path().join("progress.tar"),
        )?;
        crate::guest::validate_progress(&progress, architecture)?;
        let mke2fs = auxiliary
            .get("guest-mke2fs")
            .map(|digest| store.materialize(digest, &temporary.path().join("mke2fs")))
            .transpose()?;
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        let mut imported_sources = BTreeMap::new();
        let mut launches = Vec::new();
        for (name, service) in &simulation.services {
            if let Some(context) = context {
                context.progress(name, PreparationStage::Image);
            }
            if let Some(context) = context {
                context.check()?;
            }
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
                    context.map(|c| (c, name.as_str())),
                    architecture,
                )
            })?
            .for_service(service, store)?;
            if let Some(context) = context {
                context.check()?;
            }
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
        for (name, service, mounts, imported) in launches {
            if let Some(context) = context {
                context.progress(name, PreparationStage::Guest);
            }
            let recipe = launch_recipe(
                &imported.image.launch,
                &format!("{}/24", addresses[name]),
                &hosts,
                name,
                &mounts,
                wall_clock_epoch,
            )?;
            let archive = temporary.path().join(format!("{name}.cpio"));
            // Package only retained bootstrap bytes. The source paths may be
            // replaced by another build while an image is being downloaded.
            let mke2fs = mke2fs.as_deref().filter(|_| !service.volumes.is_empty());
            match &imported.rootfs {
                crate::oci::RootFilesystem::Directory(root) => {
                    build_initramfs_with(root, &init, mke2fs, Some(&progress), &recipe, &archive)?
                }
                crate::oci::RootFilesystem::Archive(root) => {
                    crate::guest::build_initramfs_from_tar(
                        root,
                        &init,
                        mke2fs,
                        Some(&progress),
                        &recipe,
                        &archive,
                    )?
                }
            }
            let memory_mb = service
                .memory_mb
                .unwrap_or(self.profile.semantic_parameters.memory_mb);
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory_mb < minimum {
                return Err(Error::Execution(format!("service {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase memoryMiB for this machine (legacy Compose: x-seedbox.machine.memory_mb; application memory may require more)")));
            }
            if let Some(context) = context {
                context.check()?;
            }
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
            if let Some(context) = context {
                context.progress(name, PreparationStage::Ready);
            }
        }
        Ok(PreparedInputs {
            machine_memory_mb: simulation
                .services
                .iter()
                .filter_map(|(name, service)| {
                    service
                        .memory_mb
                        .filter(|memory| *memory != self.profile.semantic_parameters.memory_mb)
                        .map(|memory| (name.clone(), memory))
                })
                .collect(),
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

pub(crate) fn validate_machine_memory(
    profile: &ExecutionProfile,
    name: &str,
    memory_mb: u32,
) -> Result<()> {
    let maximum = if profile.backend == Backend::Hvf {
        3072
    } else {
        65536
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
