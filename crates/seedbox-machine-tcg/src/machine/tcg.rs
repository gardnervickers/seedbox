//! QEMU TCG reference implementation of the machine execution contract.
//!
//! One x86-64 CPU prices each instruction with the supplied execution factor. A small
//! QEMU patch caps its native work budget and returns when that budget
//! expires, the CPU becomes idle, or the guest shuts down. Idle time advances
//! only when the simulation driver advances the injected clock.
//! There are no per-instruction host round trips or guest-specific yield calls.
//! The clock must stay fixed during each call. Guest time follows the machine's
//! instruction progress within the budget, without changing the shared clock.
//! On the next call, the machine adopts any newer clock time and delivers due
//! timers before executing. No instructions execute during the elapsed gap;
//! an overdue timer becomes deliverable at the adopted time. Shared time may
//! lag local progress while the driver brings other machines to the same barrier.
//!
//! Work is priced in QEMU icount steps, not hardware retirement. Ordinary
//! instructions and translator-raised exceptions such as `ud2` cost one step.
//! REP string I/O costs one step per iteration plus a final zero-count check;
//! even an initially zero repeat count costs one step.
// REP can stop between iterations, so step pricing is observable Machine
// behavior, not merely a translator implementation detail.
//!
//! The board is QEMU's x86-64 `microvm` with one fixed `qemu64` CPU, RAM from
//! physical address 0, the ISA COM1 UART as the console, PIC, PIT and an RTC
//! on the virtual clock, ACPI, and eight legacy virtio-mmio transports. Linux
//! boots from an owned BIOS image (qboot) that loads the bzImage, initramfs
//! and exact command line from fw_cfg; no QEMU option ROM or other data-file
//! firmware is loaded, and QEMU adds nothing to the command line. Snapshots
//! own the boot inputs, RAM, CPU/device state, virtual timers, and lifecycle
//! position. Restore requires the caller to provide a compatible patched QEMU
//! executable. This implementation supports one CPU only. It uses software
//! emulation on macOS and Linux, whatever the host architecture.
// microvm keeps the legacy virtio-mmio devices shared by
// lifecycle, disk and NIC; ACPI is its only guest power-off path, so
// `Shutdown` stays distinct from a reset; qboot is the firmware that boots
// Linux without option ROMs, keeping every guest input owned by the machine.
//!
//! A definition with a network interface adds the patched QEMU's
//! `seedbox-net-device` (`eth0`) with its MAC, and its kernel arguments carry
//! `seedbox.ipv4=A.B.C.D/P` for the guest's static address, as on PV. Frames
//! cross only stopped QMP commands, never a host network. Each due
//! `ReceiveFrame` is taken: without a NIC, in an ended boot or ahead of a
//! Crash or Reboot in the same admission, it is dropped; otherwise the NIC
//! receives it at the run's start, after lifecycle input, and a CPU it makes
//! runnable is no longer blocked, even on a zero-budget run. The device drops
//! frames until the guest driver is ready and keeps a backlog of at most 256;
//! the backlog, rings and any undrained transmissions belong to the QEMU
//! migration stream. Transmitted frames are drained after every execution
//! and published in timestamp order among console and lifecycle output. A
//! malformed receive or transmit ring stops the machine with an error.
//!
//! A definition's disk appears as `/dev/vda` through the patched QEMU's
//! `seedbox-blk-device`: the shared Rust store completes each request
//! synchronously at the guest's MMIO kick, with no QEMU block backend, AIO or
//! host flush. The QEMU process owns the disk during a boot; this machine
//! owns it between boots. When a boot ends (guest power-off or reset, an
//! accepted Crash, or the teardown for an accepted Reboot) the stopped
//! device's overlays are exported before QEMU is terminated and their
//! unflushed writes are discarded exactly once; durable writes carry to later
//! boots. A guest reset of the virtio device keeps both layers; only guest
//! FLUSH makes writes durable, never the Shutdown request itself. A machine
//! with a disk therefore ends its QEMU process when the guest powers off or
//! resets; a diskless machine keeps it. Snapshots share the immutable base
//! and carry both layers without promotion: the migration stream holds them
//! while QEMU is live, the snapshot itself between boots. A malformed disk
//! request or ring stops execution with an error.
//!
//! Shutdown is offered as `shutdown\n` on the dedicated standard virtio-serial
//! port named `seedbox-lifecycle`. The device retains one request through early
//! boot and snapshots, with synchronous queue kicks at guest MMIO instructions.
//! Guest userspace owns cleanup and Linux poweroff policy; a boot input without
//! a cooperating receiver can ignore the request and continue running.
//!
//! Guest terminal requests stop QEMU at the requesting instruction. ACPI
//! power-off is `Shutdown`. QEMU runs with `-action reboot=shutdown`, so a
//! guest reset request (the ACPI reset register or a triple fault), including
//! a kernel panic restart under `panic=-1`, stops instead of resetting in place
//! and is reported as `Crashed`; only an accepted Reboot request starts a new
//! boot. Host-caused stops and unknown causes are errors.

mod codec;
mod evidence;
mod factory;
mod native_witness;
mod policy;
mod retained;
pub use factory::Factory;
use policy::PolicyState;
use retained::{EndReport, Ended};
use seedbox_machine::definition::DiskImage;
use seedbox_machine::machine::disk::Store;
use seedbox_machine::machine::{ExitReason, Machine, RunContext, StopReason, Timestamp};
use seedbox_machine::{
    definition::BackendPolicy,
    machine::{EvidenceDisposition, StateEvidence},
};
use serde_json::Value;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

mod qemu;
use qemu::Process;

/// x86 Linux `COMMAND_LINE_SIZE`, including the terminating NUL.
const COMMAND_LINE_SIZE: usize = 2048;
/// Size of a [`Boot::Program`] image: one 64 KiB BIOS ROM.
#[cfg(any(test, feature = "test-support"))]
pub(crate) const PROGRAM_BYTES: usize = 64 * 1024;

