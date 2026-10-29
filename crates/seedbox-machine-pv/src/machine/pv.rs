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
//! Guest PV rendezvous define accepted time; host interruptions are invisible.
//! The Driver owns idle advancement. A standard virtio-console port named
//! `seedbox-lifecycle` carries `shutdown\n` to a cooperating guest supervisor.
//! The request itself executes no guest work. Cleanup and guest power-off require
//! later run calls; an uncooperative guest can remain blocked until crashed.
//! After the guest's ready handshake, a guest power-off or permanent halt ends
//! as `Shutdown` and a guest reset request (deliberate reboot or panic restart)
//! as `Crashed`; the worker never resets itself. Terminal requests before the
//! handshake, unknown exits and hypervisor failures are errors.
//! Snapshot bytes own boot inputs and all stopped worker/device state. Native
//! architectural snapshot support currently requires macOS/HVF or x86-64 KVM;
//! Linux ARM64 KVM snapshots are unsupported.
mod boot;
mod client;
mod codec;
mod evidence;
mod execution;
mod factory;
#[cfg(test)]
mod guest;
mod policy;
mod protocol;
mod retained;
mod wire;
mod worker;
use client::Worker;
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
};
use tempfile::TempDir;
pub(crate) use worker::serve as serve_worker;
#[derive(Clone, Copy, Serialize, Deserialize, PartialEq)]
enum State {
    Ready,
    Blocked(Option<u64>),
    Exited,
    Crashed,
    Invalid,
}
#[derive(Serialize, Deserialize)]
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
}
/// An owned capture of internal machine state, including pending resets.
/// It contains no live worker, mapping or hypervisor handles.
#[derive(Clone)]
pub struct Snapshot {
    // The immutable body owns the exact resource/boot/native state; the
    // separately owned disk base closes the capture independently of temp files.
    #[cfg_attr(not(test), allow(dead_code))]
    // Capture bytes are immutable; sharing the owned Vec avoids full-RAM copies
    // when a caller retains multiple restore branches.
    bytes: std::sync::Arc<Vec<u8>>,
    /// The immutable base, shared rather than copied into `bytes`.
    #[cfg_attr(not(test), allow(dead_code))]
    disk: Option<DiskImage>,
    aux: retained::Aux,
}
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
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    command_line: String,
    seed: u64,
    reached: u64,
    state: State,
    worker: Option<Worker>,
    directory: TempDir,
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
}
impl PVMachine {
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
        boot::arguments(&command_line)?;
        let layout = boot::RamLayout::native(memory_bytes)?;
        boot::layout_for(&layout, &kernel, &initrd)?;
        let directory = scratch_directory(scratch_root)?;
        fs::write(directory.path().join("kernel"), &kernel)
            .map_err(|e| format!("PV kernel input: {e}"))?;
        fs::write(directory.path().join("initrd"), &initrd)
            .map_err(|e| format!("PV initrd input: {e}"))?;
        if let Some(image) = disk.as_ref().and_then(DiskImage::image) {
            fs::write(directory.path().join("disk-base"), &image[..])
                .map_err(|e| format!("PV disk base input: {e}"))?;
        }
        Ok(Self {
            memory_bytes: layout.capacity,
            kernel,
            initrd,
            command_line,
            seed: 0,
            reached: 0,
            state: State::Ready,
            worker: None,
            directory,
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
            aux: Default::default(),
        })
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
        let mut worker = Worker::spawn(&self.executable)?;
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
    /// unflushed writes exactly once, then terminate the worker. Ended boots
    /// have no worker, so repeating this never discards twice.
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
        if let Some(mut worker) = self.worker.take() {
            worker.terminate()?;
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
    fn worker(&mut self) -> Result<&mut Worker, String> {
        self.worker.as_mut().ok_or("PV worker unavailable".into())
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
        let (time, stop, writes) = self.worker()?.run(
            Request::Run {
                now,
                upto,
                syscall_cost_ns,
                shutdown,
                lifecycle: input,
                frames: 0,
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
            _ => Err("PV worker returned invalid stop".into()),
        }
    }
    fn capture(&mut self) -> Result<Snapshot, String> {
        if self.state == State::Invalid {
            return Err("cannot snapshot invalid PV machine".into());
        }
        let live = self.worker.is_some();
        let mut image = if live {
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
            let file = fs::File::open(&artifact).map_err(|e| format!("PV snapshot image: {e}"))?;
            let length = file
                .metadata()
                .map_err(|e| format!("PV snapshot size: {e}"))?
                .len();
            Some((directory, file, length))
        } else {
            None
        };
        let mut bytes = Vec::new();
        protocol::send(
            &mut bytes,
            &Capture {
                memory_bytes: self.memory_bytes,
                layout_version: boot::LAYOUT_VERSION,
                command_line: self.command_line.clone(),
                seed: self.seed,
                reached: self.reached,
                state: self.state,
                live,
                shutdown: self.shutdown,
                pending_boots: self.pending_boots,
                pending_at: self.pending_at,
                writes: self.writes.clone(),
                input: self.input.clone(),
                guest: self.guest.clone(),
                disk: self.disk.as_ref().map(|disk| disk.base.sectors()),
                network: self.network,
            },
        )?;
        // A live worker's image already carries its disk's overlays.
        let overlays = match &self.disk {
            Some(Disk {
                store: Some(store), ..
            }) if !live => Some(store.encode_overlays()),
            Some(_) if live => Some(Vec::new()),
            Some(_) => return Err("PV disk has no owner".into()),
            None => None,
        };
        let image_len = image.as_ref().map_or(0, |(_, _, length)| *length);
        let extra = (self.kernel.len() as u64)
            .checked_add(self.initrd.len() as u64)
            .and_then(|v| v.checked_add(image_len))
            .and_then(|v| v.checked_add(overlays.as_ref().map_or(0, |v| v.len()) as u64))
            .and_then(|v| v.checked_add(if overlays.is_some() { 32 } else { 24 }))
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("PV snapshot size overflow")?;
        bytes
            .try_reserve_exact(extra)
            .map_err(|e| format!("PV snapshot allocation: {e}"))?;
        for blob in [&self.kernel[..], &self.initrd[..]] {
            bytes.extend_from_slice(&(blob.len() as u64).to_le_bytes());
            bytes.extend_from_slice(blob);
        }
        bytes.extend_from_slice(&image_len.to_le_bytes());
        if let Some((_, file, length)) = &mut image {
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
        Ok(Snapshot {
            bytes: std::sync::Arc::new(bytes),
            aux: self.aux.clone(),
            disk: self.disk.as_ref().map(|disk| disk.base.clone()),
        })
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
    pub fn from_snapshot(executable: &Path, snapshot: &Snapshot) -> Result<Self, String> {
        Self::from_snapshot_with_scratch(executable, snapshot, None)
    }

    /// Restore with all named boot, disk, capture and restore scratch beneath an
    /// existing caller-owned directory. Each machine owns a unique child, removed
    /// on construction failure or Drop; the caller's root is never removed.
    /// The caller must retain the root until the machine is dropped and reclaim
    /// its session after abrupt worker death. No global environment/cwd changes
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
        let mut input = &snapshot.bytes[..];
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
            capture.command_line,
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
        } else {
            drop(machine.worker.take());
        }
        machine.input = capture.input;
        machine.guest = capture.guest;
        machine.writes = capture.writes;
        machine.reached = capture.reached;
        machine.state = capture.state;
        machine.shutdown = capture.shutdown;
        machine.seed = capture.seed;
        machine.pending_boots = capture.pending_boots;
        machine.pending_at = capture.pending_at;
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
            let stopped = self.execute(now, upto, syscall_cost_ns, &frames)?;
            self.guest
                .observe_ordered(context, std::mem::take(&mut self.writes))?;
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
        self.capture().map_err(std::io::Error::other)
    }
}
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod resource_tests;
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
        + <Option<[u8; 6]> as seedbox_machine::snapshot::Record>::MIN;
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
        })
    }
}

#[cfg(test)]
mod disk_tests;
