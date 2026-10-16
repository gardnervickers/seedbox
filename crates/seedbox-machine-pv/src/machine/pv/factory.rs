use super::{boot, PVMachine};
use crate::{
    application::{
        boot_derived, boot_derived_configured, with_runtime, GuestBootstrap, GuestEnvironment,
        Kernel,
    },
    definition::{BackendPolicy, FilesystemArchive, MachineDefinition},
};
use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
};
/// Owns prepared guest inputs for constructing stopped PV machines.
/// Loading performs no build, environment lookup or guest execution. The host
/// worker remains a separately supplied executable. Each created machine uses
/// automatic application instrumentation and owns its derived boot inputs.
pub struct Factory {
    worker: PathBuf,
    bootstrap: GuestBootstrap,
    runtime: FilesystemArchive,
    formatter: Option<Vec<u8>>,
    scratch_root: Option<PathBuf>,
    policy: Option<BackendPolicy>,
    blocks_per_progress_call: std::num::NonZeroU32,
}
impl Factory {
    /// Load and validate the explicit offline-built guest asset directory once.
    /// Required files and supported structural formats are checked during load;
    /// runtime incompatibility can still fail when run. No alternate location is tried.
    /// An optional `mke2fs` is validated as static guest ELF and retained when
    /// present; configured named volumes require it, ordinary creation does not.
    ///
    /// `epoch_seconds` is the realtime epoch of every machine this factory
    /// creates: on every boot, before Ready, the guest sets `CLOCK_REALTIME` to
    /// the epoch plus the machine counter (its time base). Use one epoch for
    /// all machines of a run, including spawned machines; it is part of each
    /// machine's boot inputs and snapshots, not a Driver setting.
    // TODO: Re-evaluate: the epoch is concrete boot configuration owned with
    // the factory's other guest inputs and retained in each machine's boot
    // image and snapshots; neither the Driver's time mapping nor ambient state
    // can supply it to arbitrary machines.
    pub fn new(worker: &Path, guest_assets: &Path, epoch_seconds: u32) -> io::Result<Self> {
        use super::super::assets;
        assets::executable(worker)?;
        let kernel = assets::read(guest_assets, "kernel")?;
        guest::kernel(&kernel)?;
        let init = assets::read(guest_assets, "init")?;
        assets::elf(&init, guest::ELF_MACHINE, true)?;
        let command_line = assets::arguments(guest_assets, 2048)?;
        let bytes = assets::read(guest_assets, "progress.tar")?;
        validate_runtime(&bytes)?;
        let runtime = FilesystemArchive::new(bytes).map_err(io::Error::other)?;
        let path = guest_assets.join("mke2fs");
        let formatter = match std::fs::read(&path) {
            Ok(bytes) => {
                assets::elf(&bytes, guest::ELF_MACHINE, true)
                    .map_err(|e| io::Error::other(format!("{}: {e}", path.display())))?;
                Some(bytes)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("{}: {error}", path.display()),
                ));
            }
        };
        Ok(Self {
            worker: worker.to_owned(),
            bootstrap: GuestBootstrap {
                kernel: Kernel {
                    image: kernel,
                    command_line,
                },
                init,
                base_initramfs: vec![],
                epoch_seconds,
            },
            runtime,
            formatter,
            scratch_root: None,
            policy: None,
            blocks_per_progress_call: std::num::NonZeroU32::new(32).expect("nonzero default"),
        })
    }
    /// Select exact named backend semantics for subsequently created machines.
    /// Ordinary constructors retain legacy None; an explicit default selects
    /// named policy with controller sockets disabled. Validation occurs before
    /// construction. This never configures or reseeds a restored machine.
    // TODO: Re-evaluate: one retained factory policy must close ordinary,
    // configured and verbatim boot paths without new definition fields.
    pub fn with_policy(mut self, policy: BackendPolicy) -> io::Result<Self> {
        policy.validate().map_err(io::Error::other)?;
        self.policy = Some(policy);
        Ok(self)
    }
    /// Derive an instrumented application without changing the supplied workload.
    /// Construction consumes no clock or RNG and executes no guest instructions.
    /// Unsupported resources or unpackageable launch inputs are errors.
    pub fn create(&mut self, definition: &MachineDefinition) -> io::Result<PVMachine> {
        self.create_selected(definition, None)
    }
    /// Set the accepted PV instrumentation progress budget for subsequently
    /// created machines. Default is 32. The explicit nonzero value is encoded
    /// in each derived launch prefix and retained by its closed boot snapshot;
    /// this performs no guest work and changes no already-created machine.
    // TODO: Re-evaluate: the service owns one resolved PV budget in its factory
    // inventory; a required definition field would churn unrelated Driver codecs.
    pub fn with_blocks_per_progress_call(mut self, budget: std::num::NonZeroU32) -> Self {
        self.blocks_per_progress_call = budget;
        self
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
    /// must supply new authority with [`PVMachine::from_snapshot_in`].
    // TODO: Re-evaluate: service sessions own cleanup authority while factories
    // only borrow its directory lifetime through an explicitly owned path.
    pub fn with_scratch_root(mut self, root: &Path) -> Self {
        self.scratch_root = Some(root.to_owned());
        self
    }
    /// Construct with explicit per-guest setup, retaining it in the boot bytes.
    /// Named volumes require definition.disk and the optional static mke2fs
    /// guest asset loaded by new; missing prerequisites fail before guest work.
    /// Ordinary create remains unconfigured, including generic control behavior.
    // TODO: Re-evaluate: the service's per-name environment map is separate
    // factory authority, not mandatory application/Driver definition metadata.
    pub fn create_configured(
        &mut self,
        definition: &MachineDefinition,
        environment: &GuestEnvironment,
    ) -> io::Result<PVMachine> {
        self.create_selected(definition, Some(environment))
    }
    /// Assemble the complete configured Linux boot archive that creation uses,
    /// including the factory's PV runtime and selected progress budget. This
    /// consumes no clock/RNG, starts no process and creates no private files.
    /// Named-volume prerequisites are checked; native RAM constraints are not.
    /// The returned bytes can therefore be sized before choosing guest RAM.
    // TODO: Re-evaluate: preparation and creation need one byte-producing recipe;
    // estimates cannot account for retained configuration and instrumentation.
    pub fn boot_archive_configured(
        &self,
        definition: &MachineDefinition,
        environment: &GuestEnvironment,
    ) -> io::Result<Vec<u8>> {
        self.check_environment(definition, Some(environment))?;
        self.assemble_selected(definition, Some(environment))
    }
    fn check_environment(
        &self,
        definition: &MachineDefinition,
        environment: Option<&GuestEnvironment>,
    ) -> io::Result<()> {
        if environment.is_some_and(|e| !e.named_volumes.is_empty()) {
            if definition.disk.is_none() {
                return Err(io::Error::other(
                    "named guest volumes require a machine disk at /dev/vda",
                ));
            }
            if self.formatter.is_none() {
                return Err(io::Error::other(
                    "named guest volumes require guest asset mke2fs; package the static guest formatter",
                ));
            }
        }
        Ok(())
    }
    fn assemble_selected(
        &self,
        definition: &MachineDefinition,
        environment: Option<&GuestEnvironment>,
    ) -> io::Result<Vec<u8>> {
        let derived = with_runtime(
            &definition.application,
            &self.runtime,
            &runtime_prefix_with_budget(self.blocks_per_progress_call),
        )
        .map_err(io::Error::other)?;
        let initrd = match environment {
            Some(env) => {
                boot_derived_configured(&derived, &self.bootstrap, env, self.formatter.as_deref())
            }
            None => boot_derived(&derived, &self.bootstrap),
        }
        .map_err(io::Error::other)?;
        Ok(initrd)
    }
    fn create_selected(
        &mut self,
        definition: &MachineDefinition,
        environment: Option<&GuestEnvironment>,
    ) -> io::Result<PVMachine> {
        self.check_environment(definition, environment)?;
        boot::RamLayout::native(definition.resources.memory_bytes).map_err(io::Error::other)?;
        let initrd = self.assemble_selected(definition, environment)?;
        self.construct_archive(definition, initrd)
    }
    /// Construct with an already complete Linux boot archive, used verbatim.
    /// The caller supplies the guest init, epoch/setup recipe and mandatory PV
    /// instrumentation in these bytes. No filesystem packaging or additional
    /// instrumentation occurs. RAM, disk and network still come from the
    /// definition; kernel and command line come from this factory. Workload
    /// filesystem/launch fields are not repackaged by this entry. Native RAM
    /// checks and scratch placement have the same semantics as ordinary creation.
    // TODO: Re-evaluate: retained complete-boot assets are authoritative inputs,
    // not application tar archives to overlay or instrument a second time.
    pub fn create_boot_archive(
        &mut self,
        definition: &MachineDefinition,
        archive: Vec<u8>,
    ) -> io::Result<PVMachine> {
        boot::RamLayout::native(definition.resources.memory_bytes).map_err(io::Error::other)?;
        self.construct_archive(definition, archive)
    }
    fn construct_archive(
        &self,
        definition: &MachineDefinition,
        initrd: Vec<u8>,
    ) -> io::Result<PVMachine> {
        let (command_line, network) = super::command_line(
            &self.bootstrap.kernel.command_line,
            definition.network.as_ref(),
        );
        let mut machine = match self.scratch_root.as_deref() {
            Some(root) => PVMachine::construct_with_memory_in(
                &self.worker,
                self.bootstrap.kernel.image.clone(),
                initrd,
                command_line,
                definition.disk.clone(),
                network,
                definition.resources.memory_bytes,
                Some(root),
            ),
            None => PVMachine::construct_with_memory(
                &self.worker,
                self.bootstrap.kernel.image.clone(),
                initrd,
                command_line,
                definition.disk.clone(),
                network,
                definition.resources.memory_bytes,
            ),
        }
        .map_err(io::Error::other)?;
        // All three public creation routes meet here, exactly once, before a
        // worker exists. Restore bypasses this factory and validates saved policy.
        if let Some(policy) = &self.policy {
            machine.configure_policy(&definition.name, policy)?;
        }
        Ok(machine)
    }
}
/// Guest architecture details of the private kernel and instrumentation runtime.
#[cfg(target_arch = "aarch64")]
mod guest {
    pub(super) const ELF_MACHINE: u16 = 183;
    pub(super) const LOADER: &str = "lib/ld-linux-aarch64.so.1";
    pub(super) const LAUNCHER_OPTIONS: &[&str] = &[];
    /// ARM exclusives become compare-and-swap with software reservations.
    pub(super) const TRANSLATOR_OPTIONS: &[&str] = &["-ldstex2cas"];
    pub(super) fn kernel(bytes: &[u8]) -> std::io::Result<()> {
        super::super::super::assets::kernel(bytes)
    }
}
#[cfg(target_arch = "x86_64")]
mod guest {
    pub(super) const ELF_MACHINE: u16 = 62;
    pub(super) const LOADER: &str = "lib/ld-linux-x86-64.so.2";
    /// The x86 launcher otherwise warns on application stderr that the
    /// single-bitwidth runtime lacks 32-bit and debug cores. Launcher options
    /// precede the first translator option.
    pub(super) const LAUNCHER_OPTIONS: &[&str] = &["-quiet"];
    pub(super) const TRANSLATOR_OPTIONS: &[&str] = &[];
    /// A bzImage for the 64-bit boot protocol; the worker checks its layout.
    pub(super) fn kernel(bytes: &[u8]) -> std::io::Result<()> {
        if bytes.get(0x202..0x206) != Some(b"HdrS".as_slice()) {
            return Err(std::io::Error::other(
                "invalid or incompatible guest Linux bzImage",
            ));
        }
        Ok(())
    }
}
/// Mandatory translator launch prefix for every prepared application.
pub(super) fn runtime_prefix() -> Vec<String> {
    let mut prefix = vec!["/seedbox/progress/bin64/drrun"];
    prefix.extend_from_slice(guest::LAUNCHER_OPTIONS);
    prefix.extend(["-opt_speed", "-disable_traces"]);
    prefix.extend_from_slice(guest::TRANSLATOR_OPTIONS);
    prefix.extend([
        "-c",
        "/seedbox/progress/libseedbox_progress.so",
        "--blocks-per-progress-call",
        "32",
        "--",
    ]);
    prefix.into_iter().map(str::to_owned).collect()
}
fn runtime_prefix_with_budget(budget: std::num::NonZeroU32) -> Vec<String> {
    let mut prefix = runtime_prefix();
    let position = prefix.len() - 2; // Existing suffix: --blocks-per-progress-call VALUE --.
    prefix[position] = budget.get().to_string();
    prefix
}
fn validate_runtime(bytes: &[u8]) -> io::Result<()> {
    let required = [
        "bin64/drrun",
        "libseedbox_progress.so",
        "lib64/release/libdynamorio.so",
        "lib64/release/libdrpreload.so",
        "ext/lib64/release/libdrmgr.so",
        "ext/lib64/release/libdrreg.so",
        guest::LOADER,
        "lib/libc.so.6",
        "lib/libm.so.6",
        "lib/libdl.so.2",
        "lib/libpthread.so.0",
        "lib/librt.so.1",
        "DynamoRIO-LICENSE.txt",
        "DynamoRIO-ACKNOWLEDGEMENTS.txt",
        "glibc-COPYING.LIB",
    ];
    let mut missing: BTreeSet<_> = required
        .iter()
        .map(|name| format!("seedbox/progress/{name}"))
        .collect();
    let mut archive = tar::Archive::new(bytes);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if let Some(path) = path.to_str() {
            if missing.remove(path) {
                if !entry.header().entry_type().is_file() {
                    return Err(io::Error::other("PV runtime requires regular files"));
                }
                if (path == "seedbox/progress/bin64/drrun"
                    || path.strip_prefix("seedbox/progress/") == Some(guest::LOADER))
                    && entry.header().mode()? & 0o111 == 0
                {
                    return Err(io::Error::other(format!(
                        "PV runtime executable lacks execute permission: {path}"
                    )));
                }
                if !path.ends_with(".txt") && !path.ends_with(".LIB") {
                    let mut content = vec![];
                    std::io::Read::read_to_end(&mut entry, &mut content)?;
                    super::super::assets::elf(&content, guest::ELF_MACHINE, false)?;
                }
            }
        }
    }
    if let Some(path) = missing.first() {
        return Err(io::Error::other(format!(
            "PV instrumentation runtime missing {path}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