/// Owned boot inputs, independent of their original files.
#[derive(Clone, Debug)]
pub(crate) enum Boot {
    /// Exactly [`PROGRAM_BYTES`] of test BIOS image, mapped as the firmware
    /// region below 4 GiB and at 0xF0000. Tests must not rely on whether it is
    /// writable. The CPU starts at its reset vector (offset 0xFFF0, real mode);
    /// the image itself enters any other mode. There is no loader.
    #[cfg(any(test, feature = "test-support"))]
    Program(Arc<[u8]>),
    /// An x86-64 bzImage, initramfs, and the BIOS image that boots them.
    Linux {
        kernel: Arc<[u8]>,
        initramfs: Arc<[u8]>,
        firmware: Arc<[u8]>,
        command_line: String,
        /// Realtime epoch of every boot. The supervisor sets realtime from it;
        /// the board RTC derives from it too, so no host wall clock is read.
        epoch_seconds: u32,
    },
}

/// Complete initial configuration of this single-CPU machine.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub boot: Boot,
    pub memory_mib: u32,
    /// Immutable initial durable contents of `/dev/vda`, if attached.
    pub disk: Option<DiskImage>,
    /// The NIC's MAC address, when the definition has a network interface.
    pub network: Option<[u8; 6]>,
}

// QEMU's device owns the live disk so requests complete
// in-process at the kick; this machine owns it between boots so boot-end
// export and the single unflushed discard stay Machine policy, not C's.
/// The machine's disk between boots and its host input files.
struct Disk {
    /// The base image file shared by every QEMU process, if not zeroed.
    files: TempDir,
    /// A store over the base with empty overlays; clones share its base.
    empty: Store,
    /// `Some` exactly when no QEMU process owns the disk.
    store: Option<Store>,
}

impl Disk {
    fn new(image: &DiskImage, scratch_root: Option<&Path>) -> Result<Self, Error> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("seedbox-tcg-disk-");
        let requested = scratch_root
            .map(Path::to_owned)
            .unwrap_or_else(tempfile::env::temp_dir);
        let root = checked_scratch_root(&requested)?;
        let files = builder
            .tempdir_in(&root)
            .map_err(|e| Error(format!("TCG disk directory in {root:?}: {e}")))?;
        let empty = match image.image() {
            Some(bytes) => {
                fs::write(files.path().join("base"), &bytes[..])?;
                Store::from_image(bytes.clone())
            }
            None => Store::zeroed(image.sectors()),
        }
        .map_err(|error| Error(format!("TCG disk base: {error:?}")))?;
        Ok(Self {
            files,
            store: Some(empty.clone()),
            empty,
        })
    }

    /// QEMU device inputs for a new boot (with this machine's durable
    /// overlays) or a restore (whose migration stream carries the overlays).
    fn input(&self, image: &DiskImage, boot: bool) -> Result<qemu::DiskInput, Error> {
        let overlay = if boot {
            let store = self
                .store
                .as_ref()
                .ok_or_else(|| Error("TCG disk still owned by QEMU".into()))?;
            let path = self.files.path().join("overlay");
            fs::write(&path, store.encode_overlays())?;
            Some(path)
        } else {
            None
        };
        Ok(qemu::DiskInput {
            sectors: image.sectors(),
            base: image.image().map(|_| self.files.path().join("base")),
            overlay,
        })
    }
}

/// A complete, independent capture of this machine.
#[derive(Clone)]
pub struct Snapshot {
    manifest: Manifest,
    reached: Timestamp,
    state: State,
    image: Option<Vec<u8>>,
    boot_seed: u64,
    pending_boots: u64,
    pending_at: Option<Timestamp>,
    shutdown_requested: bool,
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: seedbox_machine::pv::bridge::GuestBridge,
    /// SBXSTOR1 overlays of a disk no QEMU process owns; with a live process
    /// its migration `image` carries them instead.
    disk: Option<Vec<u8>>,
    policy: Option<PolicyState>,
    logical_boot: u64,
    active_boot: Option<u64>,
    last_ended: Option<Ended>,
    last_evidence: Option<StateEvidence>,
    native_witness: native_witness::NativeWitness,
    pending_ends: std::collections::VecDeque<EndReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ready,
    Blocked { deadline: Option<Timestamp> },
    Exited(ExitReason),
    Invalid,
}

/// A QEMU process and its accepted simulation state.
pub struct TcgMachine {
    manifest: Manifest,
    reached: Timestamp,
    state: State,
    process: Option<Process>,
    executable: PathBuf,
    boot_seed: u64,
    pending_boots: u64,
    pending_at: Option<Timestamp>,
    shutdown_requested: bool,
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: seedbox_machine::pv::bridge::GuestBridge,
    console: Vec<u8>,
    disk: Option<Disk>,
    policy: Option<PolicyState>,
    // Sole admitted logical counter. Process creation/restore never increments it.
    logical_boot: u64,
    // Physical owner while future reset reservations may already exist.
    active_boot: Option<u64>,
    last_ended: Option<Ended>,
    last_evidence: Option<StateEvidence>,
    native_witness: native_witness::NativeWitness,
    pending_ends: std::collections::VecDeque<EndReport>,
    // Declared last: processes and disk files drop before their session child.
    // Host authority only; never copied into a Snapshot.
    scratch: Option<TempDir>,
}

/// QMP JSON and legacy QEMU string options cannot encode arbitrary Unix bytes.
/// Keep this conversion fallible at every string/JSON site, even after preflight.
fn path_text<'a>(path: &'a Path, role: &str) -> Result<&'a str, Error> {
    path.to_str().ok_or_else(|| Error(format!("TCG {role} {path:?}: path is not UTF-8; choose a UTF-8 scratch directory including its absolute parent path")))
}

