//! QEMU TCG reference implementation of the machine execution contract.
//!
//! One RV64 CPU prices each instruction with the supplied execution factor. A small
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
//! Both flat programs and Linux boot inputs are supported. Snapshots own the
//! boot inputs, RAM, CPU/device state, virtual timers, and lifecycle position.
//! Restore requires the caller to provide a compatible patched QEMU executable.
//! This first implementation has no attached disk or network and supports one
//! CPU only. It uses software emulation on macOS and Linux.
//!
//! Shutdown is offered as `shutdown\n` on the dedicated standard virtio-serial
//! port named `seedbox-lifecycle`. The device retains one request through early
//! boot and snapshots, with synchronous queue kicks at guest MMIO instructions.
//! Guest userspace owns cleanup and Linux poweroff policy; a boot input without
//! a cooperating receiver can ignore the request and continue running.
//!
//! Guest terminal requests stop QEMU at the requesting instruction. Power-off
//! (including Linux halt, which powers off on this board) is `Shutdown`. QEMU
//! runs with `-action reboot=shutdown`, so a guest reset request, including a
//! kernel panic restart under `panic=-1`, stops instead of resetting in place
//! and is reported as `Crashed`; only an accepted Reboot request starts a new
//! boot. Host-caused stops and unknown causes are errors.

mod factory;
use super::{ExitReason, Machine, RunContext, StopReason, Timestamp};
pub use factory::Factory;
use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod qemu;
use qemu::Process;

/// Owned boot inputs, independent of their original files.
#[derive(Clone, Debug)]
pub(crate) enum Boot {
    /// Flat machine code loaded at the start of RAM, with no firmware.
    #[cfg(test)]
    Program(Arc<[u8]>),
    /// A Linux kernel, initramfs, and OpenSBI firmware for QEMU's RV64 virt board.
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

/// Complete initial configuration of this diskless, single-CPU machine.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub boot: Boot,
    pub memory_mib: u32,
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
    writes: Vec<crate::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: crate::pv::bridge::GuestBridge,
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
    writes: Vec<crate::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: crate::pv::bridge::GuestBridge,
    console: Vec<u8>,
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

impl TcgMachine {
    /// Prepare a stopped machine using the project's patched QEMU binary.
    /// Boot initialization occurs in the first run, consuming eight bytes from
    /// its context RNG in little-endian order without executing instructions.
    /// Each pending reboot consumes another eight bytes on the next run.
    pub(crate) fn new(
        qemu: &Path,
        firmware: Vec<u8>,
        bootstrap: &crate::application::GuestBootstrap,
        definition: crate::definition::MachineDefinition,
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
        // Linux 6.12 RISC-V COMMAND_LINE_SIZE includes its trailing NUL.
        if bootstrap.kernel.command_line.contains('\0')
            || bootstrap.kernel.command_line.len() >= 1024
        {
            return Err(Error(
                "TCG kernel command line must contain no NUL and fit in 1023 bytes".into(),
            ));
        }
        // TCG needs no application instrumentation. Packaging borrows the
        // immutable prepared input; PV may derive its own private preparation.
        let initramfs = crate::application::boot_archive(&definition.application, bootstrap)
            .map_err(|error| Error(format!("TCG application preparation: {error}")))?;
        Self::from_manifest(
            qemu,
            Manifest {
                memory_mib: (memory / MIB) as u32,
                boot: Boot::Linux {
                    kernel: Arc::from(bootstrap.kernel.image.as_slice()),
                    initramfs: initramfs.into(),
                    firmware: firmware.into(),
                    command_line: bootstrap.kernel.command_line.clone(),
                    epoch_seconds: bootstrap.epoch_seconds,
                },
            },
        )
    }

    #[cfg(test)]
    pub(crate) fn new_raw(qemu: &Path, manifest: Manifest) -> Result<Self, Error> {
        Self::from_manifest(qemu, manifest)
    }

