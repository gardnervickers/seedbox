//! Native PV guest in a private, isolated worker process.
//!
//! A Linux kernel and initramfs for the host architecture run on
//! macOS/HVF or Linux/KVM (ARM64), or Linux/KVM (x86-64), with at most one
//! virtio block device (`/dev/vda`) from the definition's disk.
//! A definition with a network interface adds one virtio NIC (`eth0`) with its
//! MAC, and its kernel arguments carry `seedbox.ipv4=A.B.C.D/P` for the
//! guest's static address. Each due `ReceiveFrame` is taken: without a NIC,
//! in an ended boot or ahead of a Crash or Reboot in the same admission, it
//! is dropped; otherwise the NIC receives it at the run's start. The NIC's
//! backlog and rings belong to the worker snapshot. Transmitted frames are
//! published in guest order among console and lifecycle output.
//! The worker owns the disk during a boot; this machine owns it between boots.
//! When a boot ends (guest power-off or reset, an accepted Crash, or the
//! teardown for an accepted Reboot) the stopped worker's disk is exported
//! before the worker is terminated and its unflushed writes are discarded
//! exactly once; durable writes carry to later boots. A guest reset of the
//! virtio device keeps both layers; only guest FLUSH makes writes durable,
//! never the Shutdown request itself.
//! Snapshots preserve both layers without promotion and share the immutable
//! base, independent of the source machine.
//! A machine told to retain its snapshots ([`Retention`]) keeps those of a
//! live boot in the boot's worker, which then outlives the boot and the
//! machine for as long as one of them can rewind it.
//! Guest PV rendezvous define accepted time; host interruptions are invisible.
//! The Driver owns idle advancement. A standard virtio-console port named
//! `seedbox-lifecycle` carries `shutdown\n` to a cooperating guest supervisor.
//! The request itself executes no guest work. Cleanup and guest power-off require
//! later run calls; an uncooperative guest can remain blocked until crashed.
//! After the guest's ready handshake, a guest power-off or permanent halt ends
//! as `Shutdown` and a guest reset request (deliberate reboot or panic restart)
//! as `Crashed`; the worker never resets itself. Terminal requests before the
//! handshake, unknown exits and hypervisor failures are errors.
//! An encoded snapshot owns boot inputs and all stopped worker/device state.
//! Native architectural snapshot support currently requires macOS/HVF or
//! x86-64 KVM; Linux ARM64 KVM snapshots are unsupported.
mod boot;
mod client;
mod codec;
mod evidence;
mod execution;
mod factory;
mod faults;
#[cfg(test)]
mod guest;
mod policy;
mod protocol;
mod retained;
mod shm;
mod wire;
mod worker;
use client::Occupant;
pub use factory::Factory;
use protocol::{BootDisk, Request, Response, Stop};
use seedbox_machine::definition::{BackendPolicy, DiskImage};
use seedbox_machine::machine::disk::Store;
use seedbox_machine::machine::{EvidenceDisposition, StateEvidence};
use seedbox_machine::machine::{ExitReason, Machine, RunContext, StopReason, Timestamp};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};
use tempfile::TempDir;
pub(crate) use worker::serve as serve_worker;
#[cfg(feature = "test-support")]
pub(crate) use {client::preboot_errors, shm::accept};
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
enum State {
    Ready,
    Blocked(Option<u64>),
    Exited,
    Crashed,
    Invalid,
}
#[derive(Clone, Serialize, Deserialize)]
struct Capture {
    memory_bytes: u64,
    layout_version: u32,
    command_line: String,
    seed: u64,
    reached: u64,
    state: State,
    live: bool,
    shutdown: bool,
    pending_boots: u64,
    pending_at: Option<u64>,
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: seedbox_machine::pv::bridge::GuestBridge,
    /// Disk capacity in sectors. With a disk, a fourth blob carries the
    /// client store's overlays when no worker is live, or is empty.
    disk: Option<u64>,
    /// The NIC's MAC address; the command line already carries its address.
    network: Option<[u8; 6]>,
    /// Declared breakpoint names. A held machine is never captured.
    breaks: Vec<String>,
}
/// A stopped machine's internal state, including pending resets. It is
/// immutable, cheap to clone, and independent of the machine it was taken
/// from, which may run on or be dropped.
///
/// A snapshot of a live boot is a whole capture of the boot's worker, all of
/// RAM included, unless the machine retains its snapshots ([`Retention`]).
/// A retained snapshot stays in the worker instead. It keeps that worker,
/// and the scratch directory of the machine that took it, until it is
/// dropped, and using or dropping it waits for a run in progress there.
#[derive(Clone)]
pub struct Snapshot {
    body: Body,
    /// The immutable base, shared rather than copied into the body.
    disk: Option<DiskImage>,
    aux: retained::Aux,
}
// Every kind is immutable and shared, so a caller that keeps many restore
// branches copies none of it.
#[derive(Clone)]
enum Body {
    /// A whole capture, as [`assemble`] lays it out: a decoded snapshot, or a
    /// live boot copied out of its worker.
    Image(Arc<Vec<u8>>),
    /// A machine between boots.
    Stopped(Arc<Stopped>),
    /// A live boot kept in its worker.
    Retained(Arc<Retained>),
}
/// The part of a snapshot that is outside any worker.
struct Inputs {
    capture: Capture,
    kernel: Arc<Vec<u8>>,
    initrd: Arc<Vec<u8>>,
}
struct Stopped {
    inputs: Inputs,
    /// The disk's store, which the machine owns between boots.
    store: Option<Store>,
}
struct Retained {
    inputs: Inputs,
    version: client::Version,
    /// RAM pages the worker copied to keep it.
    copied: u64,
    /// A rewind to it compares all of RAM with it afterwards.
    verify: bool,
}
/// How a machine keeps the snapshots of its live boots.
///
/// Without it each is a whole capture: taking one copies all of RAM out of
/// the worker and restoring one loads all of it into a new worker. With it
/// the boot's worker keeps the snapshot and copies only the RAM pages written
/// since its last one, and [`PVMachine::from_snapshot`] rewinds that worker
/// in place when no machine is running in it ([`Restored`]).
///
/// This is host authority, like the scratch root: no snapshot carries it,
/// and a restored machine retains nothing until it is told to. Backends that
/// cannot record guest writes (all but macOS/HVF) capture whole regardless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// The most bytes of RAM pages one worker's snapshots may hold together;
    /// a page they share counts once. Once a worker has taken a snapshot it
    /// holds a copy of RAM as the base of its next, snapshots or none.
    ///
    /// This bounds RAM pages only. Each snapshot also holds, uncounted, its
    /// machine state (about 130 KB on HVF, nearly all of it the GIC's), a
    /// copy of the 16 KiB PV page, 2 KiB of page table for each 4 MiB of RAM
    /// in which it copied a page (at most an eighth of the pages it copied),
    /// and the disk's writes not yet flushed. A caller that keeps many
    /// snapshots bounds how many as well.
    pub limit_bytes: u64,
    /// Check each snapshot against a whole capture taken with it, and all of
    /// RAM after each rewind. Each check costs as much as the capture it
    /// replaces; for tests and soak runs.
    pub verify: bool,
}
/// The machine's worker refused a snapshot that would take its snapshots
/// over [`Retention::limit_bytes`]. Nothing was kept and the machine is
/// unchanged: drop snapshots of this boot and take it again.
///
/// The inner error of the [`std::io::ErrorKind::QuotaExceeded`] error that
/// [`Machine::snapshot`] then returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotStoreFull {
    /// Bytes of RAM pages the worker's snapshots hold.
    pub held_bytes: u64,
    pub limit_bytes: u64,
}
impl std::fmt::Display for SnapshotStoreFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PV snapshot store is full: {} bytes held, limit {}",
            self.held_bytes, self.limit_bytes
        )
    }
}
impl std::error::Error for SnapshotStoreFull {}
/// How [`PVMachine::from_snapshot`] produced a machine. Only `Rewound` costs
/// less than loading all of RAM into a new worker, so a caller that expects
/// rewinds counts the others.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restored {
    /// The snapshot's worker was parked and went back to it in place,
    /// rewriting this many RAM pages.
    Rewound { pages: u64 },
    /// A machine was running in the snapshot's worker, so the snapshot was
    /// copied out of it and loaded into a new one.
    Exported,
    /// A whole capture was loaded into a new worker.
    Loaded,
    /// The machine was between boots: no worker starts until it next runs.
    Deferred,
}
/// Whether this backend records guest writes, which retained snapshots need.
const RETAINS: bool = cfg!(all(target_arch = "aarch64", target_os = "macos"));
/// The machine's disk: its immutable base and, between boots, its store.
// Exactly one owner at a time (the worker during a boot, this machine between
// boots) keeps boot-end loss a single client decision.
struct Disk {
    base: DiskImage,
    /// A store over the base with empty overlays; clones share its base.
    empty: Store,
    /// `Some` exactly when no worker owns the disk.
    store: Option<Store>,
}
impl Disk {
    fn new(base: DiskImage) -> Self {
        let empty = match base.image() {
            Some(image) => Store::from_image(image.clone()),
            None => Store::zeroed(base.sectors()),
        }
        .expect("validated disk capacity");
        Self {
            base,
            store: Some(empty.clone()),
            empty,
        }
    }
}

