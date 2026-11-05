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

/// A host file the worker reads, by path, with the digest of its bytes now.
pub fn artifact(path: &Path) -> Result<wire::Artifact> {
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
/// An object the store already holds, by content as retention names inputs.
fn stored(store: &ContentStore, digest: &ContentDigest) -> Result<wire::Artifact> {
    Ok(wire::Artifact {
        sha256: digest.hex().into(),
        length: store.size(digest)?,
        artifact: format!("asset:{}", digest.hex()),
    })
}

/// The runtime files one preparation boots and retains, each stored once.
/// `put_file` rereads a file on every call, so a preparation builds this once
/// and its boot cache and retained inventory share the digests.
#[derive(Clone, Debug)]
pub struct RuntimeDigests {
    engine: ContentDigest,
    /// The complete package and each selected file, by its runtime name.
    files: BTreeMap<String, ContentDigest>,
    tcg: bool,
}
impl RuntimeDigests {
    /// Fingerprint bytes, never host paths or timestamps. Include selected
    /// files as well as the package: Inputs exposes their paths, and factories
    /// consume those files rather than rereading the package.
    pub fn new(engine: ContentDigest, runtime: &Inputs, store: &ContentStore) -> Result<Self> {
        let mut files = BTreeMap::from([("package".into(), store.put_file(&runtime.archive())?)]);
        for name in [
            "kernel",
            "init",
            "command-line",
            if runtime.qemu.is_some() {
                "firmware"
            } else {
                "progress.tar"
            },
        ] {
            files.insert(
                name.into(),
                store.put_file(&runtime.guest_assets.join(name))?,
            );
        }
        let formatter = runtime.guest_assets.join("mke2fs");
        match fs::metadata(&formatter) {
            Ok(_) => {
                files.insert("mke2fs".into(), store.put_file(&formatter)?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(qemu) = &runtime.qemu {
            files.insert("qemu".into(), store.put_file(qemu)?);
        }
        Ok(Self {
            engine,
            files,
            tcg: runtime.qemu.is_some(),
        })
    }
    pub fn engine(&self) -> &ContentDigest {
        &self.engine
    }
    /// Whether these are a TCG runtime's files, with QEMU and firmware.
    pub fn tcg(&self) -> bool {
        self.tcg
    }
    /// One stored file by its runtime name, such as `kernel` or `package`.
    pub fn file(&self, name: &str) -> Result<&ContentDigest> {
        self.files
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("Driver runtime has no {name}")))
    }
}
pub const FILESYSTEM: &str = "driver-world/filesystem/";
/// Interface prefix wide enough that the simulated nameserver (engine-core
/// `dns::ADDRESS`, 10.0.1.1) is on-link next to the 10.0.0.N machines.
pub const PREFIX_LEN: u8 = 16;

/// The machine's last address octet: services are numbered in name order.
pub fn machine_number(simulation: &Simulation, name: &str) -> Result<u8> {
    let index = simulation
        .services
        .keys()
        .position(|service| service == name)
        .ok_or_else(|| Error::Integrity("prepared machine has no network address".into()))?;
    u8::try_from(index + 1)
        .map_err(|_| Error::Integrity("too many prepared network machines".into()))
}

/// The guest hosts file names only loopback and the machine itself; peers
/// resolve through the simulated nameserver so unspawned machines stay absent.
pub fn hosts_file(simulation: &Simulation, name: &str) -> Result<String> {
    Ok(hosts(name, machine_number(simulation, name)?))
}
fn hosts(name: &str, number: u8) -> String {
    format!("127.0.0.1 localhost\n::1 localhost\n10.0.0.{number} {name}\n")
}

/// Everything the worker builds one machine from.
pub struct MachineInputs<'a> {
    pub name: &'a str,
    /// The last octet of the machine's address and MAC.
    pub number: u8,
    pub memory_bytes: u64,
    /// The machine's logical filesystem view.
    pub filesystem: wire::Artifact,
    pub launch: &'a seedbox_artifacts::image::Launch,
    /// Each initial file's guest path and its contents.
    pub configs: BTreeMap<String, wire::Artifact>,
    pub allow_crashes: bool,
    pub control_socket: bool,
}
impl MachineInputs<'_> {
    pub fn wire(self) -> Result<(wire::Machine, wire::Environment)> {
        let volumes = super::volume_setup(self.launch.disk_volumes.iter())
            .map_err(|e| Error::Integrity(format!("machine volumes: {e}")))?;
        let limits = seedbox_machine::application::GuestLimits::default();
        let environment = wire::Environment {
            hostname: self.name.into(),
            hosts: hosts(self.name, self.number),
            anonymous_volumes: self.launch.volumes.clone(),
            named_volumes: volumes.named_volumes,
            configs: self.configs,
            limits: wire::GuestLimits {
                max_map_count: limits.max_map_count,
                nofile: limits.nofile,
            },
            control_socket: self.control_socket,
        };
        let machine = wire::Machine {
            name: self.name.into(),
            memory_bytes: self.memory_bytes,
            filesystem: self.filesystem,
            process: wire::Process {
                argv: self.launch.argv.clone(),
                environment: self.launch.environment.clone(),
                working_directory: self.launch.working_dir.clone(),
                user: self.launch.user.clone(),
            },
            disk: volumes.disk_bytes.map(|bytes| wire::Disk::Zero { bytes }),
            interface: Some(wire::Interface {
                mac: [2, 0, 0, 0, 0, self.number],
                address: std::net::Ipv4Addr::new(10, 0, 0, self.number),
                prefix_len: PREFIX_LEN,
            }),
            allow_crashes: self.allow_crashes,
        };
        Ok((machine, environment))
    }
}

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
            let number = machine_number(request.simulation, request.name)?;
            let network = NetworkInterface::new(
                MacAddress::new([2, 0, 0, 0, 0, number])
                    .map_err(|e| Error::Integrity(e.to_string()))?,
                std::net::Ipv4Addr::new(10, 0, 0, number),
                PREFIX_LEN,
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
/// Every input is already a store object, so the manifest names it by digest.
pub fn retain_oci_prepared(
    inputs: &mut PreparedInputs,
    simulation: &Simulation,
    runtime: &RuntimeDigests,
    store: &ContentStore,
    check: &dyn Fn() -> Result<()>,
) -> Result<()> {
    super::retained::validate_contract(&inputs.profile)?;
    check()?;
    let file = |name: &str| -> Result<wire::Artifact> { stored(store, runtime.file(name)?) };
    let mut machines = BTreeMap::new();
    let mut environments = BTreeMap::new();
    let mut boots = BTreeMap::new();
    for (name, service) in &simulation.services {
        check()?;
        let image = inputs
            .images
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("prepared image missing for {name}")))?;
        let boot = inputs
            .guests
            .get(name)
            .ok_or_else(|| Error::Integrity(format!("prepared boot missing for {name}")))?;
        if !super::boot::complete(&store.read_prefix(boot, 6)?) {
            return Err(Error::Integrity(
                "OCI preparation did not produce a complete Linux boot archive".into(),
            ));
        }
        let boot = stored(store, boot)?;
        let memory_bytes = u64::from(inputs.memory_mb(name)?) * 1024 * 1024;
        if memory_bytes < super::boot::minimum_memory(boot.length)? {
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
        boots.insert(name.clone(), boot);
        let mut configs = BTreeMap::new();
        for mount in &service.configs {
            let digest = inputs
                .auxiliary
                .get(&format!("config/{}", mount.source))
                .ok_or_else(|| Error::Integrity("prepared config content missing".into()))?;
            configs.insert(mount.target.clone(), stored(store, digest)?);
        }
        let (machine, environment) = MachineInputs {
            name,
            number: machine_number(simulation, name)?,
            memory_bytes,
            filesystem: stored(store, filesystem)?,
            launch: &image.launch,
            configs,
            allow_crashes: service.allow_crashes,
            control_socket: simulation
                .control
                .as_ref()
                .is_some_and(|control| control.service == *name),
        }
        .wire()?;
        environments.insert(name.clone(), environment);
        machines.insert(name.clone(), machine);
    }
    let oracle = machines
        .remove(&simulation.check.service)
        .ok_or_else(|| Error::Integrity("prepared oracle machine missing".into()))?;
    let inventory = if runtime.tcg() {
        wire::Inventory::Tcg {
            policy: None,
            executable: file("qemu")?,
            kernel: file("kernel")?,
            init: file("init")?,
            command_line: file("command-line")?,
            firmware: file("firmware")?,
            runtime: file("package")?,
            formatter: Some(file("mke2fs")?),
            environments,
            boot_archives: boots,
            epoch_seconds: inputs
                .wall_clock_epoch
                .ok_or_else(|| Error::Integrity("prepared realtime epoch missing".into()))?,
        }
    } else {
        wire::Inventory::Pv {
            policy: None,
            executable: stored(store, inputs.executable()?)?,
            kernel: file("kernel")?,
            init: file("init")?,
            command_line: file("command-line")?,
            progress: file("progress.tar")?,
            runtime: file("package")?,
            formatter: Some(file("mke2fs")?),
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
        spawned: vec![],
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
