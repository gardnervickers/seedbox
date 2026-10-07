//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::{
    config::Simulation,
    execution::{ExecutionProfile, PreparationBackend, PreparedInputs},
    guest::{build_initramfs, launch_recipe, minimum_memory_mb},
    oci::OciTools,
    store::ContentStore,
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct OciPreparer {
    pub tools: OciTools,
    pub kernel: PathBuf,
    pub engine: PathBuf,
    pub init: PathBuf,
    pub busybox: PathBuf,
    pub profile: ExecutionProfile,
}

pub use crate::preparation_command::PreparationContext;

impl PreparationBackend for OciPreparer {
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        self.prepare_inner(simulation, store, None)
    }
    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        context.check()?;
        if simulation
            .services
            .values()
            .any(|s| matches!(s.source, crate::config::ImageSource::Build { .. }))
        {
            context.admit_builder(&self.tools.docker)?;
        }
        self.prepare_inner(simulation, store, Some(context))
    }
}
impl OciPreparer {
    fn prepare_inner(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: Option<&PreparationContext>,
    ) -> Result<PreparedInputs> {
        if let Some(context) = context {
            context.check()?;
        }
        let engine = store.put_file(&self.engine)?;
        if engine != self.profile.engine {
            return Err(Error::Integrity(
                "VMM changed after profile construction".into(),
            ));
        }
        for path in [&self.init, &self.busybox] {
            amd64_elf(path)?;
        }
        let mut auxiliary = BTreeMap::from([
            ("guest-init".into(), store.put_file(&self.init)?),
            ("guest-busybox".into(), store.put_file(&self.busybox)?),
        ]);
        let mut configs = BTreeMap::new();
        for (name, path) in &simulation.configs {
            let mut data = Vec::new();
            fs::File::open(path)?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 16 * 1024 * 1024 {
                return Err(Error::Execution(format!("config {name} exceeds 16 MiB")));
            }
            auxiliary.insert(format!("config/{name}"), store.put(&data)?);
            configs.insert(name, data);
        }
        let frozen_kernel = if let Some(context) = context {
            let kernel = store.put_file(&self.kernel)?;
            context.setup_inputs(&auxiliary, &engine, &kernel)?;
            Some(kernel)
        } else {
            None
        };
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
        let busybox = store.materialize(
            &auxiliary["guest-busybox"],
            &temporary.path().join("busybox"),
        )?;
        amd64_elf(&init)?;
        amd64_elf(&busybox)?;
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for (name, service) in &simulation.services {
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
            let imported = self.tools.import_with_context(
                service,
                &temporary.path().join(name),
                store,
                context.map(|c| (c, name.as_str())),
            )?;
            if let Some(context) = context {
                context.check()?;
            }
            let recipe = launch_recipe(
                &imported.image.launch,
                &format!("{}/24", addresses[name]),
                &hosts,
                &mounts,
            )?;
            let archive = temporary.path().join(format!("{name}.cpio"));
            // Package only retained bootstrap bytes. The source paths may be
            // replaced by another build while an image is being downloaded.
            build_initramfs(&imported.rootfs, &init, &busybox, &recipe, &archive)?;
            let memory_mb = self
                .profile
                .semantic_parameters
                .get("memory_mb")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| Error::Integrity("OCI profile missing memory_mb".into()))?;
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory_mb < minimum {
                return Err(Error::Execution(format!("service {name}: {memory_mb} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase --memory-mb (application memory may require more)")));
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
        }
        Ok(PreparedInputs {
            kernel: match frozen_kernel {
                Some(kernel) => kernel,
                None => store.put_file(&self.kernel)?,
            },
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

fn amd64_elf(path: &Path) -> Result<()> {
    let mut header = [0; 20];
    fs::File::open(path)?.read_exact(&mut header)?;
    if &header[..6] != b"\x7fELF\x02\x01" || header[18..20] != [62, 0] {
        return Err(Error::Integrity(format!(
            "expected an amd64 ELF guest helper: {}",
            path.display()
        )));
    }
    Ok(())
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