/// A single native PV guest. Construction is its only backend-specific API.
pub struct PVMachine {
    memory_bytes: u64,
    kernel: Arc<Vec<u8>>,
    initrd: Arc<Vec<u8>>,
    command_line: String,
    seed: u64,
    reached: u64,
    state: State,
    worker: Option<Occupant>,
    /// Shared with the workers this machine starts, which outlive it while a
    /// retained snapshot refers to them.
    directory: Arc<TempDir>,
    /// The boot inputs are written beneath `directory`.
    staged: bool,
    retention: Option<Retention>,
    restored: Option<Restored>,
    console: Vec<u8>,
    executable: PathBuf,
    shutdown: bool,
    pending_boots: u64,
    pending_at: Option<u64>,
    writes: Vec<seedbox_machine::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: seedbox_machine::pv::bridge::GuestBridge,
    disk: Option<Disk>,
    /// The NIC's MAC address, when the definition has a network interface.
    network: Option<[u8; 6]>,
    /// Declared breakpoint names, ascending; they outlive boots.
    breaks: Vec<String>,
    /// The current boot stopped at a declared breakpoint write or a held
    /// disk request and runs no guest work until ResumeBreak, Crash or Reboot.
    held: bool,
    /// ResumeBreak released a hold this run: the worker continues a held
    /// disk request at the start of its next run, which is this run's.
    released: bool,
    /// The hit this run stopped at, a [`seedbox_machine::pv::Response::Break`]
    /// or [`seedbox_machine::pv::Response::BlockRequestHeld`], reported after
    /// its writes.
    hit: Option<seedbox_machine::pv::Response>,
    aux: retained::Aux,
}
/// Kernel arguments for a definition: the assets' arguments plus, with a
/// network interface, the guest's static address (`guest_network.h`).
fn command_line(
    arguments: &str,
    network: Option<&seedbox_machine::definition::NetworkInterface>,
) -> (String, Option<[u8; 6]>) {
    match network {
        Some(interface) => (
            format!(
                "{arguments} seedbox.ipv4={}/{}",
                interface.ipv4(),
                interface.prefix_len()
            ),
            Some(interface.mac().octets()),
        ),
        None => (arguments.to_owned(), None),
    }
}
#[cfg(all(any(test, feature = "test-support"), target_arch = "aarch64"))]
const TEST_COMMAND_LINE: &str = "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init nokaslr nosmp maxcpus=1 random.trust_cpu=off random.trust_bootloader=on mitigations=off panic=-1 loglevel=4";
#[cfg(all(any(test, feature = "test-support"), target_arch = "x86_64"))]
const TEST_COMMAND_LINE: &str = "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4";

