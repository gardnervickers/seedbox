//! Native PV guest in a private, isolated worker process.
//!
//! A diskless Linux kernel and initramfs for the host architecture run on
//! macOS/HVF or Linux/KVM (ARM64), or Linux/KVM (x86-64).
//! Guest PV rendezvous define accepted time; host interruptions are invisible.
//! The Driver owns idle advancement. A standard virtio-console port named
//! `seedbox-lifecycle` carries `shutdown\n` to a cooperating guest supervisor.
//! The request itself executes no guest work. Cleanup and guest power-off require
//! later run calls; an uncooperative guest can remain blocked until crashed.
//! Snapshot bytes own boot inputs and all stopped worker/device state. Native
//! architectural snapshot support currently requires macOS/HVF or x86-64 KVM;
//! Linux ARM64 KVM snapshots are unsupported.
mod boot;
mod client;
mod factory;
#[cfg(test)]
#[path = "../../tests/support/pv_guest.rs"]
mod guest;
mod protocol;
use super::{ExitReason, Machine, RunContext, StopReason, Timestamp};
use client::Worker;
pub use factory::Factory;
use protocol::{Request, Response, Stop};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use tempfile::TempDir;
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
    command_line: String,
    seed: u64,
    reached: u64,
    state: State,
    live: bool,
    shutdown: bool,
    pending_boots: u64,
    pending_at: Option<u64>,
    writes: Vec<crate::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: crate::pv::bridge::GuestBridge,
}
/// An owned capture of internal machine state, including pending resets.
/// It contains no live worker, mapping or hypervisor handles.
#[derive(Clone)]
pub struct Snapshot {
    // The public contract captures state; reconstruction currently remains
    // private to backend validation rather than adding a second constructor.
    #[cfg_attr(not(test), allow(dead_code))]
    bytes: Vec<u8>,
}

/// A single native PV guest. Construction is its only backend-specific API.
pub struct PVMachine {
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
    writes: Vec<crate::pv::transport::GuestWrite>,
    input: Vec<u8>,
    guest: crate::pv::bridge::GuestBridge,
}
#[cfg(all(test, target_arch = "aarch64"))]
const TEST_COMMAND_LINE: &str = "console=ttyAMA0 earlycon=pl011,0x09000000 rdinit=/init nokaslr nosmp maxcpus=1 random.trust_cpu=off random.trust_bootloader=on mitigations=off panic=-1 loglevel=4";
#[cfg(all(test, target_arch = "x86_64"))]
const TEST_COMMAND_LINE: &str = "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4";

