//! The retained worker's closed native sum, not a backend registry.
#[cfg(test)]
mod tests;
use super::checkpoint::Opened;
use seedbox_engine_protocol::driver_world::Inventory;
use seedbox_machine::machine::{Machine, RunContext, StopReason, Timestamp};
use std::{
    io::{Read, Write},
    path::Path,
};

#[cfg(any(
    all(target_os = "linux", target_arch = "x86_64"),
    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
))]
use seedbox_machine_pv::machine::pv;
use seedbox_machine_tcg::machine::tcg;

pub(super) enum Native {
    #[cfg(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    Pv {
        name: String,
        machine: Box<pv::PVMachine>,
    },
    Tcg {
        name: String,
        machine: Box<tcg::TcgMachine>,
    },
}
pub(super) enum Snapshot {
    #[cfg(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    Pv(Box<pv::Snapshot>),
    Tcg(Box<tcg::Snapshot>),
    /// Only an already-verified descriptor, never a decoded native RAM image.
    Retained(Opened),
}
impl Machine for Native {
    type Context = ();
    type Error = String;
    type Snapshot = Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, String> {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv { machine, .. } => machine.run(context, upto).map_err(|e| e.to_string()),
            Self::Tcg { machine, .. } => machine.run(context, upto).map_err(|e| e.to_string()),
        }
    }
    fn execution_evidence(
        &mut self,
    ) -> Result<Option<seedbox_machine::machine::ExecutionEvidence>, String> {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv { machine, .. } => machine
                .execution_evidence()
                .map_err(|error| error.to_string()),
            Self::Tcg { machine, .. } => machine
                .execution_evidence()
                .map_err(|error| error.to_string()),
        }
    }
    fn state_evidence(
        &mut self,
    ) -> Result<Option<seedbox_machine::machine::StateEvidence>, String> {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv { machine, .. } => machine.state_evidence().map_err(|error| error.to_string()),
            Self::Tcg { machine, .. } => {
                machine.state_evidence().map_err(|error| error.to_string())
            }
        }
    }
    fn snapshot(&mut self) -> Result<Snapshot, String> {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv { machine, .. } => machine
                .snapshot()
                .map(|snapshot| Snapshot::Pv(Box::new(snapshot)))
                .map_err(|e| e.to_string()),
            Self::Tcg { machine, .. } => machine
                .snapshot()
                .map(|snapshot| Snapshot::Tcg(Box::new(snapshot)))
                .map_err(|e| e.to_string()),
        }
    }
}
impl Native {
    pub(super) fn name(&self) -> &str {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv { name, .. } => name,
            Self::Tcg { name, .. } => name,
        }
    }
    /// The descriptor-backed branch decodes exactly one body, restores the live
    /// target, then drops the decoded Snapshot before the next Driver callback.
    pub(super) fn restore(
        name: String,
        inventory: &Inventory,
        executable: &Path,
        snapshot: &Snapshot,
        limits: &seedbox_machine::snapshot::DecodeLimits,
        scratch_root: &Path,
    ) -> Result<Self, String> {
        super::policy::decode_optional(inventory.backend_policy())?;
        let decoded;
        let snapshot = if let Snapshot::Retained(file) = snapshot {
            decoded = Snapshot::read_from(inventory, &mut file.reader(), limits)?;
            &decoded
        } else {
            snapshot
        };
        match snapshot {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Snapshot::Pv(snapshot) => {
                let Inventory::Pv { policy, .. } = inventory else {
                    return Err("PV native body differs from retained factory backend".into());
                };
                let policy = super::policy::decode_optional(policy.as_ref())?;
                snapshot.validate_policy(&name, policy.as_ref())?;
                super::policy::admit_restore(inventory)?;
                // Linux ARM64 KVM restore is not proven; HVF restore is
                // compiled for bd-877 and verified on hardware separately.
                #[cfg(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_os = "macos", target_arch = "aarch64")
                ))]
                {
                    pv::PVMachine::from_snapshot_in(executable, snapshot, scratch_root).map(
                        |machine| Self::Pv {
                            name,
                            machine: Box::new(machine),
                        },
                    )
                }
                #[cfg(not(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_os = "macos", target_arch = "aarch64")
                )))]
                {
                    Err(super::policy::PV_INCOMPLETE.into())
                }
            }
            Snapshot::Tcg(snapshot) => {
                let Inventory::Tcg { policy, .. } = inventory else {
                    return Err("TCG native body differs from retained factory backend".into());
                };
                let policy = super::policy::decode_optional(policy.as_ref())?;
                snapshot
                    .validate_policy(&name, policy.as_ref())
                    .map_err(|error| error.to_string())?;
                super::policy::admit(inventory)?;
                tcg::TcgMachine::from_snapshot_in(executable, snapshot, scratch_root)
                    .map(|machine| Self::Tcg {
                        name,
                        machine: Box::new(machine),
                    })
                    .map_err(|e| e.to_string())
            }
            Snapshot::Retained(_) => Err("nested retained native snapshot descriptor".into()),
        }
    }
}
impl Snapshot {
    pub(super) fn write_to(&self, writer: &mut impl Write) -> Result<(), String> {
        match self {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            Self::Pv(snapshot) => snapshot.write_to(writer).map_err(|e| e.to_string()),
            Self::Tcg(snapshot) => snapshot.write_to(writer).map_err(|e| e.to_string()),
            Self::Retained(file) => std::io::copy(&mut file.reader(), writer)
                .map(|_| ())
                .map_err(|e| e.to_string()),
        }
    }
    fn read_from(
        inventory: &Inventory,
        reader: &mut impl Read,
        limits: &seedbox_machine::snapshot::DecodeLimits,
    ) -> Result<Self, String> {
        super::policy::decode_optional(inventory.backend_policy())?;
        match inventory {
            Inventory::Pv { .. } => {
                #[cfg(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
                ))]
                {
                    pv::Snapshot::read_from(reader, limits)
                        .map(|snapshot| Self::Pv(Box::new(snapshot)))
                        .map_err(|e| e.to_string())
                }
                #[cfg(not(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
                )))]
                {
                    Err("PV snapshot restore is unavailable on this host".into())
                }
            }
            Inventory::Tcg { .. } => tcg::Snapshot::read_from(reader, limits)
                .map(|snapshot| Self::Tcg(Box::new(snapshot)))
                .map_err(|e| e.to_string()),
        }
    }
}