impl Snapshot {
    /// Validate the retained optional named policy before native restore.
    // Policy authority comes from the caller's factory, never from imported
    // snapshot data; this check exposes no RAM or private native representation.
    pub fn validate_policy(
        &self,
        name: &str,
        expected: Option<&BackendPolicy>,
    ) -> Result<(), String> {
        policy::authority(self.aux.policy.as_ref(), name, expected)
    }
    /// The RAM pages the worker copied to keep this snapshot: those written
    /// since its previous one. `None` unless it is retained in a worker.
    pub fn copied_pages(&self) -> Option<u64> {
        match &self.body {
            Body::Retained(retained) => Some(retained.copied),
            _ => None,
        }
    }
    /// The snapshot as one whole capture, which is what an encoded snapshot
    /// carries. A retained snapshot is copied out of its worker on each call.
    fn image(&self) -> Result<Arc<Vec<u8>>, String> {
        match &self.body {
            Body::Image(bytes) => Ok(bytes.clone()),
            Body::Stopped(stopped) => {
                let overlays = stopped.store.as_ref().map(Store::encode_overlays);
                assemble(&stopped.inputs, None, overlays).map(Arc::new)
            }
            Body::Retained(retained) => {
                let directory = tempfile::Builder::new()
                    .prefix("export-")
                    .tempdir_in(retained.version.scratch())
                    .map_err(|e| format!("PV snapshot path: {e}"))?;
                let image = directory.path().join("image");
                retained.version.export(&image)?;
                // The worker's image carries its disk's overlays.
                let overlays = retained.inputs.capture.disk.map(|_| Vec::new());
                assemble(&retained.inputs, Some(&image), overlays).map(Arc::new)
            }
        }
    }
    #[cfg(test)]
    fn bytes(&self) -> Arc<Vec<u8>> {
        self.image().expect("snapshot image")
    }
}
// Checkpoints are kept and restored across the Driver's worker threads.
const _: fn() = || {
    fn shared<T: Clone + Send + Sync>() {}
    shared::<Snapshot>();
};
/// Lay out a whole capture: the framed header, the boot inputs, the image a
/// live boot's worker wrote at `image` and, with a disk, the store's overlays
/// (empty when the worker's image carries them).
fn assemble(
    inputs: &Inputs,
    image: Option<&Path>,
    overlays: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let mut image = match image {
        Some(path) => {
            let file = fs::File::open(path).map_err(|e| format!("PV snapshot image: {e}"))?;
            let length = file
                .metadata()
                .map_err(|e| format!("PV snapshot size: {e}"))?
                .len();
            Some((file, length))
        }
        None => None,
    };
    let mut bytes = Vec::new();
    protocol::send(&mut bytes, &inputs.capture)?;
    let image_len = image.as_ref().map_or(0, |(_, length)| *length);
    let extra = (inputs.kernel.len() as u64)
        .checked_add(inputs.initrd.len() as u64)
        .and_then(|v| v.checked_add(image_len))
        .and_then(|v| v.checked_add(overlays.as_ref().map_or(0, |v| v.len()) as u64))
        .and_then(|v| v.checked_add(if overlays.is_some() { 32 } else { 24 }))
        .and_then(|v| usize::try_from(v).ok())
        .ok_or("PV snapshot size overflow")?;
    bytes
        .try_reserve_exact(extra)
        .map_err(|e| format!("PV snapshot allocation: {e}"))?;
    for blob in [&inputs.kernel[..], &inputs.initrd[..]] {
        bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
        bytes.extend_from_slice(blob);
    }
    bytes.extend_from_slice(&image_len.to_le_bytes());
    if let Some((file, length)) = &mut image {
        let count = std::io::copy(&mut file.take(*length), &mut bytes)
            .map_err(|e| format!("PV snapshot image read: {e}"))?;
        if count != *length {
            return Err("PV snapshot truncated image".into());
        }
    }
    if let Some(overlays) = overlays {
        bytes.extend_from_slice(&(overlays.len() as u64).to_le_bytes());
        bytes.extend_from_slice(&overlays);
    }
    Ok(bytes)
}
/// Whether two files hold the same bytes.
fn same_contents(a: &Path, b: &Path) -> Result<bool, String> {
    let open = |path: &Path| {
        let file = fs::File::open(path).map_err(|e| format!("PV snapshot image: {e}"))?;
        let length = file
            .metadata()
            .map_err(|e| format!("PV snapshot size: {e}"))?
            .len();
        Ok::<_, String>((file, length))
    };
    let ((mut a, length), (mut b, other)) = (open(a)?, open(b)?);
    if length != other {
        return Ok(false);
    }
    let read = |e| format!("PV snapshot image read: {e}");
    let (mut left, mut right) = (vec![0; 1 << 20], vec![0; 1 << 20]);
    loop {
        let count = a.read(&mut left).map_err(read)?;
        if count == 0 {
            return Ok(true);
        }
        b.read_exact(&mut right[..count]).map_err(read)?;
        if left[..count] != right[..count] {
            return Ok(false);
        }
    }
}
impl PVMachine {
    /// Keep the snapshots this machine takes of its live boots in their
    /// workers from now on.
    pub fn retain_snapshots(&mut self, retention: Retention) {
        self.retention = Some(retention);
    }
    /// How this machine came out of a snapshot; `None` if it did not.
    pub fn restored(&self) -> Option<Restored> {
        self.restored
    }
    fn configure_policy(&mut self, name: &str, p: &BackendPolicy) -> Result<(), std::io::Error> {
        if self.worker.is_some()
            || self.aux.policy.is_some()
            || self.aux.logical != 0
            || self.pending_boots != 1
            || self.reached != 0
            || self.state != State::Ready
            || !self.input.is_empty()
            || !self.writes.is_empty()
            || self.shutdown
        {
            return Err(std::io::Error::other(
                "PV policy requires untouched initial machine",
            ));
        }
        let selected = policy::Named::new(name, p).map_err(std::io::Error::other)?;
        if self
            .command_line
            .split_whitespace()
            .any(|w| w.starts_with("seedbox.control="))
        {
            return Err(std::io::Error::other("PV base arguments set seedbox.control; remove it and choose the controller through BackendPolicy"));
        }
        let command = format!(
            "{} seedbox.control={}",
            self.command_line,
            u8::from(p.controller.as_deref() == Some(name))
        );
        boot::arguments(&command).map_err(std::io::Error::other)?;
        self.command_line = command;
        self.aux.policy = Some(selected);
        Ok(())
    }
    fn flush_ends(&mut self, context: &mut RunContext<'_, ()>) -> Result<(), String> {
        while let Some(r) = self.aux.pending.front() {
            context
                .outbox
                .emit(seedbox_machine::pv::Output {
                    at: Timestamp::from_nanos(r.at.max(context.clock.now())),
                    payload: r.response(),
                })
                .map_err(|e| format!("PV boot-end output time: {e:?}"))?;
            self.aux.pending.pop_front();
        }
        Ok(())
    }
    /// Prepare a stopped machine from a workload and separately supplied boot assets.
    /// Unsupported resource requests are rejected before execution.
    /// The first run initializes boot using its clock and eight RNG bytes in
    /// little-endian order. Each pending reboot draws another eight bytes.
    #[cfg(test)]
    pub(crate) fn new(
        executable: &Path,
        bootstrap: &seedbox_machine::application::GuestBootstrap,
        definition: seedbox_machine::definition::MachineDefinition,
    ) -> Result<Self, std::io::Error> {
        boot::RamLayout::native(definition.resources.memory_bytes)
            .map_err(std::io::Error::other)?;
        // Preparation may derive a private application copy before packaging;
        // the original prepared workload is never mutated by the backend.
        let initrd = seedbox_machine::application::boot_archive(&definition.application, bootstrap)
            .map_err(std::io::Error::other)?;
        let (command_line, network) =
            command_line(&bootstrap.kernel.command_line, definition.network.as_ref());
        Self::construct_with_memory(
            executable,
            bootstrap.kernel.image.clone(),
            initrd,
            command_line,
            definition.disk,
            network,
            definition.resources.memory_bytes,
        )
        .map_err(std::io::Error::other)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub(super) fn from_boot_inputs(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
    ) -> Result<Self, std::io::Error> {
        Self::construct_with_memory(
            executable,
            kernel,
            initrd,
            TEST_COMMAND_LINE.into(),
            None,
            None,
            128 << 20, // Explicit portable/native fixture default, not a production limit.
        )
        .map_err(std::io::Error::other)
    }
    #[cfg(test)]
    fn construct(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        command_line: String,
        disk: Option<DiskImage>,
        network: Option<[u8; 6]>,
    ) -> Result<Self, String> {
        Self::construct_with_memory(
            executable,
            kernel,
            initrd,
            command_line,
            disk,
            network,
            boot::RAM_BYTES as u64,
        )
    }
    /// Validate the definition's resource and boot placement before creating
    /// temporary inputs; the fixture constructor supplies its explicit default.
    fn construct_with_memory(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        command_line: String,
        disk: Option<DiskImage>,
        network: Option<[u8; 6]>,
        memory_bytes: u64,
    ) -> Result<Self, String> {
        Self::construct_with_memory_in(
            executable,
            kernel,
            initrd,
            command_line,
            disk,
            network,
            memory_bytes,
            None,
        )
    }
    /// Scratch authority is supplied by the host, never by snapshot contents.
    #[allow(clippy::too_many_arguments)]
    fn construct_with_memory_in(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        command_line: String,
        disk: Option<DiskImage>,
        network: Option<[u8; 6]>,
        memory_bytes: u64,
        scratch_root: Option<&Path>,
    ) -> Result<Self, String> {
        let mut machine = Self::unstaged(
            executable,
            kernel,
            initrd,
            command_line,
            disk,
            network,
            memory_bytes,
            scratch_root,
        )?;
        machine.stage()?;
        Ok(machine)
    }
    /// A machine whose boot inputs are not yet beneath its scratch directory.
    // A machine rewound into a parked worker boots nothing, so it writes none.
    #[allow(clippy::too_many_arguments)]
    fn unstaged(
        executable: &Path,
        kernel: impl Into<Arc<Vec<u8>>>,
        initrd: impl Into<Arc<Vec<u8>>>,
        command_line: String,
        disk: Option<DiskImage>,
        network: Option<[u8; 6]>,
        memory_bytes: u64,
        scratch_root: Option<&Path>,
    ) -> Result<Self, String> {
        let (kernel, initrd) = (kernel.into(), initrd.into());
        boot::arguments(&command_line)?;
        let layout = boot::RamLayout::native(memory_bytes)?;
        boot::layout_for(&layout, &kernel, &initrd)?;
        Ok(Self {
            memory_bytes: layout.capacity,
            kernel,
            initrd,
            command_line,
            seed: 0,
            reached: 0,
            state: State::Ready,
            worker: None,
            directory: Arc::new(scratch_directory(scratch_root)?),
            staged: false,
            retention: None,
            restored: None,
            console: Vec::new(),
            executable: executable.to_owned(),
            pending_boots: 1,
            pending_at: None,
            writes: Default::default(),
            input: Vec::new(),
            guest: Default::default(),
            shutdown: false,
            disk: disk.map(Disk::new),
            network,
            breaks: Vec::new(),
            held: false,
            released: false,
            hit: None,
            aux: Default::default(),
        })
    }
    /// Write the boot inputs every worker of this machine reads, once.
    fn stage(&mut self) -> Result<(), String> {
        if self.staged {
            return Ok(());
        }
        let directory = self.directory.path();
        fs::write(directory.join("kernel"), &self.kernel[..])
            .map_err(|e| format!("PV kernel input: {e}"))?;
        fs::write(directory.join("initrd"), &self.initrd[..])
            .map_err(|e| format!("PV initrd input: {e}"))?;
        if let Some(image) = self.disk.as_ref().and_then(|disk| disk.base.image()) {
            fs::write(directory.join("disk-base"), &image[..])
                .map_err(|e| format!("PV disk base input: {e}"))?;
        }
        self.staged = true;
        Ok(())
    }
    fn boot(&mut self, seed: u64, now: u64) -> Result<(), String> {
        self.boot_restore(seed, now, None)
    }
    fn boot_restore(
        &mut self,
        seed: u64,
        now: u64,
        restore: Option<PathBuf>,
    ) -> Result<(), String> {
        self.stage()?;
        let disk = match &self.disk {
            Some(disk) => {
                let store = disk
                    .store
                    .as_ref()
                    .ok_or("PV disk still owned by a worker")?;
                let overlay = self.directory.path().join("disk-overlay");
                fs::write(&overlay, store.encode_overlays())
                    .map_err(|e| format!("PV disk overlay input: {e}"))?;
                Some(BootDisk {
                    sectors: disk.base.sectors(),
                    base: disk
                        .base
                        .image()
                        .map(|_| self.directory.path().join("disk-base")),
                    overlay: Some(overlay),
                })
            }
            None => None,
        };
        let owner = if restore.is_some() {
            self.aux
                .active
                .ok_or("PV restore image has no physical owner")?
        } else {
            self.aux.active.unwrap_or(self.aux.logical)
        };
        let selected = self.aux.policy.as_ref().map(|p| p.setup(owner));
        let witness = self.witness_setup(owner)?;
        let mut worker = Occupant::spawn(&self.executable, &self.directory)?;
        protocol::ready(
            worker.call(Request::Boot {
                witness: Box::new(witness.clone()),
                memory_bytes: self.memory_bytes,
                layout_version: boot::LAYOUT_VERSION,
                restore,
                kernel: self.directory.path().join("kernel"),
                initrd: self.directory.path().join("initrd"),
                seed,
                now,
                command_line: self.command_line.clone(),
                disk,
                network: self.network,
                policy: Box::new(selected.clone()),
                faults: Box::new(self.aux.faults.boot(owner)),
            })?,
            self.memory_bytes,
            selected.as_ref(),
            &witness,
        )?;
        self.worker = Some(worker);
        self.aux.active = Some(owner);
        self.seed = seed;
        if let Some(disk) = &mut self.disk {
            disk.store = None;
        }
        Ok(())
    }
    /// End the live boot, if any: export its stopped disk, discard the
    /// unflushed writes exactly once, then leave the worker, which exits
    /// unless a retained snapshot refers to it. Ended boots have no worker,
    /// so repeating this never discards twice.
    fn end_boot(&mut self, shutdown: bool, at: u64, mark: u8) -> Result<(), String> {
        let boot = self.aux.active.unwrap_or(self.aux.logical);
        if self.aux.last.as_ref().is_some_and(|e| e.boot >= boot) {
            return Ok(());
        }
        let native = self.worker.is_some();
        if !native && self.aux.policy.is_none() {
            return Ok(());
        }
        if native {
            let witness = match self.aux.retiring.take() {
                Some(w) => w,
                None => self.capture_execution()?,
            };
            self.aux.execution = Some(retained::EndedExecution {
                boot,
                shutdown,
                witness,
                mark,
                at,
            });
        }
        let store = self.stopped_store()?;
        if native && self.aux.faults.live() {
            // The ending boot's counts and hold progress carry to the next.
            let report = self.fault_call(protocol::FaultChange::Report)?;
            self.aux.faults.retire(&report)?;
        }
        let disposition = if shutdown {
            EvidenceDisposition::Shutdown
        } else {
            EvidenceDisposition::Crashed
        };
        let capture = if native
            && self
                .aux
                .policy
                .as_ref()
                .is_some_and(|p| p.settings.full_state)
        {
            Some(self.capture_evidence(boot, disposition, store.as_ref())?)
        } else {
            None
        };
        let report = self
            .aux
            .policy
            .as_ref()
            .is_some_and(|p| p.settings.boot_end_evidence);
        let mut lost = 0;
        let mut surviving = None;
        if let Some(store) = store {
            let (next, n) = evidence::lose(&store, self.aux.policy.as_ref(), boot)?;
            lost = n;
            if report {
                surviving = Some(evidence::disk_digest(
                    &next,
                    self.aux
                        .policy
                        .as_ref()
                        .is_some_and(|p| p.settings.torn_writes),
                )?);
            }
            self.disk.as_mut().expect("store has disk").store = Some(next);
        }
        if let Some(worker) = self.worker.take() {
            worker.leave()?;
        }
        self.aux.active = None;
        let end = retained::Ended {
            boot,
            shutdown,
            native,
        };
        if report {
            self.aux.pending.push_back(retained::Report {
                at,
                end: end.clone(),
                lost,
                disk: surviving,
                retired: capture.as_ref().map(|e| e.digest.clone()),
            });
        }
        self.aux.last = Some(end);
        self.aux.cache = capture.map(retained::Cached::from);
        Ok(())
    }
    fn request_shutdown(&mut self) -> Result<(), std::io::Error> {
        if self.state == State::Invalid {
            return Err(std::io::Error::other("cannot shut down invalid PV machine"));
        }
        if (self.pending_boots > 0 || !matches!(self.state, State::Exited | State::Crashed))
            && !self.shutdown
        {
            self.shutdown = true;
            self.input.extend_from_slice(b"shutdown\n");
        }
        Ok(())
    }
    fn request_crash(&mut self) -> Result<(), std::io::Error> {
        if self.state == State::Invalid {
            return Err(std::io::Error::other("PV crash requires valid machine"));
        }
        self.held = false;
        self.released = false;
        if matches!(self.state, State::Exited | State::Crashed) && self.pending_boots == 0 {
            self.guest.reset();
            self.input.clear();
            self.writes.clear();
            self.shutdown = false;
            return Ok(());
        }
        self.state = State::Invalid;
        self.end_boot(false, self.reached, 1)
            .map_err(std::io::Error::other)?;
        self.shutdown = false;
        self.guest.reset();
        self.input.clear();
        self.writes.clear();
        self.state = State::Crashed;
        self.pending_boots = 0;
        self.pending_at = None;
        Ok(())
    }
    fn request_reboot(&mut self, at: Timestamp) -> Result<(), std::io::Error> {
        let at = at.as_nanos();
        if self.state == State::Invalid
            || at < self.reached
            || self.pending_at.is_some_and(|previous| at < previous)
            || at == u64::MAX
        {
            return Err(std::io::Error::other(
                "PV reboot requires valid finite barrier at or after machine position",
            ));
        }
        let count = self
            .pending_boots
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("PV pending reboot count exhausted"))?;
        let boot = self
            .aux
            .logical
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("PV logical boot count exhausted"))?;
        if self.worker.is_some() && self.aux.retiring.is_none() {
            self.aux.retiring = Some(self.capture_execution().map_err(std::io::Error::other)?);
        } else if self.worker.is_none() {
            if let Some(old) = &mut self.aux.execution {
                if old.mark == 3 {
                    old.at = at;
                }
            }
        }
        self.pending_boots = count;
        self.aux.logical = boot;
        self.pending_at = Some(at);
        self.held = false;
        self.released = false;
        self.shutdown = false;
        self.guest.reset();
        self.input.clear();
        self.writes.clear();
        Ok(())
    }
    fn initialize(&mut self, context: &mut RunContext<'_, ()>, at: u64) -> Result<(), String> {
        self.state = State::Invalid;
        if self.pending_boots == 0 {
            return Err("PV initialization without reservation".into());
        }
        let first = self
            .aux
            .logical
            .checked_sub(self.pending_boots - 1)
            .ok_or("PV pending boot ordinals differ")?;
        if self.worker.is_some() {
            self.end_boot(false, at, 2)?;
        }
        for index in 0..self.pending_boots {
            let mut bytes = [0; 8];
            context.rng.fill_bytes(&mut bytes);
            if self.worker.is_some() {
                self.end_boot(false, at, 2)?;
            }
            self.aux.active = Some(first + index);
            self.boot(u64::from_le_bytes(bytes), at)?;
        }
        self.reached = at;
        self.pending_boots = 0;
        self.pending_at = None;
        self.state = State::Ready;
        Ok(())
    }
    fn worker(&mut self) -> Result<&mut Occupant, String> {
        self.worker.as_mut().ok_or("PV worker unavailable".into())
    }
    fn fault_call(
        &mut self,
        change: protocol::FaultChange,
    ) -> Result<protocol::FaultReport, String> {
        match self.worker()?.call(Request::Fault(change))? {
            Response::Faults(report) => Ok(report),
            _ => Err("PV unexpected fault response".into()),
        }
    }
    /// Apply a device fault request: to the retained mirror, and to the live
    /// worker if there is one. A boot that has ended keeps it for the next.
    fn device_request(&mut self, request: &seedbox_machine::pv::Request) -> Result<(), String> {
        use protocol::FaultChange;
        use seedbox_machine::pv::Request as R;
        let live = self.worker.is_some();
        match *request {
            R::InstallFault { id, seed, fault } => {
                self.aux.faults.install(id, seed, fault)?;
                if live {
                    let installed = self.aux.faults.installed(id).expect("just installed");
                    self.fault_call(FaultChange::Install(installed))?;
                }
            }
            R::HealFault { id } => {
                let live = if live {
                    self.fault_call(FaultChange::Heal(id))?
                        .healed
                        .ok_or("PV worker heal without counts")?
                } else {
                    Default::default()
                };
                self.aux.faults.heal(id, live)?;
            }
            R::BreakOnBlockRequest { op, nth } => {
                let hold = seedbox_machine::machine::disk::fault::Hold { op, nth, seen: 0 };
                self.aux.faults.hold(hold)?;
                if live {
                    self.fault_call(FaultChange::Hold(hold))?;
                }
            }
            _ => return Err("PV request is not a device fault".into()),
        }
        Ok(())
    }
    fn execute(
        &mut self,
        now: u64,
        upto: u64,
        syscall_cost_ns: u64,
        frames: &[Vec<u8>],
    ) -> Result<StopReason, String> {
        let now = now.max(self.reached);
        let shutdown = false;
        let input = self.input.clone();
        let breaks = self.breaks.clone();
        let resume = std::mem::take(&mut self.released);
        let (time, stop, writes) = self.worker()?.run(
            Request::Run {
                now,
                upto,
                syscall_cost_ns,
                shutdown,
                lifecycle: input,
                frames: 0,
                breaks,
                resume,
            },
            frames,
        )?;
        if time < now || time < self.reached || time > upto {
            return Err("PV worker crossed execution interval".into());
        }
        if writes
            .iter()
            .any(|write| write.time() < now || write.time() > time)
            || writes
                .windows(2)
                .any(|pair| pair[0].time() > pair[1].time())
        {
            return Err("PV guest write crossed execution interval".into());
        }
        self.input.clear();
        self.guest.input_delivered()?;
        self.reached = time;
        for write in &writes {
            if let seedbox_machine::pv::transport::GuestWrite::Console(write) = write {
                self.console.extend_from_slice(&write.bytes);
            }
        }
        self.writes.extend(writes);
        match stop {
            Stop::Horizon if time == upto => {
                self.state = State::Ready;
                Ok(StopReason::BoundaryReached)
            }
            Stop::Break(name) => {
                self.held = true;
                self.state = State::Blocked(None);
                self.hit = Some(seedbox_machine::pv::Response::Break { name });
                Ok(StopReason::Blocked {
                    reached: Timestamp::from_nanos(time),
                    deadline: None,
                })
            }
            Stop::Blocked(deadline) => {
                if deadline.is_some_and(|d| d <= time) {
                    return Err("PV worker blocked with expired deadline".into());
                }
                self.state = State::Blocked(deadline);
                Ok(StopReason::Blocked {
                    reached: Timestamp::from_nanos(time),
                    deadline: deadline.map(Timestamp::from_nanos),
                })
            }
            Stop::PoweredOff => {
                self.end_boot(true, time, 3)?;
                self.state = State::Exited;
                Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(time),
                    reason: ExitReason::Shutdown,
                })
            }
            // A guest reset request ends this boot; only an accepted Reboot
            // request starts another one.
            Stop::Reset => {
                self.end_boot(false, time, 3)?;
                self.state = State::Crashed;
                Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(time),
                    reason: ExitReason::Crashed,
                })
            }
            // The worker keeps the request held; its next run continues it.
            Stop::Held(hold) => {
                self.held = true;
                self.state = State::Blocked(None);
                self.hit = Some(seedbox_machine::pv::Response::BlockRequestHeld {
                    op: hold.op,
                    nth: hold.nth,
                });
                Ok(StopReason::Blocked {
                    reached: Timestamp::from_nanos(time),
                    deadline: None,
                })
            }
            _ => Err("PV worker returned invalid stop".into()),
        }
    }
    fn capturable(&self) -> Result<(), String> {
        if self.state == State::Invalid {
            return Err("cannot snapshot invalid PV machine".into());
        }
        if self.held || self.released {
            return Err("cannot snapshot PV machine held at a breakpoint".into());
        }
        Ok(())
    }
    /// What a snapshot of the machine as it stands holds outside a worker.
    fn inputs(&self) -> Inputs {
        Inputs {
            capture: Capture {
                memory_bytes: self.memory_bytes,
                layout_version: boot::LAYOUT_VERSION,
                command_line: self.command_line.clone(),
                seed: self.seed,
                reached: self.reached,
                state: self.state,
                live: self.worker.is_some(),
                shutdown: self.shutdown,
                pending_boots: self.pending_boots,
                pending_at: self.pending_at,
                writes: self.writes.clone(),
                input: self.input.clone(),
                guest: self.guest.clone(),
                disk: self.disk.as_ref().map(|disk| disk.base.sectors()),
                network: self.network,
                breaks: self.breaks.clone(),
            },
            kernel: self.kernel.clone(),
            initrd: self.initrd.clone(),
        }
    }
    fn snapshot_of(&self, body: Body) -> Snapshot {
        Snapshot {
            body,
            aux: self.aux.clone(),
            disk: self.disk.as_ref().map(|disk| disk.base.clone()),
        }
    }
    /// A snapshot that depends on no worker: a live boot copied whole out of
    /// its worker, or the machine as it stands between boots.
    fn capture(&mut self) -> Result<Snapshot, String> {
        self.capturable()?;
        let body = if self.worker.is_some() {
            let directory = tempfile::Builder::new()
                .prefix("capture-")
                .tempdir_in(self.directory.path())
                .map_err(|e| format!("PV snapshot path: {e}"))?;
            let artifact = directory.path().join("image");
            if !matches!(
                self.worker()?.call(Request::Capture(artifact.clone()))?,
                Response::Captured
            ) {
                return Err("PV unexpected snapshot response".into());
            }
            // A live worker's image already carries its disk's overlays.
            let overlays = self.disk.as_ref().map(|_| Vec::new());
            Body::Image(Arc::new(assemble(
                &self.inputs(),
                Some(&artifact),
                overlays,
            )?))
        } else {
            let store = match &self.disk {
                Some(disk) => Some(disk.store.clone().ok_or("PV disk has no owner")?),
                None => None,
            };
            Body::Stopped(Arc::new(Stopped {
                inputs: self.inputs(),
                store,
            }))
        };
        Ok(self.snapshot_of(body))
    }
    /// Keep the live boot in its worker as a retained snapshot.
    fn retain(&mut self, retention: Retention) -> Result<Snapshot, std::io::Error> {
        self.capturable().map_err(std::io::Error::other)?;
        let taken = self
            .worker()
            .and_then(|worker| worker.snapshot(retention.limit_bytes, retention.verify))
            .map_err(std::io::Error::other)?
            .map_err(|held_bytes| {
                std::io::Error::new(
                    std::io::ErrorKind::QuotaExceeded,
                    SnapshotStoreFull {
                        held_bytes,
                        limit_bytes: retention.limit_bytes,
                    },
                )
            })?;
        if retention.verify {
            // A failure drops the snapshot, which the worker then forgets.
            self.verify_retained(&taken.version)
                .map_err(std::io::Error::other)?;
        }
        Ok(self.snapshot_of(Body::Retained(Arc::new(Retained {
            inputs: self.inputs(),
            version: taken.version,
            copied: taken.pages,
            verify: retention.verify,
        }))))
    }
    /// Check that a retained snapshot just taken exports, byte for byte, the
    /// image a capture of the machine writes now.
    fn verify_retained(&mut self, version: &client::Version) -> Result<(), String> {
        let directory = tempfile::Builder::new()
            .prefix("verify-")
            .tempdir_in(self.directory.path())
            .map_err(|e| format!("PV snapshot path: {e}"))?;
        let capture = directory.path().join("capture");
        if !matches!(
            self.worker()?.call(Request::Capture(capture.clone()))?,
            Response::Captured
        ) {
            return Err("PV unexpected snapshot response".into());
        }
        let export = directory.path().join("export");
        version.export(&export)?;
        if !same_contents(&capture, &export)? {
            return Err("PV retained snapshot differs from a capture of the machine".into());
        }
        Ok(())
    }
    /// The durable disk between boots; `None` while a worker owns it.
    #[cfg(test)]
    fn disk_store(&self) -> Option<&Store> {
        self.disk.as_ref()?.store.as_ref()
    }
    /// Reconstruct a stopped machine from an independently owned capture.
    /// The explicit executable must implement this worker protocol on a supported
    /// native PV host (ARM64 Linux/macOS or x86-64 Linux). An unstarted capture
    /// retains deferred worker startup; a live capture starts a compatible worker
    /// and restores its native state before returning, without executing guest work.
    /// Live architectural restore is implemented for ARM64 macOS/HVF and
    /// x86-64 Linux/KVM, not ARM64 Linux/KVM. Unstarted captures need no native
    /// architectural restore. This operation does not restore caller-owned
    /// clocks, RNG streams or simulation queues. The default temporary directory's
    /// effective absolute path must be UTF-8 for the worker JSON transport;
    /// invalid encoding is rejected before scratch creation or worker startup.
    ///
    /// A retained snapshot ([`Retention`]) needs no new worker when none is
    /// running a machine in its own: that worker goes back to the snapshot in
    /// place and the new machine continues in it, so `executable` starts only
    /// the machine's later boots. [`Self::restored`] reports which of these
    /// happened.
    pub fn from_snapshot(executable: &Path, snapshot: &Snapshot) -> Result<Self, String> {
        Self::from_snapshot_with_scratch(executable, snapshot, None)
    }

    /// Restore with all named boot, disk, capture and restore scratch beneath an
    /// existing caller-owned directory. Each machine owns a unique child, removed
    /// on construction failure or Drop; the caller's root is never removed.
    /// The caller must retain the root until the machine and the snapshots it
    /// retains ([`Retention`]) are dropped, and reclaim its session after
    /// abrupt worker death. No global environment/cwd changes
    /// occur. Snapshots contain no scratch authority: a branch always uses this
    /// new root, independently of source-machine files. The effective absolute
    /// scratch pathname must be UTF-8 for the worker's JSON transport, including
    /// parents supplied by the current directory or default temporary directory.
    /// An encoding error is returned before creating scratch or a worker.
    /// Native restore capability
    /// and execution semantics are those of [`Self::from_snapshot`].
    pub fn from_snapshot_in(
        executable: &Path,
        snapshot: &Snapshot,
        scratch_root: &Path,
    ) -> Result<Self, String> {
        Self::from_snapshot_with_scratch(executable, snapshot, Some(scratch_root))
    }

    fn from_snapshot_with_scratch(
        executable: &Path,
        snapshot: &Snapshot,
        scratch_root: Option<&Path>,
    ) -> Result<Self, String> {
        match &snapshot.body {
            Body::Image(bytes) => Self::from_image(executable, snapshot, bytes, scratch_root),
            Body::Stopped(stopped) => {
                let mut machine =
                    Self::from_inputs(executable, snapshot, &stopped.inputs, scratch_root)?;
                machine.stage()?;
                if let (Some(disk), Some(store)) = (&mut machine.disk, &stopped.store) {
                    disk.store = Some(store.clone());
                }
                machine.restored = Some(Restored::Deferred);
                Ok(machine)
            }
            Body::Retained(retained) => {
                let mut machine =
                    Self::from_inputs(executable, snapshot, &retained.inputs, scratch_root)?;
                match retained
                    .version
                    .rewind(retained.verify, &machine.directory)?
                {
                    // The worker holds the whole boot, disk included, as
                    // after a restoring boot.
                    Some((worker, pages)) => {
                        machine.worker = Some(worker);
                        if let Some(disk) = &mut machine.disk {
                            disk.store = None;
                        }
                        machine.restored = Some(Restored::Rewound { pages });
                    }
                    None => {
                        let directory = tempfile::Builder::new()
                            .prefix("restore-")
                            .tempdir_in(machine.directory.path())
                            .map_err(|e| format!("PV restore image path: {e}"))?;
                        let image = directory.path().join("image");
                        retained.version.export(&image)?;
                        let capture = &retained.inputs.capture;
                        machine.boot_restore(capture.seed, capture.reached, Some(image))?;
                        directory
                            .close()
                            .map_err(|e| format!("PV restore image cleanup: {e}"))?;
                        machine.restored = Some(Restored::Exported);
                    }
                }
                Ok(machine)
            }
        }
    }
    /// A machine for a snapshot whose worker state is not in `inputs`: it has
    /// no worker yet, and its disk, if any, an empty store.
    fn from_inputs(
        executable: &Path,
        snapshot: &Snapshot,
        inputs: &Inputs,
        scratch_root: Option<&Path>,
    ) -> Result<Self, String> {
        let capture = &inputs.capture;
        snapshot.aux.validate(capture)?;
        if capture.layout_version != boot::LAYOUT_VERSION {
            return Err("PV snapshot layout version".into());
        }
        if capture.disk != snapshot.disk.as_ref().map(DiskImage::sectors) {
            return Err("PV snapshot disk base mismatch".into());
        }
        let mut machine = Self::unstaged(
            executable,
            inputs.kernel.clone(),
            inputs.initrd.clone(),
            capture.command_line.clone(),
            snapshot.disk.clone(),
            capture.network,
            capture.memory_bytes,
            scratch_root,
        )?;
        machine.aux = snapshot.aux.clone();
        machine.resume(capture.clone());
        Ok(machine)
    }
    /// Take up the client's part of a captured machine.
    fn resume(&mut self, capture: Capture) {
        self.input = capture.input;
        self.guest = capture.guest;
        self.writes = capture.writes;
        self.reached = capture.reached;
        self.state = capture.state;
        self.shutdown = capture.shutdown;
        self.seed = capture.seed;
        self.pending_boots = capture.pending_boots;
        self.pending_at = capture.pending_at;
        self.breaks = capture.breaks;
    }
    fn from_image(
        executable: &Path,
        snapshot: &Snapshot,
        mut input: &[u8],
        scratch_root: Option<&Path>,
    ) -> Result<Self, String> {
        let capture: Capture =
            protocol::receive(&mut input)?.ok_or("PV snapshot header missing")?;
        snapshot.aux.validate(&capture)?;
        if capture.layout_version != boot::LAYOUT_VERSION {
            return Err("PV snapshot layout version".into());
        }
        boot::RamLayout::native(capture.memory_bytes)?;
        let kernel = read_blob(&mut input)?;
        let initrd = read_blob(&mut input)?;
        let image = read_blob(&mut input)?;
        let overlays = capture.disk.map(|_| read_blob(&mut input)).transpose()?;
        if !input.is_empty() {
            return Err("PV snapshot trailing bytes".into());
        }
        if capture.disk != snapshot.disk.as_ref().map(DiskImage::sectors) {
            return Err("PV snapshot disk base mismatch".into());
        }
        let mut machine = Self::construct_with_memory_in(
            executable,
            kernel.to_vec(),
            initrd.to_vec(),
            capture.command_line.clone(),
            snapshot.disk.clone(),
            capture.network,
            capture.memory_bytes,
            scratch_root,
        )?;
        machine.aux = snapshot.aux.clone();
        if let (Some(disk), Some(overlays)) = (&mut machine.disk, &overlays) {
            if !capture.live {
                let store = disk.store.as_mut().expect("constructed disk");
                store
                    .replace_overlays(overlays)
                    .map_err(|e| format!("PV snapshot disk: {e:?}"))?;
            } else if !overlays.is_empty() {
                return Err("PV live snapshot carries client disk overlays".into());
            }
        }
        if capture.live {
            let image = restore_image(machine.directory.path(), image)?;
            machine.boot_restore(capture.seed, capture.reached, Some(image.path().to_owned()))?;
            remove_restore_image(image)?;
            machine.restored = Some(Restored::Loaded);
        } else {
            machine.restored = Some(Restored::Deferred);
        }
        machine.resume(capture);
        Ok(machine)
    }
}
/// Validate the effective path, not just the supplied spelling: an ASCII relative
/// root or TMPDIR can resolve beneath a non-UTF-8 working directory. No cwd/env
/// mutation or lossy substitution is allowed in this host authority.
fn scratch_directory(root: Option<&Path>) -> Result<TempDir, String> {
    let requested = root
        .map(Path::to_owned)
        .unwrap_or_else(tempfile::env::temp_dir);
    let effective = std::path::absolute(&requested)
        .map_err(|e| format!("PV scratch directory {requested:?}: resolve absolute path: {e}"))?;
    if effective.to_str().is_none() {
        return Err(format!("PV scratch directory {effective:?}: path is not UTF-8; choose a UTF-8 scratch directory including its absolute parent path"));
    }
    let mut builder = tempfile::Builder::new();
    if root.is_some() {
        builder.prefix("seedbox-pv-");
    }
    builder
        .tempdir_in(&effective)
        .map_err(|e| format!("PV boot directory in {effective:?}: {e}"))
}

