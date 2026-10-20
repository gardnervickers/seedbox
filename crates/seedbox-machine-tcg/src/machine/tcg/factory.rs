use super::{Error, TcgMachine};
use crate::{
    application::{GuestBootstrap, GuestEnvironment, Kernel},
    definition::{BackendPolicy, MachineDefinition},
};
use std::path::{Path, PathBuf};
/// ELF machine of the guest `init`.
const EM_X86_64: u16 = 62;
/// Owns prepared guest inputs for constructing stopped TCG machines.
/// Loading performs no build, environment lookup or guest execution. The host
/// QEMU executable remains separate from the guest-only assets.
pub struct Factory {
    qemu: PathBuf,
    bootstrap: GuestBootstrap,
    firmware: Vec<u8>,
    formatter: Option<Vec<u8>>,
    scratch_root: Option<PathBuf>,
    policy: Option<BackendPolicy>,
}
impl Factory {
    /// Load and validate the explicit offline-built guest asset directory once.
    /// Required files and supported structural formats are checked during load:
    /// an x86-64 bzImage `kernel`, a static x86-64 ELF `init`, `command-line`
    /// shorter than 2048 bytes, and a BIOS image `firmware` that boots Linux
    /// from fw_cfg without option ROMs (qboot). Runtime incompatibility can
    /// still fail when run. No alternate location is tried. Optional `mke2fs`
    /// is validated as static guest ELF and retained when present; configured
    /// named volumes require it, ordinary creation does not.
    ///
    /// `epoch_seconds` is the realtime epoch of every machine this factory
    /// creates: on every boot, before Ready, the guest sets `CLOCK_REALTIME` to
    /// the epoch plus the machine counter (its time base), and the board RTC
    /// derives from the same epoch. Use one epoch for all machines of a run,
    /// including spawned machines; it is part of each machine's boot inputs
    /// and snapshots, not a Driver setting.
    // TODO: Re-evaluate: the epoch is concrete boot configuration owned with
    // the factory's other guest inputs and retained in each machine's boot
    // image and snapshots; neither the Driver's time mapping nor ambient state
    // can supply it to arbitrary machines.
    pub fn new(qemu: &Path, guest_assets: &Path, epoch_seconds: u32) -> Result<Self, Error> {
        use super::super::assets;
        assets::executable(qemu)?;
        let kernel = assets::read(guest_assets, "kernel")?;
        assets::bzimage(&kernel)?;
        let init = assets::read(guest_assets, "init")?;
        assets::elf(&init, EM_X86_64, true)?;
        let command_line = assets::arguments(guest_assets, super::COMMAND_LINE_SIZE)?;
        let firmware = assets::read(guest_assets, "firmware")?;
        assets::bios(&firmware)?;
        let path = guest_assets.join("mke2fs");
        let formatter = match std::fs::read(&path) {
            Ok(bytes) => {
                assets::elf(&bytes, EM_X86_64, true)
                    .map_err(|e| Error(format!("{}: {e}", path.display())))?;
                Some(bytes)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(Error(format!("{}: {error}", path.display()))),
        };
        Ok(Self {
            qemu: qemu.to_owned(),
            bootstrap: GuestBootstrap {
                kernel: Kernel {
                    image: kernel,
                    command_line,
                },
                init,
                base_initramfs: vec![],
                epoch_seconds,
            },
            firmware,
            formatter,
            scratch_root: None,
            policy: None,
        })
    }
    /// Select explicit run-wide backend semantics for subsequently created
    /// machines. `None` remains the ordinary constructor's legacy behavior;
    /// even `Some(default)` selects named policy with controller sockets off.
    /// Validation happens before any machine construction. Restore never calls
    /// this builder on an existing machine or reseeds its saved streams.
    // TODO: Re-evaluate: one retained policy closes initial and future factory
    // creation without adding mandatory fields to generic MachineDefinition.
    pub fn with_policy(mut self, policy: BackendPolicy) -> Result<Self, Error> {
        policy.validate().map_err(|error| Error(error.into()))?;
        self.policy = Some(policy);
        Ok(self)
    }
    /// Package the original application without binary rewriting.
    /// Construction consumes no clock or RNG and executes no guest instructions.
    /// Unsupported resources or unpackageable launch inputs are errors.
    pub fn create(&mut self, definition: &MachineDefinition) -> Result<TcgMachine, Error> {
        self.create_selected(definition, None)
    }
    /// Place subsequently created machines' private host files under `root`.
    /// This applies to both create and create_configured. The path is owned,
    /// but the caller owns the existing directory and must keep it available
    /// until its machines are dropped. No filesystem work occurs in this
    /// builder; creation reports an invalid or unavailable root as an error.
    /// The effective absolute root must be UTF-8 (including relative-path
    /// resolution); unsupported encoding is refused before private files exist.
    /// Without this builder, standalone temporary-file behavior is unchanged.
    /// The root is host authority, not a guest input or snapshot field; restore
    /// must supply new authority with [`TcgMachine::from_snapshot_in`].
    // TODO: Re-evaluate: service sessions own cleanup authority while factories
    // only borrow its directory lifetime through an explicitly owned path.
    pub fn with_scratch_root(mut self, root: &Path) -> Self {
        self.scratch_root = Some(root.to_owned());
        self
    }
    /// Construct with explicit per-guest setup retained in the Linux boot bytes.
    /// Named volumes require definition.disk and the optional static x86-64
    /// mke2fs guest asset loaded by new. Missing prerequisites fail before guest
    /// execution. This adds no PV instrumentation/progress budget to TCG.
    // TODO: Re-evaluate: service-owned environments close boot inputs without
    // adding mandatory MachineDefinition fields or a generic inspection API.
    pub fn create_configured(
        &mut self,
        definition: &MachineDefinition,
        environment: &GuestEnvironment,
    ) -> Result<TcgMachine, Error> {
        self.create_selected(definition, Some(environment))
    }
    /// Assemble the complete configured Linux boot archive that creation uses.
    /// This consumes no clock/RNG, starts no process and creates no private
    /// files. It adds no PV instrumentation. Named-volume prerequisites are
    /// checked, but native RAM constraints are not: callers may size these
    /// actual bytes before selecting guest RAM.
    // TODO: Re-evaluate: preparation and creation must share byte assembly;
    // archive estimates omit retained configuration and bootstrap overhead.
    pub fn boot_archive_configured(
        &self,
        definition: &MachineDefinition,
        environment: &GuestEnvironment,
    ) -> Result<Vec<u8>, Error> {
        environment
            .check_attachments(definition.disk.is_some(), self.formatter.is_some())
            .map_err(|e| Error(e.into()))?;
        self.assemble_selected(&definition.application, &self.bootstrap, Some(environment))
            .map_err(|error| Error(error.to_string()))
    }
    fn assemble_selected(
        &self,
        application: &crate::definition::PreparedApplication,
        bootstrap: &GuestBootstrap,
        environment: Option<&GuestEnvironment>,
    ) -> Result<Vec<u8>, crate::definition::DefinitionError> {
        match environment {
            Some(environment) => crate::application::boot_archive_configured(
                application,
                bootstrap,
                environment,
                self.formatter.as_deref(),
            ),
            None => crate::application::boot_archive(application, bootstrap),
        }
    }
    /// Construct with an already complete Linux boot archive, used verbatim.
    /// No filesystem packaging, guest-setup rewriting or instrumentation occurs.
    /// The caller supplies init and its epoch/setup recipe in these bytes. RAM,
    /// disk and network come from the definition; kernel, firmware, command line
    /// and board epoch come from this factory. Workload filesystem/launch fields
    /// are not repackaged. Native resource checks and scratch placement retain
    /// ordinary creation semantics. This entry executes no guest instructions.
    // TODO: Re-evaluate: complete retained boot input cannot be reconstructed
    // from a stale paired filesystem after a caller replaces its boot archive.
    pub fn create_boot_archive(
        &mut self,
        definition: &MachineDefinition,
        archive: Vec<u8>,
    ) -> Result<TcgMachine, Error> {
        self.create_with(definition, move |_, _| Ok(archive))
    }
    fn create_selected(
        &mut self,
        definition: &MachineDefinition,
        environment: Option<&GuestEnvironment>,
    ) -> Result<TcgMachine, Error> {
        if let Some(environment) = environment {
            environment
                .check_attachments(definition.disk.is_some(), self.formatter.is_some())
                .map_err(|e| Error(e.into()))?;
        }
        self.create_with(definition, |application, bootstrap| {
            self.assemble_selected(application, bootstrap, environment)
        })
    }
    /// Construct with this factory's QEMU, firmware, bootstrap and scratch
    /// root, then apply its configured policy.
    fn create_with(
        &self,
        definition: &MachineDefinition,
        package: impl FnOnce(
            &crate::definition::PreparedApplication,
            &GuestBootstrap,
        ) -> Result<Vec<u8>, crate::definition::DefinitionError>,
    ) -> Result<TcgMachine, Error> {
        let mut machine = TcgMachine::prepare(
            &self.qemu,
            self.firmware.clone(),
            &self.bootstrap,
            definition.clone(),
            self.scratch_root.as_deref(),
            package,
        )?;
        if let Some(policy) = &self.policy {
            machine.configure_policy(&definition.name, policy)?;
        }
        Ok(machine)
    }
}

#[cfg(test)]
mod tests;