    fn from_manifest(qemu: &Path, manifest: Manifest) -> Result<Self, Error> {
        if manifest.memory_mib == 0 {
            return Err(Error("guest memory must be nonzero".into()));
        }
        match &manifest.boot {
            #[cfg(test)]
            Boot::Program(_) => {}
            Boot::Linux { initramfs, .. } => {
                let memory = u64::from(manifest.memory_mib) * 1024 * 1024;
                if initramfs.len() as u64 > memory {
                    return Err(Error(format!(
                        "TCG initramfs ({} bytes) must fit in guest RAM ({memory} bytes)",
                        initramfs.len()
                    )));
                }
            }
        }
        #[cfg(test)]
        if let Boot::Program(program) = &manifest.boot {
            if program.is_empty()
                || program.len() as u64 > u64::from(manifest.memory_mib) * 1024 * 1024
            {
                return Err(Error(
                    "program must be nonempty and fit in guest RAM".into(),
                ));
            }
        }
        Ok(Self {
            manifest,
            reached: Timestamp::from_nanos(0),
            state: State::Ready,
            process: None,
            executable: qemu.to_owned(),
            boot_seed: 0,
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
    pub fn from_snapshot(qemu: &Path, snapshot: &Snapshot) -> Result<Self, Error> {
        let process = snapshot
            .image
            .as_deref()
            .map(|image| Process::start(qemu, &snapshot.manifest, Some(image), snapshot.boot_seed))
            .transpose()?;
        Ok(Self {
            manifest: snapshot.manifest.clone(),
            reached: snapshot.reached,
            state: snapshot.state,
            process,
            executable: qemu.to_owned(),
            boot_seed: snapshot.boot_seed,
            pending_boots: snapshot.pending_boots,
            pending_at: snapshot.pending_at,
            shutdown_requested: snapshot.shutdown_requested,
            writes: snapshot.writes.clone(),
            input: snapshot.input.clone(),
            guest: snapshot.guest.clone(),
            console: Vec::new(),
        })
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
    /// Destroy the process. This diskless machine retains no guest RAM after
    /// crash; previously captured snapshots remain independent.
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
        if let Some(mut process) = self.process.take() {
            self.console.extend(process.console()?);
            process.terminate()?;
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
    /// Defer a diskless reset until the next run. Each recorded reset draws its
    /// own boot seed from that run's caller-owned RNG before any guest work.
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
        self.pending_boots = self
            .pending_boots
            .checked_add(1)
            .ok_or_else(|| Error("pending reboot count exhausted".into()))?;
        self.pending_at = Some(at);
        self.shutdown_requested = false;
        self.guest.reset();
        self.input.clear();
        self.writes = Default::default();
        Ok(())
    }
    fn initialize(&mut self, context: &mut RunContext<'_, ()>, at: Timestamp) -> Result<(), Error> {
        self.state = State::Invalid;
        if let Some(mut process) = self.process.take() {
            self.console.extend(process.console()?);
            process.terminate()?;
        }
        for _ in 0..self.pending_boots {
            let mut bytes = [0; 8];
            context.rng.fill_bytes(&mut bytes);
            self.boot_seed = u64::from_le_bytes(bytes);
            let mut process =
                Process::start(&self.executable, &self.manifest, None, self.boot_seed)?;
            process.advance(at)?;
            if let Some(mut previous) = self.process.replace(process) {
                previous.terminate()?;
            }
        }
        self.reached = at;
        self.pending_boots = 0;
        self.pending_at = None;
        self.state = State::Ready;
        Ok(())
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

    fn process(&mut self) -> Result<&mut Process, Error> {
        self.process
            .as_mut()
            .ok_or_else(|| Error("machine has no execution process".into()))
    }

    fn execute(
        &mut self,
        upto: Timestamp,
        execution: crate::execution::ExecutionConfig,
    ) -> Result<(), Error> {
        let start = self.reached;
        let result = self.process()?.execute(start, upto, execution)?;
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
        let writes = self.process()?.drain_guest()?;
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
        Ok(())
    }
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
        let accepted = (|| -> Result<(), Error> {
            while let Some(input) = context.inbox.front() {
                match &input.payload {
                    crate::pv::Request::Crash => {
                        crashed_this_run |=
                            !matches!(self.state, State::Exited(_)) || self.pending_boots > 0;
                        self.request_crash()?;
                    }
                    crate::pv::Request::Reboot => self.request_reboot(now.max(self.reached))?,
                    crate::pv::Request::Shutdown => self.request_shutdown()?,
                    request @ (crate::pv::Request::Application { .. }
                    | crate::pv::Request::StepRealtime { .. }
                    | crate::pv::Request::SimulationReply { .. }) => {
                        if !matches!(self.state, State::Exited(_)) || self.pending_boots > 0 {
                            self.input
                                .extend(self.guest.request(request).map_err(Error)?);
                        }
                    }
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

    fn snapshot(&mut self) -> Result<Snapshot, Error> {
        if self.state == State::Invalid {
            return Err(Error("cannot snapshot an invalid machine".into()));
        }
        let image = self.process.as_mut().map(Process::snapshot).transpose()?;
        Ok(Snapshot {
            manifest: self.manifest.clone(),
            reached: self.reached,
            state: self.state,
            image,
            boot_seed: self.boot_seed,
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