/// Scratch belongs to this restoration, never to the source capture. Unwind or
/// a failed worker boot drops it; a successful Ready permits explicit removal.
// Once Boot is Ready the worker owns all restored bytes, so scratch is released
// then rather than retained until Drop.
fn restore_image(directory: &Path, bytes: &[u8]) -> Result<tempfile::NamedTempFile, String> {
    let mut image = tempfile::NamedTempFile::new_in(directory)
        .map_err(|e| format!("PV restore image create in {}: {e}", directory.display()))?;
    std::io::Write::write_all(image.as_file_mut(), bytes)
        .map_err(|e| format!("PV restore image write {}: {e}", image.path().display()))?;
    Ok(image)
}
fn remove_restore_image(image: tempfile::NamedTempFile) -> Result<(), String> {
    let path = image.path().to_owned();
    image
        .close()
        .map_err(|e| format!("PV restore image cleanup {}: {e}", path.display()))
}

fn read_blob<'a>(input: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let mut length = [0; 8];
    std::io::Read::read_exact(input, &mut length)
        .map_err(|e| format!("PV snapshot length: {e}"))?;
    let length =
        usize::try_from(u64::from_le_bytes(length)).map_err(|_| "PV snapshot length overflow")?;
    if length > input.len() {
        return Err("PV snapshot truncated".into());
    }
    let blob = &input[..length];
    *input = &input[length..];
    Ok(blob)
}
// The nominal PV progress charge is independent of the caller's execution rate.
// Price each complete work unit once; the guest preserves any partial charge.
fn priced_syscall_ns(factor_permille: u32) -> u64 {
    (1000u128 * u128::from(factor_permille)).div_ceil(1000) as u64
}