fn environment(
    retained: &seedbox_engine_protocol::driver_world::Environment,
) -> Result<seedbox_machine::application::GuestEnvironment, String> {
    use seedbox_machine::application::{GuestEnvironment, GuestLimits, NamedVolume};
    let mut configs = std::collections::BTreeMap::new();
    for (target, artifact) in &retained.configs {
        let file = super::checkpoint::verified(artifact).map_err(|error| error.to_string())?;
        let length = usize::try_from(artifact.length)
            .map_err(|_| "config artifact exceeds host address space".to_owned())?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| "config artifact allocation failed".to_owned())?;
        file.take(artifact.length.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() != length {
            return Err("config artifact changed after verification".into());
        }
        configs.insert(target.clone(), bytes);
    }
    Ok(GuestEnvironment {
        hostname: retained.hostname.clone(),
        hosts: retained.hosts.clone(),
        anonymous_volumes: retained.anonymous_volumes.clone(),
        named_volumes: retained
            .named_volumes
            .iter()
            .map(|volume| NamedVolume {
                name: volume.name.clone(),
                target: volume.target.clone(),
            })
            .collect(),
        configs,
        limits: GuestLimits {
            max_map_count: retained.limits.max_map_count,
            nofile: retained.limits.nofile,
        },
        control_socket: retained.control_socket,
    })
}

pub(super) struct Factory {
    inner: FactoryKind,
    // Selected paths include QEMU's adjacent loader/libraries. This ownership
    // must survive every machine and future factory call, not just loading.
    // None only for a launch-only binding whose parent retains Inputs through
    // worker/group shutdown. It is never authority recovered from a checkpoint.
    _runtime: Option<crate::runtime_assets::Inputs>,
    environments:
        std::collections::BTreeMap<String, seedbox_engine_protocol::driver_world::Environment>,
    boot_archives:
        std::collections::BTreeMap<String, seedbox_engine_protocol::driver_world::Artifact>,
}
fn check_boot_memory(
    definition: &seedbox_machine::definition::MachineDefinition,
    bytes: &[u8],
) -> Result<(), String> {
    let floor =
        super::boot::minimum_memory(bytes.len() as u64).map_err(|error| error.to_string())?;
    if definition.resources.memory_bytes < floor {
        return Err(format!("machine {}: {} bytes RAM is below {} bytes required by its actual {}-byte complete boot (archive * 3 + 128 MiB); increase machine memory", definition.name, definition.resources.memory_bytes, floor, bytes.len()));
    }
    Ok(())
}