impl PVMachine {
    /// Prepare a stopped machine from a workload and separately supplied boot assets.
    /// Unsupported resource requests are rejected before execution.
    /// The first run initializes boot using its clock and eight RNG bytes in
    /// little-endian order. Each pending reboot draws another eight bytes.
    #[cfg(test)]
    pub(crate) fn new(
        executable: &Path,
        bootstrap: &crate::application::GuestBootstrap,
        definition: crate::definition::MachineDefinition,
    ) -> Result<Self, std::io::Error> {
        if definition.resources.memory_bytes != boot::RAM_BYTES as u64 {
            return Err(std::io::Error::other(
                "PV currently supports exactly 128 MiB RAM",
            ));
        }
        // Preparation may derive a private application copy before packaging;
        // the original prepared workload is never mutated by the backend.
        let initrd = crate::application::boot_archive(&definition.application, bootstrap)
            .map_err(std::io::Error::other)?;
        Self::construct(
            executable,
            bootstrap.kernel.image.clone(),
            initrd,
            bootstrap.kernel.command_line.clone(),
        )
        .map_err(std::io::Error::other)
    }
    #[cfg(test)]
    pub(super) fn from_boot_inputs(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
    ) -> Result<Self, std::io::Error> {
        Self::construct(executable, kernel, initrd, TEST_COMMAND_LINE.into())
            .map_err(std::io::Error::other)
    }
    fn construct(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        command_line: String,
    ) -> Result<Self, String> {
        boot::arguments(&command_line)?;
        boot::layout(&kernel, &initrd)?;
        let directory = tempfile::tempdir().map_err(|e| format!("PV boot directory: {e}"))?;
        fs::write(directory.path().join("kernel"), &kernel)
            .map_err(|e| format!("PV kernel input: {e}"))?;
        fs::write(directory.path().join("initrd"), &initrd)
            .map_err(|e| format!("PV initrd input: {e}"))?;
        Ok(Self {
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
        })
    }
    fn boot(&mut self, seed: u64, now: u64) -> Result<(), String> {
        let mut worker = Worker::spawn(&self.executable)?;
        if !matches!(
            worker.call(Request::Boot {
                kernel: self.directory.path().join("kernel"),
                initrd: self.directory.path().join("initrd"),
                seed,
                now,
                command_line: self.command_line.clone(),
            })?,
            Response::Ready
        ) {
            return Err("PV worker did not acknowledge boot".into());
        }
        self.worker = Some(worker);
        self.seed = seed;
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
        self.shutdown = false;
        if let Some(mut worker) = self.worker.take() {
            worker.terminate().map_err(std::io::Error::other)?;
        }
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
        self.pending_boots = self
            .pending_boots
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("PV pending reboot count exhausted"))?;
        self.pending_at = Some(at);
        self.shutdown = false;
        self.guest.reset();
        self.input.clear();
        self.writes.clear();
        Ok(())
    }
    fn initialize(&mut self, context: &mut RunContext<'_, ()>, at: u64) -> Result<(), String> {
        self.state = State::Invalid;
        if let Some(mut worker) = self.worker.take() {
            worker.terminate()?;
        }
        for _ in 0..self.pending_boots {
            let mut bytes = [0; 8];
            context.rng.fill_bytes(&mut bytes);
            if let Some(mut worker) = self.worker.take() {
                worker.terminate()?;
            }
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
    fn execute(&mut self, now: u64, upto: u64, syscall_cost_ns: u64) -> Result<StopReason, String> {
        let now = now.max(self.reached);
        let shutdown = false;
        let input = self.input.clone();
        let Response::Stopped { time, stop, writes } = self.worker()?.call(Request::Run {
            now,
            upto,
            syscall_cost_ns,
            shutdown,
            lifecycle: input,
        })?
        else {
            return Err("PV worker returned unexpected run response".into());
        };
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
            if let crate::pv::transport::GuestWrite::Console(write) = write {
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
                self.state = State::Exited;
                drop(self.worker.take());
                Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(time),
                    reason: ExitReason::Shutdown,
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
        let image = if live {
            let file = tempfile::Builder::new()
                .prefix("snapshot-")
                .tempfile_in(self.directory.path())
                .map_err(|e| format!("PV snapshot path: {e}"))?;
            let artifact = file.path().to_owned();
            drop(file);
            if !matches!(
                self.worker()?.call(Request::Capture(artifact.clone()))?,
                Response::Captured
            ) {
                return Err("PV unexpected snapshot response".into());
            }
            let bytes = fs::read(&artifact).map_err(|e| format!("PV snapshot image: {e}"));
            let _ = fs::remove_file(artifact);
            bytes?
        } else {
            Vec::new()
        };
        let mut bytes = Vec::new();
        protocol::send(
            &mut bytes,
            &Capture {
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
            },
        )?;
        for blob in [&self.kernel[..], &self.initrd[..], &image[..]] {
            bytes
                .write_all(&(blob.len() as u64).to_le_bytes())
                .and_then(|_| bytes.write_all(blob))
                .map_err(|e| format!("PV snapshot bytes: {e}"))?;
        }
        Ok(Snapshot { bytes })
    }
    #[cfg(test)]
    pub(super) fn from_snapshot(executable: &Path, snapshot: &Snapshot) -> Result<Self, String> {
        let mut input = &snapshot.bytes[..];
        let capture: Capture =
            protocol::receive(&mut input)?.ok_or("PV snapshot header missing")?;
        let kernel = read_blob(&mut input)?;
        let initrd = read_blob(&mut input)?;
        let image = read_blob(&mut input)?;
        if !input.is_empty() {
            return Err("PV snapshot trailing bytes".into());
        }
        let mut machine = Self::construct(executable, kernel, initrd, capture.command_line)?;
        if capture.live {
            machine.boot(capture.seed, capture.reached)?;
            let path = machine.directory.path().join("restore");
            fs::write(&path, image).map_err(|e| format!("PV restore image: {e}"))?;
            if !matches!(
                machine.worker()?.call(Request::Restore(path))?,
                Response::Ready
            ) {
                return Err("PV unexpected restore response".into());
            }
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
#[cfg(test)]
fn read_blob(input: &mut &[u8]) -> Result<Vec<u8>, String> {
    let mut length = [0; 8];
    std::io::Read::read_exact(input, &mut length)
        .map_err(|e| format!("PV snapshot length: {e}"))?;
    let length =
        usize::try_from(u64::from_le_bytes(length)).map_err(|_| "PV snapshot length overflow")?;
    if length > input.len() {
        return Err("PV snapshot truncated".into());
    }
    let blob = input[..length].to_vec();
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
        let result: Result<StopReason, String> = (|| {
            while let Some(input) = context.inbox.front() {
                match &input.payload {
                    crate::pv::Request::Crash => {
                        crashed_this_run |= !matches!(self.state, State::Exited | State::Crashed)
                            || self.pending_boots > 0;
                        self.request_crash().map_err(|e| e.to_string())?;
                    }
                    crate::pv::Request::Reboot => self
                        .request_reboot(Timestamp::from_nanos(now.max(self.reached)))
                        .map_err(|e| e.to_string())?,
                    crate::pv::Request::Shutdown => {
                        self.request_shutdown().map_err(|e| e.to_string())?
                    }
                    request @ (crate::pv::Request::Application { .. }
                    | crate::pv::Request::SimulationReply { .. }) => {
                        if !matches!(self.state, State::Exited | State::Crashed)
                            || self.pending_boots > 0
                        {
                            self.input.extend(self.guest.request(request)?);
                        }
                    }
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
                return Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(self.reached),
                    reason: match self.state {
                        State::Crashed => ExitReason::Crashed,
                        _ => ExitReason::Shutdown,
                    },
                });
            }
            let syscall_cost_ns = priced_syscall_ns(context.execution.factor_permille());
            let stopped = self.execute(now, upto, syscall_cost_ns)?;
            self.guest
                .observe_ordered(context, std::mem::take(&mut self.writes))?;
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

    fn snapshot(&mut self) -> Result<Snapshot, Self::Error> {
        self.capture().map_err(std::io::Error::other)
    }
}
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
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