impl Machine for PVMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        if self.state == State::Invalid {
            return Err(std::io::Error::other("PV machine invalid"));
        }
        let upto = upto.as_nanos();
        let now = context.clock.now();
        if upto < self.reached
            || upto < now
            || self.pending_at.is_some_and(|at| upto < at)
            || upto == u64::MAX
        {
            return Err(std::io::Error::other(
                "PV execution interval backwards or infinite",
            ));
        }
        let mut invalidates = true;
        let mut crashed_this_run = false;
        // Frames for the boot that runs next; a Crash or Reboot ends the boot
        // they were admitted for.
        let mut frames = Vec::new();
        let result: Result<StopReason, String> = (|| {
            while let Some(input) = context.inbox.front() {
                match &input.payload {
                    seedbox_machine::pv::Request::Crash => {
                        crashed_this_run |= !matches!(self.state, State::Exited | State::Crashed)
                            || self.pending_boots > 0;
                        self.request_crash().map_err(|e| e.to_string())?;
                        frames.clear();
                    }
                    seedbox_machine::pv::Request::Reboot => {
                        self.request_reboot(Timestamp::from_nanos(now.max(self.reached)))
                            .map_err(|e| e.to_string())?;
                        frames.clear();
                    }
                    seedbox_machine::pv::Request::Shutdown => {
                        self.request_shutdown().map_err(|e| e.to_string())?
                    }
                    seedbox_machine::pv::Request::ReceiveFrame { bytes } => {
                        if !seedbox_machine::pv::frame_in_bounds(bytes) {
                            return Err("PV receive frame outside frame bounds".into());
                        }
                        if self.network.is_some()
                            && (!matches!(self.state, State::Exited | State::Crashed)
                                || self.pending_boots > 0)
                        {
                            frames.push(bytes.clone());
                        }
                    }
                    request @ (seedbox_machine::pv::Request::Application { .. }
                    | seedbox_machine::pv::Request::StepRealtime { .. }
                    | seedbox_machine::pv::Request::SimulationReply { .. }
                    | seedbox_machine::pv::Request::PropertyReply { .. }) => {
                        if !matches!(self.state, State::Exited | State::Crashed)
                            || self.pending_boots > 0
                        {
                            self.input.extend(self.guest.request(request)?);
                        }
                    }
                    request @ (seedbox_machine::pv::Request::InstallFault { .. }
                    | seedbox_machine::pv::Request::HealFault { .. }
                    | seedbox_machine::pv::Request::BreakOnBlockRequest { .. }) => {
                        self.device_request(request)?
                    }
                    seedbox_machine::pv::Request::DeclareBreaks { names } => {
                        if !names.windows(2).all(|pair| pair[0] < pair[1])
                            || !names
                                .iter()
                                .all(|name| seedbox_machine::pv::break_name_valid(name.as_bytes()))
                        {
                            return Err("PV invalid declared breakpoints".into());
                        }
                        self.breaks = names.clone();
                    }
                    seedbox_machine::pv::Request::ResumeBreak => {
                        self.released |= self.held;
                        self.held = false;
                    }
                    // The common request is non-exhaustive across crate boundaries.
                    _ => return Err("unsupported machine request".into()),
                }
                context.inbox.take();
            }
            if self.pending_boots > 0 {
                let at = now
                    .max(self.reached)
                    .max(self.pending_at.unwrap_or(self.reached));
                self.initialize(context, at)?;
            }
            if matches!(self.state, State::Exited | State::Crashed) {
                self.reached = now.max(self.reached);
                if upto > self.reached && !crashed_this_run {
                    invalidates = false;
                    return Err("PV exited machine requires a zero-budget run or Reboot".into());
                }
                self.flush_ends(context)?;
                return Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(self.reached),
                    reason: match self.state {
                        State::Crashed => ExitReason::Crashed,
                        _ => ExitReason::Shutdown,
                    },
                });
            }
            let syscall_cost_ns = priced_syscall_ns(context.execution.factor_permille());
            // A held machine admits its inputs at the current time and runs
            // no guest work; time it spends held is a stall to the guest.
            let held = self.held;
            let upto = if held { now.max(self.reached) } else { upto };
            let mut stopped = self.execute(now, upto, syscall_cost_ns, &frames)?;
            if held {
                self.state = State::Blocked(None);
                stopped = StopReason::Blocked {
                    reached: Timestamp::from_nanos(self.reached),
                    deadline: None,
                };
            }
            self.guest
                .observe_ordered(context, std::mem::take(&mut self.writes))?;
            // The hit follows this run's writes, which are no later.
            if let Some(payload) = self.hit.take() {
                context
                    .outbox
                    .emit(seedbox_machine::pv::Output {
                        at: Timestamp::from_nanos(self.reached),
                        payload,
                    })
                    .map_err(|e| format!("PV breakpoint output: {e:?}"))?;
            }
            self.flush_ends(context)?;
            Ok(stopped)
        })();
        result.map_err(|error| {
            if invalidates {
                self.state = State::Invalid;
                drop(self.worker.take());
            }
            std::io::Error::other(error)
        })
    }

    fn execution_evidence(
        &mut self,
    ) -> Result<Option<seedbox_machine::machine::ExecutionEvidence>, Self::Error> {
        self.read_execution().map_err(std::io::Error::other)
    }
    fn state_evidence(&mut self) -> Result<Option<StateEvidence>, Self::Error> {
        if self.state == State::Invalid {
            return Err(std::io::Error::other("PV machine is invalid"));
        }
        if !self
            .aux
            .policy
            .as_ref()
            .is_some_and(|p| p.settings.full_state)
        {
            return Ok(None);
        }
        if self.worker.is_some() {
            let owner = self
                .aux
                .active
                .ok_or_else(|| std::io::Error::other("PV native evidence has no physical owner"))?;
            let store = self.stopped_store().map_err(std::io::Error::other)?;
            return self
                .capture_evidence(owner, EvidenceDisposition::Live, store.as_ref())
                .map(Some)
                .map_err(std::io::Error::other);
        }
        Ok(self.aux.cache.as_ref().map(retained::Cached::evidence))
    }
    fn snapshot(&mut self) -> Result<Snapshot, Self::Error> {
        match self.retention {
            Some(retention) if RETAINS && self.worker.is_some() => self.retain(retention),
            _ => self.capture().map_err(std::io::Error::other),
        }
    }
    fn take_fault_counts(
        &mut self,
        id: u64,
    ) -> Result<Option<seedbox_machine::pv::FaultCounts>, Self::Error> {
        Ok(self.aux.faults.take(id))
    }
}
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod resource_tests;
#[cfg(test)]
mod retention_tests;
#[cfg(any(test, feature = "test-support"))]
mod test_image;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod execution_tests;