/// Read-only resolution catches non-UTF-8 CWD ancestors of ASCII relative roots
/// before a directory is created. It does not canonicalize/alias caller storage.
fn checked_scratch_root(root: &Path) -> Result<PathBuf, Error> {
    let effective = std::path::absolute(root).map_err(|e| {
        Error(format!(
            "TCG scratch directory {root:?}: resolve absolute path: {e}"
        ))
    })?;
    path_text(&effective, "scratch directory")?;
    Ok(effective)
}

fn machine_scratch(root: Option<&Path>) -> Result<Option<TempDir>, Error> {
    root.map(|root| {
        let effective = checked_scratch_root(root)?;
        tempfile::Builder::new()
            .prefix("seedbox-tcg-machine-")
            .tempdir_in(&effective)
            .map_err(|e| Error(format!("TCG machine directory in {effective:?}: {e}")))
    })
    .transpose()
}

/// Setup, protocol, or execution failure.
#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self(format!("QEMU transport or state I/O: {error}"))
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self(format!("invalid QMP response: {error}"))
    }
}

impl Snapshot {
    /// Check retained named policy against the caller's factory authority before
    /// creating native restore resources. Absence differs from an explicit
    /// default; a snapshot never silently selects a different controller.
    // Policy authority comes from factory inventories, never from snapshot
    // data; this narrow check exposes no private migration or machine state.
    pub fn validate_policy(
        &self,
        name: &str,
        expected: Option<&BackendPolicy>,
    ) -> Result<(), Error> {
        policy::validate_authority(self.policy.as_ref(), name, expected)
    }
}

impl TcgMachine {
    /// Prepare a stopped machine using the project's patched QEMU binary and
    /// the default boot archive, without a factory scratch root.
    #[cfg(test)]
    pub(crate) fn new(
        qemu: &Path,
        firmware: Vec<u8>,
        bootstrap: &seedbox_machine::application::GuestBootstrap,
        definition: seedbox_machine::definition::MachineDefinition,
    ) -> Result<Self, Error> {
        Self::prepare(
            qemu,
            firmware,
            bootstrap,
            definition,
            None,
            seedbox_machine::application::boot_archive,
        )
    }

