//! Driver boot assembly and retention of explicit prepared factory inputs.
//! The application supplies logical filesystem bytes and resolved launches; this
//! module neither resolves OCI images nor imports application configuration.
use crate::{
    config::Simulation,
    execution::PreparedInputs,
    runtime_assets::Inputs,
    store::{ContentDigest, ContentStore},
    Error, Result,
};
use seedbox_engine_protocol::driver_world as wire;
use std::{collections::BTreeMap, fs, path::Path};

mod cache;
pub use cache::PreparedBootCache;

fn artifact(path: &Path) -> Result<wire::Artifact> {
    let bytes = fs::read(path)?;
    Ok(wire::Artifact {
        sha256: ContentDigest::of(&bytes).hex().into(),
        length: bytes.len() as u64,
        artifact: path
            .to_str()
            .ok_or_else(|| Error::Integrity("Driver input path is not UTF-8".into()))?
            .into(),
    })
}
pub const FILESYSTEM: &str = "driver-world/filesystem/";

pub struct OciBoot<'a> {
    pub name: &'a str,
    pub filesystem: Vec<u8>,
    pub launch: &'a seedbox_artifacts::image::Launch,
    pub simulation: &'a Simulation,
    pub configs: &'a BTreeMap<String, Vec<u8>>,
    pub hosts: &'a str,
}
pub struct BootArchive {
    pub bytes: Vec<u8>,
    pub filesystem: ContentDigest,
}
#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64")
))]
enum BootFactory {
    Pv(seedbox_machine_pv::machine::pv::Factory),
    Tcg(seedbox_machine_tcg::machine::tcg::Factory),
}
pub struct OciBootFactory {
    #[cfg(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    ))]
    factory: BootFactory,
}
impl OciBootFactory {
    pub fn new(
        engine: &Path,
        runtime: &Inputs,
        epoch: u32,
        blocks: seedbox_spec::progress::ProgressBudget,
    ) -> Result<Self> {
        #[cfg(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        ))]
        {
            let factory = if let Some(qemu) = &runtime.qemu {
                BootFactory::Tcg(
                    seedbox_machine_tcg::machine::tcg::Factory::new(
                        qemu,
                        &runtime.guest_assets,
                        epoch,
                    )
                    .map_err(|error| Error::Setup(format!("TCG bootstrap: {error}")))?,
                )
            } else {
                BootFactory::Pv(
                    seedbox_machine_pv::machine::pv::Factory::new(
                        engine,
                        &runtime.guest_assets,
                        epoch,
                    )?
                    .with_progress_budget(blocks),
                )
            };
            Ok(Self { factory })
        }
        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        )))]
        {
            let _ = (engine, runtime, epoch, blocks);
            Err(Error::Setup(super::policy::PV_INCOMPLETE.into()))
        }
    }
    pub fn boot(&self, request: OciBoot<'_>, store: &ContentStore) -> Result<BootArchive> {
        #[cfg(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        ))]
        {
            use seedbox_machine::{
                application::{GuestEnvironment, NamedVolume},
                definition::{
                    DiskImage, FilesystemArchive, Launch, MacAddress, MachineDefinition,
                    NetworkInterface, PreparedApplication, Resources,
                },
            };
            let bytes = request.filesystem;
            let filesystem =
                FilesystemArchive::new(bytes).map_err(|e| Error::Integrity(e.to_string()))?;
            let retained_filesystem = store.put(filesystem.as_bytes())?;
            let launch = request.launch;
            let volumes = super::volume_setup(launch.disk_volumes.iter())
                .map_err(|e| Error::Integrity(format!("machine volumes: {e}")))?;
            let index = request
                .simulation
                .services
                .keys()
                .position(|name| name == request.name)
                .ok_or_else(|| {
                    Error::Integrity("prepared machine has no network address".into())
                })?;
            let number = u8::try_from(index + 1).map_err(|e| Error::Integrity(e.to_string()))?;
            let network = NetworkInterface::new(
                MacAddress::new([2, 0, 0, 0, 0, number])
                    .map_err(|e| Error::Integrity(e.to_string()))?,
                std::net::Ipv4Addr::new(10, 0, 0, number),
                24,
            )
            .map_err(|e| Error::Integrity(e.to_string()))?;
            let definition = MachineDefinition {
                name: request.name.into(),
                resources: Resources {
                    memory_bytes: u64::from(
                        request
                            .simulation
                            .services
                            .get(request.name)
                            .and_then(|service| service.memory_mb)
                            .unwrap_or(request.simulation.machine.memory_mb),
                    ) * 1024
                        * 1024,
                },
                application: PreparedApplication {
                    filesystem,
                    launch: Launch {
                        argv: launch.argv.clone(),
                        environment: launch.environment.clone(),
                        working_directory: launch.working_dir.clone(),
                        user: launch.user.clone(),
                    },
                },
                disk: volumes
                    .disk_bytes
                    .map(|bytes| {
                        DiskImage::zeroed(bytes).map_err(|e| Error::Integrity(e.to_string()))
                    })
                    .transpose()?,
                network: Some(network),
            };
            let mut environment = GuestEnvironment::new(
                request.name,
                request.hosts,
                request
                    .simulation
                    .control
                    .as_ref()
                    .is_some_and(|control| control.service == request.name),
            );
            environment.anonymous_volumes = launch.volumes.clone();
            environment.named_volumes = volumes
                .named_volumes
                .into_iter()
                .map(|volume| NamedVolume {
                    name: volume.name,
                    target: volume.target,
                })
                .collect();
            environment.configs = request.configs.clone();
            let bytes = match &self.factory {
                BootFactory::Pv(factory) => {
                    factory.boot_archive_configured(&definition, &environment)?
                }
                BootFactory::Tcg(factory) => factory
                    .boot_archive_configured(&definition, &environment)
                    .map_err(|error| Error::Execution(format!("TCG boot preparation: {error}")))?,
            };
            Ok(BootArchive {
                bytes,
                filesystem: retained_filesystem,
            })
        }
        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64")
        )))]
        {
            let OciBoot {
                name,
                filesystem,
                launch,
                simulation,
                configs,
                hosts,
            } = request;
            let _ = (name, filesystem, launch, simulation, configs, hosts, store);
            Err(Error::Setup(super::policy::PV_INCOMPLETE.into()))
        }
    }
}
/// Retain complete OCI boot archives, their logical filesystem views, and all
/// stateless future-factory authority. No guest is constructed or executed.
pub fn retain_oci_prepared(
    inputs: &mut PreparedInputs,
    simulation: &Simulation,
    runtime: &Inputs,
    store: &ContentStore,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    super::retained::validate_contract(&inputs.profile)?;
    check()?;
    let scratch = tempfile::tempdir()?;
    let engine = store.materialize(inputs.executable()?, &scratch.path().join("engine"))?;
    let root = &runtime.guest_assets;
    let mut hosts = "127.0.0.1 localhost\n::1 localhost\n".to_owned();
    for (index, name) in simulation.services.keys().enumerate() {
        hosts.push_str(&format!("10.0.0.{} {name}\n", index + 1));
    }
    let mut machines = BTreeMap::new();
    let mut environments = BTreeMap::new();
    let mut boots = BTreeMap::new();
    for (index, (name, service)) in simulation.services.iter().enumerate() {
        check()?;
        let image = inputs
            .images
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("prepared image missing for {name}")))?;
        let volumes = super::volume_setup(image.launch.disk_volumes.iter())
            .map_err(|e| Error::Integrity(format!("machine volumes: {e}")))?;
        let boot = inputs
            .guests
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("prepared boot missing for {name}")))?;
        let boot = store.materialize(boot, &scratch.path().join("boot").join(name))?;
        let bytes = fs::read(&boot)?;
        if !super::boot::complete(&bytes) {
            return Err(Error::Integrity(
                "OCI preparation did not produce a complete Linux boot archive".into(),
            ));
        }
        let memory_bytes = u64::from(inputs.memory_mb(name)?) * 1024 * 1024;
        if memory_bytes < super::boot::minimum_memory(bytes.len() as u64)? {
            return Err(Error::Execution(format!(
                "service {name}: guest RAM is below the actual Driver archive boot floor"
            )));
        }
        let filesystem = inputs
            .auxiliary
            .get(&format!("{FILESYSTEM}{name}"))
            .ok_or_else(|| {
                Error::Integrity("normal Driver OCI filesystem was not retained".into())
            })?;
        let filesystem =
            store.materialize(filesystem, &scratch.path().join("filesystem").join(name))?;
        let view = artifact(&filesystem)?;
        boots.insert(name.clone(), artifact(&boot)?);
        let mut configs = BTreeMap::new();
        for mount in &service.configs {
            let digest = inputs
                .auxiliary
                .get(&format!("config/{}", mount.source))
                .ok_or_else(|| Error::Integrity("prepared config content missing".into()))?;
            let path =
                store.materialize(digest, &scratch.path().join("configs").join(&mount.source))?;
            configs.insert(mount.target.clone(), artifact(&path)?);
        }
        let limits = seedbox_machine::application::GuestLimits::default();
        environments.insert(
            name.clone(),
            wire::Environment {
                hostname: name.clone(),
                hosts: hosts.clone(),
                anonymous_volumes: image.launch.volumes.clone(),
                named_volumes: volumes.named_volumes,
                configs,
                limits: wire::GuestLimits {
                    max_map_count: limits.max_map_count,
                    nofile: limits.nofile,
                },
                control_socket: simulation
                    .control
                    .as_ref()
                    .is_some_and(|control| control.service == *name),
            },
        );
        let number = u8::try_from(index + 1)
            .map_err(|_| Error::Integrity("too many prepared network machines".into()))?;
        machines.insert(
            name.clone(),
            wire::Machine {
                name: name.clone(),
                memory_bytes,
                filesystem: view,
                process: wire::Process {
                    argv: image.launch.argv.clone(),
                    environment: image.launch.environment.clone(),
                    working_directory: image.launch.working_dir.clone(),
                    user: image.launch.user.clone(),
                },
                disk: volumes.disk_bytes.map(|bytes| wire::Disk::Zero { bytes }),
                interface: Some(wire::Interface {
                    mac: [2, 0, 0, 0, 0, number],
                    address: std::net::Ipv4Addr::new(10, 0, 0, number),
                    prefix_len: 24,
                }),
            },
        );
    }
    let oracle = machines
        .remove(&simulation.check.service)
        .ok_or_else(|| Error::Integrity("prepared oracle machine missing".into()))?;
    let inventory = if let Some(qemu) = &runtime.qemu {
        wire::Inventory::Tcg {
            policy: None,
            executable: artifact(qemu)?,
            kernel: artifact(&root.join("kernel"))?,
            init: artifact(&root.join("init"))?,
            command_line: artifact(&root.join("command-line"))?,
            firmware: artifact(&root.join("firmware"))?,
            runtime: artifact(&runtime.archive())?,
            formatter: Some(artifact(&root.join("mke2fs"))?),
            environments,
            boot_archives: boots,
            epoch_seconds: inputs
                .wall_clock_epoch
                .ok_or_else(|| Error::Integrity("prepared realtime epoch missing".into()))?,
        }
    } else {
        wire::Inventory::Pv {
            policy: None,
            executable: artifact(&engine)?,
            kernel: artifact(&root.join("kernel"))?,
            init: artifact(&root.join("init"))?,
            command_line: artifact(&root.join("command-line"))?,
            progress: artifact(&root.join("progress.tar"))?,
            runtime: artifact(&runtime.archive())?,
            formatter: Some(artifact(&root.join("mke2fs"))?),
            environments,
            boot_archives: boots,
            progress_budget: simulation.simulator.progress_budget,
            epoch_seconds: inputs
                .wall_clock_epoch
                .ok_or_else(|| Error::Integrity("prepared realtime epoch missing".into()))?,
        }
    };
    let prepared = wire::Prepared {
        inventory,
        oracle,
        machines: machines.into_values().collect(),
        spawnable: vec![],
        quantum_ns: simulation.simulator.quantum_ns.get(),
        network: wire::Network {
            latency_ns: simulation.network.latency_ns,
            jitter_ns: simulation.network.jitter_ns,
            loss_ppm: 0,
            bandwidth_bps: simulation.network.bandwidth_bps,
            queue_bound_ns: simulation.network.queue_bound_ns,
            links: simulation
                .network
                .links
                .iter()
                .map(|link| wire::Link {
                    from: link.from.clone(),
                    to: link.to.clone(),
                    latency_ns: link.latency_ns,
                    jitter_ns: link.jitter_ns,
                    loss_ppm: link.loss_ppm,
                    bandwidth_bps: link.bandwidth_bps,
                    queue_bound_ns: link.queue_bound_ns,
                })
                .collect(),
        },
    };
    check()?;
    super::retain_prepared(inputs, &prepared, store)?;
    inputs.auxiliary.retain(|name, _| {
        !matches!(
            name.as_str(),
            "guest-init" | "guest-progress" | "guest-firmware" | "guest-mke2fs"
        )
    });
    check()
}