#[cfg(test)]
mod console_tests;

#[cfg(test)]
mod program_exit_tests;

#[cfg(test)]
mod progress_tests;

/// Explicit fixture-only construction and stopped evidence for retained scenarios.
/// These helpers are absent unless `test-support` is enabled; they never export
/// native handles, boot manifests or mutable machine state.
// Driver scenarios need backend-local construction and assertions; a reverse
// dev-dependency or general private-state inspection would be broader.
#[cfg(feature = "test-support")]
pub mod test_support;

// Closed persisted record: exhaustive destructuring prevents silently omitting new fields.
impl seedbox_machine::snapshot::Record for Capture {
    const MIN: u64 = <u64 as seedbox_machine::snapshot::Record>::MIN
        + <u32 as seedbox_machine::snapshot::Record>::MIN
        + <String as seedbox_machine::snapshot::Record>::MIN
        + <u64 as seedbox_machine::snapshot::Record>::MIN
        + <u64 as seedbox_machine::snapshot::Record>::MIN
        + <State as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <bool as seedbox_machine::snapshot::Record>::MIN
        + <u64 as seedbox_machine::snapshot::Record>::MIN
        + <Option<u64> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<seedbox_machine::pv::transport::GuestWrite> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<u8> as seedbox_machine::snapshot::Record>::MIN
        + <seedbox_machine::pv::bridge::GuestBridge as seedbox_machine::snapshot::Record>::MIN
        + <Option<u64> as seedbox_machine::snapshot::Record>::MIN
        + <Option<[u8; 6]> as seedbox_machine::snapshot::Record>::MIN
        + <Vec<String> as seedbox_machine::snapshot::Record>::MIN;
    fn encode<W: std::io::Write>(
        &self,
        w: &mut seedbox_machine::snapshot::Writer<'_, W>,
    ) -> seedbox_machine::snapshot::Result<()> {
        let Self {
            memory_bytes,
            layout_version,
            command_line,
            seed,
            reached,
            state,
            live,
            shutdown,
            pending_boots,
            pending_at,
            writes,
            input,
            guest,
            disk,
            network,
            breaks,
        } = self;
        memory_bytes.encode(w)?;
        layout_version.encode(w)?;
        command_line.encode(w)?;
        seed.encode(w)?;
        reached.encode(w)?;
        state.encode(w)?;
        live.encode(w)?;
        shutdown.encode(w)?;
        pending_boots.encode(w)?;
        pending_at.encode(w)?;
        writes.encode(w)?;
        input.encode(w)?;
        guest.encode(w)?;
        disk.encode(w)?;
        network.encode(w)?;
        breaks.encode(w)?;
        Ok(())
    }
    fn decode<R: std::io::Read>(
        r: &mut seedbox_machine::snapshot::Reader<'_, R>,
    ) -> seedbox_machine::snapshot::Result<Self> {
        use seedbox_machine::snapshot::Record;
        let memory_bytes = Record::decode(r)?;
        let layout_version = Record::decode(r)?;
        let command_line = Record::decode(r)?;
        let seed = Record::decode(r)?;
        let reached = Record::decode(r)?;
        let state = Record::decode(r)?;
        let live = Record::decode(r)?;
        let shutdown = Record::decode(r)?;
        let pending_boots = Record::decode(r)?;
        let pending_at = Record::decode(r)?;
        let writes = Record::decode(r)?;
        let input = Record::decode(r)?;
        let guest = seedbox_machine::pv::bridge::GuestBridge::read_snapshot(r, reached, true)?;
        let disk = Record::decode(r)?;
        let network = Record::decode(r)?;
        let breaks = Record::decode(r)?;
        Ok(Self {
            memory_bytes,
            layout_version,
            command_line,
            seed,
            reached,
            state,
            live,
            shutdown,
            pending_boots,
            pending_at,
            writes,
            input,
            guest,
            disk,
            network,
            breaks,
        })
    }
}

#[cfg(test)]
mod disk_tests;
#[cfg(test)]
mod fault_tests;