    /// Prepare a stopped machine using the project's patched QEMU binary.
    /// Boot initialization occurs in the first run, consuming eight bytes from
    /// its context RNG in little-endian order without executing instructions.
    /// Each pending reboot consumes another eight bytes on the next run.
    ///
    /// The factory supplies host scratch authority separately from guest
    /// inputs. `package` runs once, after resource/argument checks, so
    /// factory-owned environment configuration does not alter validation order.
    pub(crate) fn prepare(
        qemu: &Path,
        firmware: Vec<u8>,
        bootstrap: &seedbox_machine::application::GuestBootstrap,
        definition: seedbox_machine::definition::MachineDefinition,
        scratch_root: Option<&Path>,
        package: impl FnOnce(
            &seedbox_machine::definition::PreparedApplication,
            &seedbox_machine::application::GuestBootstrap,
        ) -> Result<Vec<u8>, seedbox_machine::definition::DefinitionError>,
    ) -> Result<Self, Error> {
        const MIB: u64 = 1024 * 1024;
        let memory = definition.resources.memory_bytes;
        if memory == 0 || !memory.is_multiple_of(MIB) || memory / MIB > u64::from(u32::MAX) {
            return Err(Error(
                "TCG guest memory must be a nonzero whole number of MiB representable in u32"
                    .into(),
            ));
        }
        if bootstrap.kernel.image.is_empty() || firmware.is_empty() {
            return Err(Error("TCG kernel and firmware must be nonempty".into()));
        }
        // The guest's static address, as on PV (guest_network.h).
        let command_line = match &definition.network {
            Some(interface) => format!(
                "{} seedbox.ipv4={}/{}",
                bootstrap.kernel.command_line,
                interface.ipv4(),
                interface.prefix_len()
            ),
            None => bootstrap.kernel.command_line.clone(),
        };
        // x86 COMMAND_LINE_SIZE includes its trailing NUL; Linux would
        // silently truncate a longer line copied from the boot parameters.
        if command_line.contains('\0') || command_line.len() >= COMMAND_LINE_SIZE {
            return Err(Error(format!(
                "TCG kernel command line must contain no NUL and fit in {} bytes",
                COMMAND_LINE_SIZE - 1
            )));
        }
        // TCG needs no application instrumentation. Packaging borrows the
        // immutable prepared input; PV may derive its own private preparation.
        let initramfs = package(&definition.application, bootstrap)
            .map_err(|error| Error(format!("TCG application preparation: {error}")))?;
        Self::from_manifest_with_scratch(
            qemu,
            Manifest {
                memory_mib: (memory / MIB) as u32,
                boot: Boot::Linux {
                    kernel: Arc::from(bootstrap.kernel.image.as_slice()),
                    initramfs: initramfs.into(),
                    firmware: firmware.into(),
                    command_line,
                    epoch_seconds: bootstrap.epoch_seconds,
                },
                disk: definition.disk,
                network: definition
                    .network
                    .as_ref()
                    .map(|interface| interface.mac().octets()),
            },
            scratch_root,
        )
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn new_raw(qemu: &Path, manifest: Manifest) -> Result<Self, Error> {
        Self::from_manifest(qemu, manifest)
    }

    #[cfg(any(test, feature = "test-support"))]
    fn from_manifest(qemu: &Path, manifest: Manifest) -> Result<Self, Error> {
        Self::from_manifest_with_scratch(qemu, manifest, None)
    }

    fn from_manifest_with_scratch(
        qemu: &Path,
        manifest: Manifest,
        scratch_root: Option<&Path>,
    ) -> Result<Self, Error> {
        if manifest.memory_mib == 0 {
            return Err(Error("guest memory must be nonzero".into()));
        }
        match &manifest.boot {
            #[cfg(any(test, feature = "test-support"))]
            Boot::Program(_) => {}
            Boot::Linux {
                kernel,
                initramfs,
                firmware,
                ..
            } => {
                let invalid = |error: std::io::Error| Error(format!("TCG boot input: {error}"));
                seedbox_machine::machine::assets::bzimage(kernel).map_err(invalid)?;
                seedbox_machine::machine::assets::bios(firmware).map_err(invalid)?;
                let memory = u64::from(manifest.memory_mib) * 1024 * 1024;
                if initramfs.len() as u64 > memory {
                    return Err(Error(format!(
                        "TCG initramfs ({} bytes) must fit in guest RAM ({memory} bytes)",
                        initramfs.len()
                    )));
                }
            }
        }
        #[cfg(any(test, feature = "test-support"))]
        if let Boot::Program(program) = &manifest.boot {
            if program.len() != PROGRAM_BYTES {
                return Err(Error(format!(
                    "test program must be a {PROGRAM_BYTES}-byte BIOS image"
                )));
            }
        }
        let scratch = machine_scratch(scratch_root)?;
        let disk = manifest
            .disk
            .as_ref()
            .map(|image| Disk::new(image, scratch.as_ref().map(|d| d.path())))
            .transpose()?;
        Ok(Self {
            scratch,
            disk,
            manifest,
            reached: Timestamp::from_nanos(0),
            state: State::Ready,
            process: None,
            executable: qemu.to_owned(),
            boot_seed: 0,
            policy: None,
            logical_boot: 0,
            active_boot: None,
            last_ended: None,
            last_evidence: None,
            native_witness: Default::default(),
            pending_ends: Default::default(),
            pending_boots: 1,
            pending_at: None,
            shutdown_requested: false,
            writes: Default::default(),
            input: Vec::new(),
            guest: Default::default(),
            console: Vec::new(),
        })
    }

    /// Reconstruct internal machine state without restoring driver dependencies.
    /// Effective default scratch paths must be UTF-8 for QMP JSON and QEMU
    /// options; encoding errors precede directory creation or process startup.
    pub fn from_snapshot(qemu: &Path, snapshot: &Snapshot) -> Result<Self, Error> {
        Self::from_snapshot_with_scratch(qemu, snapshot, None)
    }

    /// Restore with all named boot, disk and migration scratch beneath an existing
    /// caller-owned directory. The machine owns a unique child, cleaned on setup
    /// failure or Drop; it never removes the caller's root. Keep that root alive
    /// until Drop and reclaim the session after abrupt worker death. No cwd or
    /// environment changes occur. Anonymous management channels have no Unix
    /// socket pathname limit. Scratch authority is not snapshot data: this target
    /// uses only the supplied new root, not source-machine paths. Native state and
    /// execution semantics are those of [`Self::from_snapshot`]. The effective
    /// absolute scratch pathname (including CWD/default-temp parents) must be
    /// UTF-8 for QMP JSON and QEMU options. Encoding errors are returned before
    /// creating scratch or spawning a child; valid UTF-8 characters are preserved.
    pub fn from_snapshot_in(
        qemu: &Path,
        snapshot: &Snapshot,
        scratch_root: &Path,
    ) -> Result<Self, Error> {
        Self::from_snapshot_with_scratch(qemu, snapshot, Some(scratch_root))
    }

    fn from_snapshot_with_scratch(
        qemu: &Path,
        snapshot: &Snapshot,
        scratch_root: Option<&Path>,
    ) -> Result<Self, Error> {
        retained::validate(snapshot).map_err(|e| Error(format!("TCG snapshot: {e}")))?;
        // Client overlays exist exactly for a disk that no live QEMU owns;
        // otherwise the migration image carries them, or there is no disk.
        if snapshot.disk.is_some() != (snapshot.manifest.disk.is_some() && snapshot.image.is_none())
        {
            return Err(Error(
                "TCG snapshot disk does not match its manifest".into(),
            ));
        }
        let scratch = machine_scratch(scratch_root)?;
        let root = scratch.as_ref().map(|d| d.path());
        let mut disk = snapshot
            .manifest
            .disk
            .as_ref()
            .map(|image| Disk::new(image, root))
            .transpose()?;
        let disk_input = match (&snapshot.manifest.disk, &disk) {
            (Some(image), Some(disk)) if snapshot.image.is_some() => {
                Some(disk.input(image, false)?)
            }
            _ => None,
        };
        let process = snapshot
            .image
            .as_deref()
            .map(|image| {
                if let Some(policy) = &snapshot.policy {
                    // Incoming RAM/CPU belongs to the physical owner, not a
                    // later admitted logical reset reservation.
                    let owner = snapshot.active_boot.ok_or_else(|| {
                        Error("TCG migration image has no physical boot owner".into())
                    })?;
                    let setup = policy.timer_setup(owner);
                    Process::start_with_timer_policy(
                        qemu,
                        &snapshot.manifest,
                        Some(image),
                        snapshot.boot_seed,
                        disk_input.as_ref(),
                        root,
                        &setup,
                    )
                } else {
                    Process::start_with_scratch(
                        qemu,
                        &snapshot.manifest,
                        Some(image),
                        snapshot.boot_seed,
                        disk_input.as_ref(),
                        root,
                    )
                }
            })
            .transpose()?;
        if let Some(disk) = &mut disk {
            match &snapshot.disk {
                // Checked above: overlays exactly when no live QEMU owns them.
                Some(overlays) => disk
                    .store
                    .as_mut()
                    .expect("new disk")
                    .replace_overlays(overlays)
                    .map_err(|error| Error(format!("TCG snapshot disk: {error:?}")))?,
                None => disk.store = None,
            }
        }
        Ok(Self {
            scratch,
            disk,
            manifest: snapshot.manifest.clone(),
            reached: snapshot.reached,
            state: snapshot.state,
            process,
            executable: qemu.to_owned(),
            boot_seed: snapshot.boot_seed,
            policy: snapshot.policy.clone(),
            logical_boot: snapshot.logical_boot,
            active_boot: snapshot.active_boot,
            last_ended: snapshot.last_ended.clone(),
            last_evidence: snapshot.last_evidence.clone(),
            native_witness: snapshot.native_witness.clone(),
            pending_ends: snapshot.pending_ends.clone(),
            pending_boots: snapshot.pending_boots,
            pending_at: snapshot.pending_at,
            shutdown_requested: snapshot.shutdown_requested,
            writes: snapshot.writes.clone(),
            input: snapshot.input.clone(),
            guest: snapshot.guest.clone(),
            console: Vec::new(),
        })
    }

    /// Apply factory-owned semantics before any native startup. Complete boot
    /// archive bytes are retained verbatim; only the owned controller argument
    /// is added to the kernel command line. Reconfiguration is never allowed.
    fn configure_policy(&mut self, name: &str, policy: &BackendPolicy) -> Result<(), Error> {
        if self.policy.is_some()
            || self.process.is_some()
            || self.logical_boot != 0
            || self.pending_boots != 1
            || self.reached.as_nanos() != 0
            || self.last_ended.is_some()
            || !self.input.is_empty()
            || !self.writes.is_empty()
            || self.state != State::Ready
        {
            return Err(Error(
                "TCG backend policy requires an untouched initial machine".into(),
            ));
        }
        let selected = PolicyState::new(name, policy)?;
        match &mut self.manifest.boot {
            Boot::Linux { command_line, .. } => {
                if command_line
                    .split_whitespace()
                    .any(|word| word.starts_with("seedbox.control="))
                {
                    return Err(Error("TCG kernel command line sets seedbox.control; remove it and select the controller through BackendPolicy".into()));
                }
                let next = format!(
                    "{command_line} seedbox.control={}",
                    u8::from(policy.controller.as_deref() == Some(name))
                );
                if next.len() >= COMMAND_LINE_SIZE {
                    return Err(Error("TCG configured controller argument exceeds kernel command-line capacity; shorten the base command line".into()));
                }
                *command_line = next;
            }
            #[cfg(any(test, feature = "test-support"))]
            Boot::Program(_) => {}
        }
        self.policy = Some(selected);
        Ok(())
    }

    /// Capture actual stopped native state before atomic disk loss/teardown.
    /// The retained cursor prevents a later Crash/Reboot from reclassifying an
    /// already-ended boot. A configured never-started slot has explicit no-RAM
    /// retirement, not a snapshot-byte digest or invented successor state.
    fn end_boot(
        &mut self,
        boot: u64,
        disposition: EvidenceDisposition,
        at: Timestamp,
    ) -> Result<(), Error> {
        if self
            .last_ended
            .as_ref()
            .is_some_and(|last| last.boot >= boot)
        {
            return Ok(());
        }
        if self.policy.is_none() && self.process.is_none() {
            return Ok(());
        }
        self.cache_native(boot, disposition)?;
        let native_booted = self.process.is_some();
        let store = self.stopped_store()?;
        let capture = if native_booted
            && self
                .policy
                .as_ref()
                .is_some_and(|p| p.configuration.full_state)
        {
            Some(self.capture_evidence(boot, disposition, store.as_ref())?)
        } else {
            None
        };
        let ended = Ended {
            boot,
            disposition,
            native_booted,
        };
        let emit = self
            .policy
            .as_ref()
            .is_some_and(|p| p.configuration.boot_end_evidence);
        let mut lost = 0;
        let mut surviving = None;
        if let Some(store) = &store {
            let (next, count) = policy::lose(store, self.policy.as_ref(), boot)?;
            lost = count;
            if emit {
                surviving = Some(policy::disk_digest(
                    &next,
                    self.policy
                        .as_ref()
                        .is_some_and(|p| p.configuration.torn_writes),
                )?);
            }
            self.disk.as_mut().expect("store has disk").store = Some(next);
        }
        // Preserve the old unconfigured diskless process lifetime. Configured
        // runs retain pre-loss evidence and never invent successor RAM.
        if self.policy.is_some() || self.disk.is_some() {
            if let Some(mut process) = self.process.take() {
                self.console.extend(process.console()?);
                process.terminate()?;
            }
            self.active_boot = None;
        }
        if emit {
            self.pending_ends.push_back(EndReport {
                at,
                ended: ended.clone(),
                lost,
                disk: surviving,
                retired: capture.as_ref().map(|e| e.digest.clone()),
            });
        }
        self.last_ended = Some(ended);
        self.last_evidence = capture;
        Ok(())
    }

    fn request_shutdown(&mut self) -> Result<(), Error> {
        match self.state {
            State::Invalid => Err(Error("cannot shut down an invalid machine".into())),
            State::Exited(_) if self.pending_boots == 0 => Ok(()),
            _ => {
                if !self.shutdown_requested {
                    self.shutdown_requested = true;
                    self.input.extend_from_slice(b"shutdown\n");
                }
                Ok(())
            }
        }
    }
    /// Destroy the process, ending any disk boot first. No guest RAM survives
    /// a crash; previously captured snapshots remain independent.
    fn request_crash(&mut self) -> Result<(), Error> {
        if self.state == State::Invalid {
            return Err(Error("crash requires a valid machine".into()));
        }
        if matches!(self.state, State::Exited(_)) && self.pending_boots == 0 {
            self.guest.reset();
            self.input.clear();
            self.writes = Default::default();
            self.shutdown_requested = false;
            return Ok(());
        }
        self.state = State::Invalid;
        let boot = self.active_boot.unwrap_or(self.logical_boot);
        self.end_boot(boot, EvidenceDisposition::Crashed, self.reached)?;
        if let Some(mut process) = self.process.take() {
            self.console.extend(process.console()?);
            process.terminate()?;
        }
        self.active_boot = None;
        if boot != self.logical_boot {
            self.end_boot(
                self.logical_boot,
                EvidenceDisposition::Crashed,
                self.reached,
            )?;
        }
        self.guest.reset();
        self.input.clear();
        self.writes = Default::default();
        self.state = State::Exited(ExitReason::Crashed);
        self.pending_boots = 0;
        self.pending_at = None;
        self.shutdown_requested = false;
        Ok(())
    }
    /// Defer a reset until the next run, which ends the old disk boot. Each
    /// recorded reset draws its own boot seed from that run's caller-owned RNG
    /// before any guest work.
    fn request_reboot(&mut self, at: Timestamp) -> Result<(), Error> {
        if self.state == State::Invalid {
            return Err(Error("cannot reboot an invalid machine".into()));
        }
        if at < self.reached
            || self.pending_at.is_some_and(|previous| at < previous)
            || at.as_nanos() > i64::MAX as u64
        {
            return Err(Error(
                "reboot precedes machine position or exceeds QEMU's time range".into(),
            ));
        }
        let pending = self
            .pending_boots
            .checked_add(1)
            .ok_or_else(|| Error("pending reboot count exhausted".into()))?;
        let boot = self
            .logical_boot
            .checked_add(1)
            .ok_or_else(|| Error("logical boot ordinal exhausted".into()))?;
        self.accept_native_reboot()?;
        self.pending_boots = pending;
        self.logical_boot = boot;
        self.pending_at = Some(at);
        self.shutdown_requested = false;
        self.guest.reset();
        self.input.clear();
        self.writes = Default::default();
        Ok(())
    }
    fn initialize(&mut self, context: &mut RunContext<'_, ()>, at: Timestamp) -> Result<(), Error> {
        self.state = State::Invalid;
        if self.pending_boots == 0 {
            return Err(Error("TCG initialization has no reserved boot".into()));
        }
        let first = self
            .logical_boot
            .checked_sub(self.pending_boots - 1)
            .ok_or_else(|| Error("TCG pending boot ordinals disagree".into()))?;
        if let Some(boot) = self.active_boot {
            self.end_boot(boot, EvidenceDisposition::Crashed, at)?;
        }
        if let Some(mut process) = self.process.take() {
            self.console.extend(process.console()?);
            process.terminate()?;
        }
        self.active_boot = None;
        for index in 0..self.pending_boots {
            let boot = first + index;
            let mut bytes = [0; 8];
            context.rng.fill_bytes(&mut bytes);
            self.boot_seed = u64::from_le_bytes(bytes);
            // Preserve exactly one caller-RNG fill per existing reserved reset.
            if let Some(previous) = self.active_boot {
                self.end_boot(previous, EvidenceDisposition::Crashed, at)?;
            }
            let disk = match (&self.manifest.disk, &self.disk) {
                (Some(image), Some(disk)) => Some(disk.input(image, true)?),
                _ => None,
            };
            let root = self.scratch.as_ref().map(|d| d.path());
            let mut process = if let Some(policy) = &self.policy {
                let setup = policy.timer_setup(boot);
                Process::start_with_timer_policy(
                    &self.executable,
                    &self.manifest,
                    None,
                    self.boot_seed,
                    disk.as_ref(),
                    root,
                    &setup,
                )?
            } else {
                Process::start_with_scratch(
                    &self.executable,
                    &self.manifest,
                    None,
                    self.boot_seed,
                    disk.as_ref(),
                    root,
                )?
            };
            process.advance(at)?;
            self.start_native(&mut process, boot, at)?;
            if let Some(disk) = &mut self.disk {
                disk.store = None;
            }
            if let Some(mut previous) = self.process.replace(process) {
                previous.terminate()?;
            }
            self.active_boot = Some(boot);
        }
        self.reached = at;
        self.pending_boots = 0;
        self.pending_at = None;
        self.state = State::Ready;
        Ok(())
    }

    /// The durable disk between boots; `None` while QEMU owns it.
    #[cfg(test)]
    fn disk_store(&self) -> Option<&Store> {
        self.disk.as_ref()?.store.as_ref()
    }

    /// Read the serial output produced by this instance since creation/restore.
    /// Already emitted output is external observation, not replayed by restore.
    pub fn console(&self) -> Result<Vec<u8>, Error> {
        let mut console = self.console.clone();
        if let Some(process) = &self.process {
            console.extend(process.console()?);
        }
        Ok(console)
    }

    fn flush_ends(&mut self, context: &mut RunContext<'_, ()>) -> Result<(), Error> {
        while let Some(report) = self.pending_ends.front() {
            let at = report.at.max(Timestamp::from_nanos(context.clock.now()));
            if let Err(error) = context.outbox.emit(seedbox_machine::pv::Output {
                at,
                payload: report.response(),
            }) {
                self.state = State::Invalid;
                drop(self.process.take());
                return Err(Error(format!("TCG boot-end output time: {error:?}")));
            }
            self.pending_ends.pop_front();
        }
        Ok(())
    }

    fn process(&mut self) -> Result<&mut Process, Error> {
        self.process
            .as_mut()
            .ok_or_else(|| Error("machine has no execution process".into()))
    }

    fn execute(
        &mut self,
        upto: Timestamp,
        execution: seedbox_machine::execution::ExecutionConfig,
    ) -> Result<(), Error> {
        let start = self.reached;
        let network = self.manifest.network.is_some();
        let qemu::RunOutput {
            result,
            writes,
            frames,
        } = self
            .process()?
            .execute_collect(start, upto, execution, network)?;
        let reached = timestamp(&result, "time")?;
        if reached < start || reached > upto {
            return Err(Error(format!(
                "QEMU crossed the requested execution boundary: {result}"
            )));
        }
        let state = match result["reason"].as_str() {
            Some("boundary") if reached == upto => State::Ready,
            Some("blocked") => {
                let deadline = result
                    .get("deadline")
                    .map(|_| timestamp(&result, "deadline"))
                    .transpose()?;
                if deadline.is_some_and(|at| at <= reached) {
                    return Err(Error(format!(
                        "QEMU returned a timer that is already due: {result}"
                    )));
                }
                State::Blocked { deadline }
            }
            Some("shutdown") => State::Exited(ExitReason::Shutdown),
            // Guest reset request: a deliberate reboot or a panic restart.
            // The Driver owns reboots, so this boot ends as a crash.
            Some("reset") => State::Exited(ExitReason::Crashed),
            _ => return Err(Error(format!("unexpected QEMU stop: {result}"))),
        };
        if frames
            .iter()
            .any(|frame| frame.time < start.as_nanos() || frame.time > reached.as_nanos())
        {
            return Err(Error(
                "QEMU transmitted frame crossed execution interval".into(),
            ));
        }
        let writes = merge_frames(writes, frames);
        if writes
            .iter()
            .any(|write| write.time() < start.as_nanos() || write.time() > reached.as_nanos())
            || writes
                .windows(2)
                .any(|pair| pair[0].time() > pair[1].time())
        {
            return Err(Error("QEMU guest write crossed execution interval".into()));
        }
        self.writes.extend(writes);
        self.reached = reached;
        self.state = state;
        // Evidence is captured from this actual boot before loss or teardown.
        if let State::Exited(reason) = state {
            let disposition = match reason {
                ExitReason::Shutdown => EvidenceDisposition::Shutdown,
                ExitReason::Crashed => EvidenceDisposition::Crashed,
                ExitReason::ProgramExit { code } => EvidenceDisposition::Exited { code },
            };
            self.end_boot(
                self.active_boot.unwrap_or(self.logical_boot),
                disposition,
                reached,
            )?;
        }
        Ok(())
    }
}

// QEMU keeps console/lifecycle and NIC output in separate
// device queues, so one ordered stream must be rebuilt here before emission;
// Driver sorting cannot repair an output stream that went backwards.
/// Merge transmitted frames into the guest's console and lifecycle writes by
/// timestamp, both already in order; each stream keeps its own order. At
/// equal timestamps console and lifecycle writes deterministically precede
/// frames.
fn merge_frames(
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    frames: Vec<seedbox_machine::pv::transport::LifecycleWrite>,
) -> Vec<seedbox_machine::pv::transport::GuestWrite> {
    use seedbox_machine::pv::transport::GuestWrite;
    let mut merged = Vec::with_capacity(writes.len() + frames.len());
    let mut frames = frames.into_iter().peekable();
    for write in writes {
        while let Some(frame) = frames.next_if(|frame| frame.time < write.time()) {
            merged.push(GuestWrite::Frame(frame));
        }
        merged.push(write);
    }
    merged.extend(frames.map(GuestWrite::Frame));
    merged
}

fn timestamp(value: &Value, field: &str) -> Result<Timestamp, Error> {
    value[field]
        .as_u64()
        .map(Timestamp::from_nanos)
        .ok_or_else(|| Error(format!("invalid QEMU {field}: {value}")))
}

impl Machine for TcgMachine {
    type Context = ();
    type Error = Error;
    type Snapshot = Snapshot;

    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Error> {
        if self.state == State::Invalid {
            return Err(Error("machine is invalid".into()));
        }
        let now = Timestamp::from_nanos(context.clock.now());
        if upto < self.reached
            || upto < now
            || self.pending_at.is_some_and(|at| upto < at)
            || upto.as_nanos() > i64::MAX as u64
        {
            return Err(Error(
                "execution boundary is backwards or exceeds QEMU's time range".into(),
            ));
        }
        let mut crashed_this_run = false;
        // Frames for the boot that runs next; a Crash or Reboot ends the boot
        // they were admitted for, so they never reach a later boot.
        let mut frames = Vec::new();
        let accepted = (|| -> Result<(), Error> {
            while let Some(input) = context.inbox.front() {
                match &input.payload {
                    seedbox_machine::pv::Request::Crash => {
                        crashed_this_run |=
                            !matches!(self.state, State::Exited(_)) || self.pending_boots > 0;
                        self.request_crash()?;
                        frames.clear();
                    }
                    seedbox_machine::pv::Request::Reboot => {
                        self.request_reboot(now.max(self.reached))?;
                        frames.clear();
                    }
                    seedbox_machine::pv::Request::Shutdown => self.request_shutdown()?,
                    seedbox_machine::pv::Request::ReceiveFrame { bytes } => {
                        if !seedbox_machine::pv::frame_in_bounds(bytes) {
                            return Err(Error("receive frame outside frame bounds".into()));
                        }
                        if self.manifest.network.is_some()
                            && (!matches!(self.state, State::Exited(_)) || self.pending_boots > 0)
                        {
                            frames.push(bytes.clone());
                        }
                    }
                    request @ (seedbox_machine::pv::Request::Application { .. }
                    | seedbox_machine::pv::Request::StepRealtime { .. }
                    | seedbox_machine::pv::Request::SimulationReply { .. }) => {
                        if !matches!(self.state, State::Exited(_)) || self.pending_boots > 0 {
                            self.input
                                .extend(self.guest.request(request).map_err(Error)?);
                        }
                    }
                    // The common request is non-exhaustive across crate boundaries.
                    _ => return Err(Error("unsupported machine request".into())),
                }
                context.inbox.take();
            }
            Ok(())
        })();
        if let Err(error) = accepted {
            self.state = State::Invalid;
            drop(self.process.take());
            return Err(error);
        }
        if self.pending_boots > 0 {
            let at = now
                .max(self.reached)
                .max(self.pending_at.unwrap_or(self.reached));
            if let Err(error) = self.initialize(context, at) {
                self.state = State::Invalid;
                drop(self.process.take());
                return Err(error);
            }
        }
        if let State::Exited(reason) = self.state {
            self.reached = now.max(self.reached);
            if upto > self.reached && !crashed_this_run {
                return Err(Error(
                    "exited machine requires a zero-budget run or Reboot".into(),
                ));
            }
            self.flush_ends(context)?;
            return Ok(StopReason::Exited {
                reached: self.reached,
                reason,
            });
        }
        if now > self.reached {
            if let Err(error) = self.process()?.advance(now) {
                self.state = State::Invalid;
                drop(self.process.take());
                return Err(error);
            }
            self.reached = now;
            // Adoption itself executes no instructions. Keep a blocked machine
            // blocked unless its reported timer has become due at this time.
            if matches!(self.state, State::Blocked { deadline: Some(at) } if at <= now) {
                self.state = State::Ready;
            }
        }
        if !self.input.is_empty() {
            let input = self.input.clone();
            match self.process()?.input(&input) {
                Ok(runnable) => {
                    self.input.clear();
                    if let Err(error) = self.guest.input_delivered() {
                        self.state = State::Invalid;
                        drop(self.process.take());
                        return Err(Error(error));
                    }
                    if runnable {
                        self.state = State::Ready;
                    }
                }
                Err(error) => {
                    self.state = State::Invalid;
                    drop(self.process.take());
                    return Err(error);
                }
            }
        }
        // Received frames follow lifecycle input at the adopted time; an
        // interrupt they raise makes a blocked CPU runnable without executing.
        if !frames.is_empty() {
            match self.process()?.net_receive(&frames) {
                Ok(true) => self.state = State::Ready,
                Ok(false) => {}
                Err(error) => {
                    self.state = State::Invalid;
                    drop(self.process.take());
                    return Err(error);
                }
            }
        }
        if self.reached < upto && self.state == State::Ready {
            if let Err(error) = self.execute(upto, context.execution) {
                self.state = State::Invalid;
                // A transport failure may leave QEMU executing. Reap it before
                // returning so the failed machine cannot keep producing effects.
                drop(self.process.take());
                return Err(error);
            }
        }
        let observed = self
            .guest
            .observe_ordered(context, std::mem::take(&mut self.writes))
            .map_err(Error);
        if let Err(error) = observed {
            self.state = State::Invalid;
            drop(self.process.take());
            return Err(error);
        }
        self.flush_ends(context)?;
        Ok(match self.state {
            State::Ready => StopReason::BoundaryReached,
            State::Blocked { deadline } => StopReason::Blocked {
                reached: self.reached,
                deadline,
            },
            State::Exited(reason) => StopReason::Exited {
                reached: self.reached,
                reason,
            },
            State::Invalid => return Err(Error("machine is invalid".into())),
        })
    }

