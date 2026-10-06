//! OCI-backed preparation. Runtime binaries are explicit inputs, so library
//! callers do not depend on CLI environment or a particular checkout layout.

use crate::{
    config::Simulation,
    execution::{ExecutionProfile, Layout, PreparationBackend, PreparedInputs},
    guest::{build_initramfs, launch_recipe, minimum_memory_mb, GuestNetwork, GuestService},
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

impl PreparationBackend for OciPreparer {
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs> {
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
        let layout = self
            .profile
            .semantic_parameters
            .get("layout")
            .and_then(|value| Layout::parse(value))
            .ok_or_else(|| Error::Integrity("OCI profile missing layout".into()))?;
        let memory_mb = self
            .profile
            .semantic_parameters
            .get("memory_mb")
            .and_then(|value| value.parse::<u32>().ok())
            .ok_or_else(|| Error::Integrity("OCI profile missing memory_mb".into()))?;
        // Both layouts use the same addresses and names, in service order.
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
        // One root per distinct image manifest, shared read-only by services.
        let mut roots: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut mounts = BTreeMap::new();
        let mut provenance = BTreeMap::new();
        for (name, service) in &simulation.services {
            mounts.insert(
                name,
                service
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
                    .collect::<Result<BTreeMap<_, _>>>()?,
            );
            let imported =
                self.tools
                    .import(service, &temporary.path().join("images").join(name), store)?;
            roots
                .entry(imported.image.manifest.hex().to_owned())
                .or_insert(imported.rootfs);
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
        let placement = layout.placement(simulation.services.keys());
        let network = match layout {
            Layout::SharedKernel => GuestNetwork::Namespaces {
                latency_ns: simulation.network.latency_ns,
                jitter_ns: simulation.network.jitter_ns,
            },
            Layout::MachinePerService => GuestNetwork::Device,
        };
        let mut guests = BTreeMap::new();
        for machine in placement
            .values()
            .collect::<std::collections::BTreeSet<_>>()
        {
            let hosted: Vec<_> = placement
                .iter()
                .filter(|(_, m)| *m == machine)
                .map(|(service, _)| service)
                .collect();
            let services = hosted
                .iter()
                .map(|name| GuestService {
                    name,
                    image: images[*name].manifest.hex(),
                    address: &addresses[name],
                    launch: &images[*name].launch,
                    configs: &mounts[name],
                    check: **name == simulation.check.service,
                    controller: simulation
                        .control
                        .as_ref()
                        .is_some_and(|control| control.service == **name),
                })
                .collect::<Vec<_>>();
            let recipe = launch_recipe(network, &hosts, &services)?;
            let archive = temporary.path().join(format!("{machine}.cpio"));
            let used: std::collections::BTreeSet<_> = hosted
                .iter()
                .map(|name| images[*name].manifest.hex())
                .collect();
            let trees: Vec<_> = used.iter().map(|id| (*id, roots[*id].as_path())).collect();
            let names: Vec<_> = hosted.iter().map(|name| name.as_str()).collect();
            // Package only retained bootstrap bytes. The source paths may be
            // replaced by another build while an image is being downloaded.
            build_initramfs(&trees, &names, &init, &busybox, &recipe, &archive)?;
            let minimum = minimum_memory_mb(fs::metadata(&archive)?.len())?;
            if memory_mb < minimum {
                return Err(Error::Execution(format!(
                    "machine {machine}: {memory_mb} MiB guest RAM for {} service(s) is below \
                     the archive boot floor of {minimum} MiB; increase --memory-mb (application \
                     memory may require more)",
                    hosted.len()
                )));
            }
            guests.insert(machine.clone(), store.put_file(&archive)?);
        }
        Ok(PreparedInputs {
            kernel: store.put_file(&self.kernel)?,
            images,
            guests,
            placement,
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
