//! Prepare retained guest archives from completed images without a builder.

use super::{validate_config_target, OciPreparer, PreparationContext};
use crate::{
    config::{ImageSource, Simulation},
    execution::{PreparationBackend, PreparedInputs},
    guest::{build_initramfs_image, launch_recipe, minimum_memory_mb},
    oci::{
        native::{ImageInput, NativeOci},
        GuestPlatform,
    },
    store::ContentStore,
    Error, Result,
};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
};

/// Owned transport overrides, keyed by Compose service name. A missing override
/// selects the service's registry reference. Clients only export finished images.
#[derive(Clone, Debug)]
pub enum NativeSource {
    Registry {
        reference: Option<String>,
    },
    OciLayout {
        path: PathBuf,
        reference: Option<String>,
    },
    OciArchive {
        path: PathBuf,
        reference: Option<String>,
    },
    DockerArchive {
        path: PathBuf,
        reference: Option<String>,
    },
    Docker {
        client: PathBuf,
        reference: String,
    },
    Podman {
        client: PathBuf,
        reference: String,
    },
}
impl NativeSource {
    fn input<'a>(&'a self, fallback: &'a str) -> ImageInput<'a> {
        match self {
            Self::Registry { reference } => {
                ImageInput::Registry(reference.as_deref().unwrap_or(fallback))
            }
            Self::OciLayout { path, reference } => ImageInput::OciLayout {
                path,
                reference: reference.as_deref(),
            },
            Self::OciArchive { path, reference } => ImageInput::OciArchive {
                path,
                reference: reference.as_deref(),
            },
            Self::DockerArchive { path, reference } => ImageInput::DockerArchive {
                path,
                reference: reference.as_deref(),
            },
            Self::Docker { client, reference } => ImageInput::Docker { client, reference },
            Self::Podman { client, reference } => ImageInput::Podman { client, reference },
        }
    }
}

pub struct NativePreparer {
    pub runtime: OciPreparer,
    pub importer: NativeOci,
    pub platform: GuestPlatform,
    pub sources: BTreeMap<String, NativeSource>,
}

impl PreparationBackend for NativePreparer {
    fn validate_runtime_inputs(&self) -> Result<()> {
        self.runtime.validate_runtime_inputs()?;
        // The legacy seedbox-kvm executable implements only x86 machines;
        // optional architecture metadata cannot make it an ARM64 backend.
        if self.runtime.profile.backend == "seedbox-kvm"
            && self.platform != GuestPlatform::LinuxAmd64
        {
            return Err(Error::Integrity(format!(
                "seedbox-kvm requires linux/amd64 guests; native preparation selected {}",
                self.platform.as_str()
            )));
        }
        if crate::admission::file_digest(&self.runtime.engine)? != self.runtime.profile.engine {
            return Err(Error::Integrity(
                "VMM changed after profile construction".into(),
            ));
        }
        // Kernel bytes are checked before image transport, including on platforms
        // whose execution admission belongs to a separate backend.
        crate::admission::file_digest(&self.runtime.kernel)?;
        guest_elf(&self.runtime.init, self.platform)?;
        guest_elf(&self.runtime.busybox, self.platform)?;
        memory_mb(&self.runtime)?;
        for (key, value) in &self.runtime.profile.semantic_parameters {
            let expected = match key.as_str() {
                "guest_architecture" => Some(self.platform.architecture()),
                "guest_platform" => Some(self.platform.as_str()),
                _ => None,
            };
            if expected.is_some_and(|expected| expected != value) {
                return Err(Error::Integrity(format!(
                    "native preparation platform conflicts with {key}={value}"
                )));
            }
        }
        Ok(())
    }

    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
        self.prepare_inner(simulation, store, None)
    }

    fn prepare_cancellable(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: &PreparationContext,
    ) -> Result<PreparedInputs> {
        self.prepare_inner(simulation, store, Some(context))
    }
}