    fn execution_evidence(
        &mut self,
    ) -> Result<Option<seedbox_machine::machine::ExecutionEvidence>, Error> {
        self.execution_observation()
    }

    fn state_evidence(&mut self) -> Result<Option<StateEvidence>, Error> {
        if self.state == State::Invalid {
            return Err(Error("machine is invalid".into()));
        }
        if !self
            .policy
            .as_ref()
            .is_some_and(|p| p.configuration.full_state)
        {
            return Ok(None);
        }
        if let Some(boot) = self.active_boot {
            if !matches!(self.state, State::Exited(_)) {
                let store = self.stopped_store()?;
                return self
                    .capture_evidence(boot, EvidenceDisposition::Live, store.as_ref())
                    .map(Some);
            }
        }
        // No native RAM is absence, not a failed observation. A full-state
        // service consumer must diagnose this None rather than invent RAM.
        Ok(self.last_evidence.clone())
    }

    fn snapshot(&mut self) -> Result<Snapshot, Error> {
        if self.state == State::Invalid {
            return Err(Error("cannot snapshot an invalid machine".into()));
        }
        let image = self.process.as_mut().map(Process::snapshot).transpose()?;
        let disk = match (&self.disk, &image) {
            (Some(_), Some(_)) | (None, _) => None,
            (Some(disk), None) => Some(
                disk.store
                    .as_ref()
                    .ok_or_else(|| Error("TCG disk has no owner".into()))?
                    .encode_overlays(),
            ),
        };
        Ok(Snapshot {
            disk,
            manifest: self.manifest.clone(),
            reached: self.reached,
            state: self.state,
            image,
            boot_seed: self.boot_seed,
            policy: self.policy.clone(),
            logical_boot: self.logical_boot,
            active_boot: self.active_boot,
            last_ended: self.last_ended.clone(),
            last_evidence: self.last_evidence.clone(),
            native_witness: self.native_witness.clone(),
            pending_ends: self.pending_ends.clone(),
            pending_boots: self.pending_boots,
            pending_at: self.pending_at,
            shutdown_requested: self.shutdown_requested,
            writes: self.writes.clone(),
            input: self.input.clone(),
            guest: self.guest.clone(),
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod elapsed_tests;

#[cfg(test)]
mod shutdown_tests;

#[cfg(test)]
mod execution_tests;

#[cfg(test)]
mod network_unit_tests;

/// Explicit fixture-only construction and stopped evidence for retained scenarios.
/// These helpers are absent unless `test-support` is enabled; they never export
/// native handles, boot manifests or mutable machine state.
// Driver scenarios need backend-local construction and assertions; a reverse
// dev-dependency or general private-state inspection would be broader.
#[cfg(feature = "test-support")]
pub mod test_support;

#[cfg(any(test, feature = "test-support"))]
mod raw_program_data;

#[cfg(test)]
mod linux_disk_tests;

#[cfg(test)]
mod disk_tests;

#[cfg(test)]
mod network_adapter_tests;