enum FactoryKind {
    #[cfg(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
    ))]
    Pv(pv::Factory),
    Tcg(tcg::Factory),
}
impl Factory {
    pub(super) fn load(
        inventory: &Inventory,
        executable: &Path,
        guest_assets: &Path,
        runtime: Option<crate::runtime_assets::Inputs>,
        scratch_root: &Path,
    ) -> Result<Self, String> {
        super::policy::admit(inventory)?;
        let inner = match inventory {
            Inventory::Pv {
                epoch_seconds,
                blocks_per_progress_call,
                policy,
                ..
            } => {
                #[cfg(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
                ))]
                {
                    let factory = pv::Factory::new(executable, guest_assets, *epoch_seconds)
                        .map_err(|error| error.to_string())?;
                    let factory = match policy {
                        Some(policy) => factory
                            .with_policy(super::policy::decode(policy)?)
                            .map_err(|error| error.to_string())?,
                        None => factory,
                    };
                    Ok::<FactoryKind, String>(FactoryKind::Pv(
                        factory
                            .with_scratch_root(scratch_root)
                            .with_blocks_per_progress_call(*blocks_per_progress_call),
                    ))
                }
                #[cfg(not(any(
                    all(target_os = "linux", target_arch = "x86_64"),
                    all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
                )))]
                {
                    Err("PV machine construction is unavailable on this host".into())
                }
            }
            Inventory::Tcg {
                epoch_seconds,
                policy,
                ..
            } => {
                let factory = tcg::Factory::new(executable, guest_assets, *epoch_seconds)
                    .map_err(|error| error.to_string())?;
                let factory = match policy {
                    Some(policy) => factory
                        .with_policy(super::policy::decode(policy)?)
                        .map_err(|error| error.to_string())?,
                    None => factory,
                };
                Ok(FactoryKind::Tcg(factory.with_scratch_root(scratch_root)))
            }
        }?;
        Ok(Self {
            inner,
            _runtime: runtime,
            boot_archives: match inventory {
                Inventory::Pv { boot_archives, .. } | Inventory::Tcg { boot_archives, .. } => {
                    boot_archives.clone()
                }
            },
            environments: match inventory {
                Inventory::Pv { environments, .. } | Inventory::Tcg { environments, .. } => {
                    environments.clone()
                }
            },
        })
    }
    pub(super) fn create(
        &mut self,
        definition: &seedbox_machine::definition::MachineDefinition,
    ) -> Result<Native, String> {
        let name = definition.name.clone();
        let environment = self.environments.get(&name).map(environment).transpose()?;
        let boot = self
            .boot_archives
            .get(&name)
            .map(|artifact| {
                let mut file =
                    super::checkpoint::verified(artifact).map_err(|error| error.to_string())?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>(bytes)
            })
            .transpose()?;
        match &mut self.inner {
            #[cfg(any(
                all(target_os = "linux", target_arch = "x86_64"),
                all(target_arch = "aarch64", any(target_os = "linux", target_os = "macos"))
            ))]
            FactoryKind::Pv(factory) => {
                let bytes = match boot {
                    Some(bytes) => bytes,
                    None => factory
                        .boot_archive_configured(
                            definition,
                            environment.as_ref().ok_or_else(|| {
                                format!("retained factory environment missing for {name}")
                            })?,
                        )
                        .map_err(|error| error.to_string())?,
                };
                check_boot_memory(definition, &bytes)?;
                factory
                    .create_boot_archive(definition, bytes)
                    .map(|machine| Native::Pv {
                        name,
                        machine: Box::new(machine),
                    })
                    .map_err(|error| error.to_string())
            }
            FactoryKind::Tcg(factory) => {
                let bytes = match boot {
                    Some(bytes) => bytes,
                    None => factory
                        .boot_archive_configured(
                            definition,
                            environment.as_ref().ok_or_else(|| {
                                format!("retained factory environment missing for {name}")
                            })?,
                        )
                        .map_err(|error| error.to_string())?,
                };
                check_boot_memory(definition, &bytes)?;
                factory
                    .create_boot_archive(definition, bytes)
                    .map(|machine| Native::Tcg {
                        name,
                        machine: Box::new(machine),
                    })
                    .map_err(|error| error.to_string())
            }
        }
    }
}