impl NativePreparer {
    fn prepare_inner(
        &self,
        simulation: &Simulation,
        store: &ContentStore,
        context: Option<&PreparationContext>,
    ) -> Result<PreparedInputs> {
        let check = || context.map_or(Ok(()), PreparationContext::check);
        check()?;
        self.validate_runtime_inputs()?;
        self.validate_simulation(simulation)?;
        let kernel = store.put_file_checked(&self.runtime.kernel, &check)?;
        if self.runtime.profile.backend == "seedbox-kvm" {
            crate::admission::validate_artifacts(&self.runtime.profile, &kernel)?;
        }
        let engine = store.put_file_checked(&self.runtime.engine, &check)?;
        if engine != self.runtime.profile.engine {
            return Err(Error::Integrity(
                "VMM changed after profile construction".into(),
            ));
        }
        let mut auxiliary = BTreeMap::from([
            (
                "guest-init".into(),
                store.put_file_checked(&self.runtime.init, &check)?,
            ),
            (
                "guest-busybox".into(),
                store.put_file_checked(&self.runtime.busybox, &check)?,
            ),
        ]);
        let volumes = simulation
            .services
            .values()
            .any(|service| !service.volumes.is_empty());
        if volumes {
            let mke2fs = self.runtime.mke2fs.as_ref().ok_or_else(|| Error::Setup(
                "named volumes need a static mke2fs for the selected guest architecture; supply the guest mke2fs helper".into()
            ))?;
            guest_elf(mke2fs, self.platform)?;
            auxiliary.insert(
                "guest-mke2fs".into(),
                store.put_file_checked(mke2fs, &check)?,
            );
        }
        let mut configs = BTreeMap::new();
        for (name, path) in &simulation.configs {
            check()?;
            let mut data = Vec::new();
            fs::File::open(path)
                .map_err(|error| {
                    Error::Setup(format!(
                        "config {name} {}: {error}; restore the declared config file",
                        path.display()
                    ))
                })?
                .take(16 * 1024 * 1024 + 1)
                .read_to_end(&mut data)?;
            if data.len() > 16 * 1024 * 1024 {
                return Err(Error::Execution(format!("config {name} exceeds 16 MiB")));
            }
            auxiliary.insert(format!("config/{name}"), store.put(&data)?);
            configs.insert(name, data);
        }
        check()?;
        if let Some(context) = context {
            context.setup_inputs(&auxiliary, &engine, &kernel)?;
        }
        let temporary = tempfile::tempdir()?;
        let init = store.materialize_checked(
            &auxiliary["guest-init"],
            &temporary.path().join("init"),
            &check,
        )?;
        let busybox = store.materialize_checked(
            &auxiliary["guest-busybox"],
            &temporary.path().join("busybox"),
            &check,
        )?;
        guest_elf(&init, self.platform)?;
        guest_elf(&busybox, self.platform)?;
        let mke2fs = auxiliary
            .get("guest-mke2fs")
            .map(|digest| {
                store.materialize_checked(digest, &temporary.path().join("mke2fs"), &check)
            })
            .transpose()?;
        if let Some(path) = &mke2fs {
            guest_elf(path, self.platform)?;
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
        let mut images = BTreeMap::new();
        let mut guests = BTreeMap::new();
        let mut provenance =
            BTreeMap::from([("guest-platform".into(), self.platform.as_str().into())]);
        for (index, (name, service)) in simulation.services.iter().enumerate() {
            check()?;
            let ImageSource::Registry { reference } = &service.source else {
                return Err(build_error(name));
            };
            let input = self
                .sources
                .get(name)
                .map_or(ImageInput::Registry(reference), |source| {
                    source.input(reference)
                });
            let imported = self.importer.import(
                input,
                service,
                &temporary.path().join(format!("image-{index}")),
                store,
                self.platform,
                context,
            )?;
            check()?;
            let mounts = service
                .configs
                .iter()
                .map(|mount| {
                    Ok((
                        mount.target.clone(),
                        configs
                            .get(&mount.source)
                            .ok_or_else(|| Error::Integrity("unknown config source".into()))?
                            .clone(),
                    ))
                })
                .collect::<Result<BTreeMap<_, _>>>()?;
            let recipe = launch_recipe(
                &imported.image.launch,
                &format!("{}/24", addresses[name]),
                &hosts,
                &mounts,
            )?;
            let archive = temporary.path().join(format!("guest-{index}.cpio"));
            build_initramfs_image(
                &imported.filesystem,
                &init,
                &busybox,
                mke2fs.as_deref().filter(|_| !service.volumes.is_empty()),
                &recipe,
                &archive,
                &check,
            )?;
            let memory = memory_mb(&self.runtime)?;
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory < minimum {
                return Err(Error::Execution(format!("service {name}: {memory} MiB guest RAM is below the archive boot floor of {minimum} MiB; increase --memory-mb (application memory may require more)")));
            }
            check()?;
            guests.insert(name.clone(), store.put_file_checked(&archive, &check)?);
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
        check()?;
        Ok(PreparedInputs {
            kernel,
            images,
            guests,
            placement: simulation
                .services
                .keys()
                .map(|name| (name.clone(), name.clone()))
                .collect(),
            profile: self.runtime.profile.clone(),
            auxiliary,
            provenance,
        })
    }

    fn validate_simulation(&self, simulation: &Simulation) -> Result<()> {
        for name in self.sources.keys() {
            if !simulation.services.contains_key(name) {
                return Err(Error::Setup(format!("image source override names unknown service {name}; use a declared Compose service")));
            }
        }
        for (name, service) in &simulation.services {
            if matches!(service.source, ImageSource::Build { .. }) {
                return Err(build_error(name));
            }
            if service.platform != self.platform.as_str() {
                return Err(Error::Setup(format!("service {name}: platform {} conflicts with guest {}; supply an image for the selected guest architecture", service.platform, self.platform.as_str())));
            }
            for mount in &service.volumes {
                validate_config_target(&mount.target).map_err(|_| Error::Validation {
                    path: format!("$.services.{name}.volumes"),
                    message: format!("reserved guest runtime path {}", mount.target),
                })?;
            }
            for mount in &service.configs {
                validate_config_target(&mount.target)?;
                if !simulation.configs.contains_key(&mount.source) {
                    return Err(Error::Integrity(format!(
                        "service {name}: unknown config source {}",
                        mount.source
                    )));
                }
            }
        }
        Ok(())
    }
}

fn build_error(name: &str) -> Error {
    Error::Setup(format!("service {name}: Seedbox consumes finished images and does not build Dockerfiles; build externally and supply an image: reference"))
}

fn memory_mb(runtime: &OciPreparer) -> Result<u32> {
    runtime
        .profile
        .semantic_parameters
        .get("memory_mb")
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|memory| (64..=65536).contains(memory))
        .ok_or_else(|| {
            Error::Integrity("native OCI profile requires memory_mb in 64..65536".into())
        })
}

fn guest_elf(path: &Path, platform: GuestPlatform) -> Result<()> {
    let mut header = [0; 20];
    fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .map_err(|error| {
            Error::Setup(format!(
                "guest helper {}: {error}; supply a static {} ELF helper",
                path.display(),
                platform.architecture()
            ))
        })?;
    let machine = match platform {
        GuestPlatform::LinuxAmd64 => 62,
        GuestPlatform::LinuxArm64 => 183,
    };
    if &header[..6] != b"\x7fELF\x02\x01" || u16::from_le_bytes([header[18], header[19]]) != machine
    {
        return Err(Error::Integrity(format!(
            "expected an {} ELF guest helper: {}",
            platform.architecture(),
            path.display()
        )));
    }
    Ok(())
}
