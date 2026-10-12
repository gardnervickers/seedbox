//! Native arm64 PV machine, isolated in an owned worker process.
//!
//! The diskless, single-vCPU fixture personality supports PV Linux Images and
//! initramfs inputs on macOS/HVF and Linux/arm64 KVM. Compute between guest PV
//! rendezvous costs no simulated time. Idle returns to the driver, which owns
//! shared time and timer wakeups. Host interruptions are never time boundaries.
//! Crash destroys RAM and the worker; no disk is attached. Snapshot bytes own
//! boot inputs and complete stopped CPU, RAM, device, timer and lifecycle state.

use super::{ExitReason, Machine, StopReason, Timestamp};
use crate::clock::Clock;
use seedbox_arm64::{
    application::worker::{
        write_message, Answer, Boot, Checkpoint, Identity, Operation, OwnedWorker,
    },
    boot::{BootSpec, DEFAULT_COMMAND_LINE},
    disk::DiskFaultModel,
    machine::MachineStop,
};
use seedbox_engine_core::rng::Rng;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
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
    seed: u64,
    reached: u64,
    state: State,
    checkpoint: Option<Checkpoint>,
    boot_rng: Rng,
}

/// A real PV guest implementing the shared machine contract.
/// Only construction is backend-specific; execution and lifecycle use Machine.
pub struct PVMachine {
    clock: Arc<dyn Clock + Send + Sync>,
    kernel: Vec<u8>,
    initrd: Vec<u8>,
    seed: u64,
    reached: u64,
    state: State,
    worker: Option<OwnedWorker>,
    directory: TempDir,
    console: Vec<u8>,
    executable: PathBuf,
    boot_rng: Rng,
}

impl PVMachine {
    /// Create a stopped diskless ARM PV guest with owned boot inputs.
    /// `executable` is a compatible seedbox-arm64 or integrated seedbox worker;
    /// on macOS it must carry the Hypervisor entitlement. The clock is supplied
    /// by Driver's construction factory. Guest RAM is 128 MiB.
    pub fn new(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        seed: u64,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        Self::construct(executable, kernel, initrd, seed, clock).map_err(std::io::Error::other)
    }

    fn construct(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        seed: u64,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, String> {
        let reached = clock.now();
        if reached == u64::MAX {
            return Err("PV initial timestamp exceeds finite time range".into());
        }
        BootSpec::for_application(&kernel, &initrd, seed, 128 << 20, DEFAULT_COMMAND_LINE)?
            .validate()?;
        let directory = tempfile::tempdir().map_err(|e| format!("PV boot directory: {e}"))?;
        let kernel_path = directory.path().join("kernel");
        let initrd_path = directory.path().join("initrd");
        fs::write(&kernel_path, &kernel).map_err(|e| format!("PV kernel input: {e}"))?;
        fs::write(&initrd_path, &initrd).map_err(|e| format!("PV initrd input: {e}"))?;
        let mut worker = OwnedWorker::spawn(
            executable,
            Identity {
                name: "pv".into(),
                generation: 0,
                incarnation: 0,
            },
        )?;
        worker.call(Operation::BootFixture(Boot {
            kernel: kernel_path,
            initrd: initrd_path,
            seed,
            memory_bytes: 128 << 20,
            command_line: DEFAULT_COMMAND_LINE.into(),
            mac: [0; 6],
            disk_image: None,
            disk_sectors: None,
            disk_fault_model: DiskFaultModel {
                sector_bytes: 512,
                survival_probability: 0.0,
            },
            timer_policy: None,
        }))?;
        if reached != 0 {
            worker.call(Operation::ResumeAt(reached))?;
        }
        Ok(Self {
            clock,
            kernel,
            initrd,
            seed,
            reached,
            state: State::Ready,
            worker: Some(worker),
            directory,
            console: Vec::new(),
            executable: executable.to_owned(),
            boot_rng: Rng::new(seed, "pv/reboot-entropy"),
        })
    }

    fn worker(&mut self) -> Result<&mut OwnedWorker, String> {
        self.worker
            .as_mut()
            .ok_or_else(|| "PV worker is unavailable".into())
    }

    fn execute(&mut self, upto: u64) -> Result<StopReason, String> {
        let now = self.clock.now();
        if now > self.reached {
            // Adopt elapsed simulation time without running the vCPU. The
            // worker coalesces expired timers at this time, rather than running
            // guest work or publishing effects at their former deadlines.
            self.worker()?.call(Operation::ResumeAt(now))?;
            self.reached = now;
            if matches!(self.state, State::Blocked(Some(deadline)) if deadline <= now) {
                self.state = State::Ready;
            }
        }
        if let State::Blocked(deadline) = self.state {
            return Ok(StopReason::Blocked {
                reached: Timestamp::from_nanos(self.reached),
                deadline: deadline.map(Timestamp::from_nanos),
            });
        }
        if self.reached == upto {
            return Ok(StopReason::BoundaryReached);
        }
        let answer = self
            .worker()?
            .call(Operation::AdvanceUntilIdle { hard_limit: upto })?;
        let Answer::Advanced {
            stop,
            time_ns,
            console,
            events,
            ..
        } = answer
        else {
            return Err("PV worker returned an unexpected execution answer".into());
        };
        if !events.is_empty() || time_ns < self.reached || time_ns > upto {
            return Err("PV fixture returned unsupported effects or crossed its boundary".into());
        }
        self.console.extend(console);
        self.reached = time_ns;
        match stop {
            MachineStop::Horizon { .. } if time_ns == upto => {
                self.state = State::Ready;
                Ok(StopReason::BoundaryReached)
            }
            MachineStop::Blocked { deadline, .. } => {
                self.state = State::Blocked(deadline);
                Ok(StopReason::Blocked {
                    reached: Timestamp::from_nanos(time_ns),
                    deadline: deadline.map(Timestamp::from_nanos),
                })
            }
            MachineStop::PoweredOff => {
                self.state = State::Exited;
                drop(self.worker.take());
                Ok(StopReason::Exited {
                    reached: Timestamp::from_nanos(time_ns),
                    reason: ExitReason::Shutdown,
                })
            }
            _ => Err("PV fixture returned an unsupported stop".into()),
        }
    }

    fn capture(&mut self) -> Result<Vec<u8>, String> {
        if self.state == State::Invalid {
            return Err("cannot snapshot an invalid PV machine".into());
        }
        let (checkpoint, image) = if self.worker.is_some() {
            let file = tempfile::Builder::new()
                .prefix("snapshot-")
                .tempfile_in(self.directory.path())
                .map_err(|e| format!("PV snapshot path: {e}"))?;
            let artifact = file.path().to_owned();
            // The protocol creates the artifact exclusively.
            drop(file);
            let answer = self.worker()?.call(Operation::Snapshot {
                artifact: artifact.clone(),
            })?;
            let checkpoint = match answer {
                Answer::Snapshot(checkpoint) => checkpoint,
                Answer::SnapshotFailed(error) => return Err(error),
                _ => return Err("unexpected PV snapshot answer".into()),
            };
            let image = fs::read(&artifact).map_err(|e| format!("PV snapshot image: {e}"));
            let _ = fs::remove_file(artifact);
            (Some(checkpoint), Some(image?))
        } else {
            (None, None)
        };
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &Capture {
                seed: self.seed,
                reached: self.reached,
                state: self.state,
                checkpoint,
                boot_rng: self.boot_rng.clone(),
            },
        )?;
        for blob in [
            &self.kernel[..],
            &self.initrd[..],
            image.as_deref().unwrap_or_default(),
        ] {
            bytes
                .write_all(&(blob.len() as u64).to_le_bytes())
                .map_err(|e| format!("PV snapshot length: {e}"))?;
            bytes
                .write_all(blob)
                .map_err(|e| format!("PV snapshot data: {e}"))?;
        }
        Ok(bytes)
    }

    #[cfg(test)]
    fn from_snapshot(
        executable: &Path,
        bytes: &[u8],
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, String> {
        let mut input = bytes;
        let capture: Capture = seedbox_arm64::application::worker::read_message(&mut input)?
            .ok_or("missing PV snapshot header")?;
        let kernel = read_blob(&mut input)?;
        let initrd = read_blob(&mut input)?;
        let image = read_blob(&mut input)?;
        if !input.is_empty() {
            return Err("trailing PV snapshot bytes".into());
        }
        let mut machine = Self::construct(executable, kernel, initrd, capture.seed, clock)?;
        if let Some(mut checkpoint) = capture.checkpoint {
            checkpoint.artifact = machine.directory.path().join("restore");
            fs::write(&checkpoint.artifact, image).map_err(|e| format!("PV restore image: {e}"))?;
            machine.worker()?.call(Operation::Restore(checkpoint))?;
        } else {
            drop(machine.worker.take());
        }
        machine.reached = capture.reached;
        machine.state = capture.state;
        machine.boot_rng = capture.boot_rng;
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
        return Err("truncated PV snapshot data".into());
    }
    let bytes = input[..length].to_vec();
    *input = &input[length..];
    Ok(bytes)
}

impl Machine for PVMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Vec<u8>;

    fn run(&mut self, _: &mut (), upto: Timestamp) -> Result<StopReason, std::io::Error> {
        if matches!(self.state, State::Exited | State::Crashed | State::Invalid) {
            return Err(std::io::Error::other("PV machine is exited or invalid"));
        }
        let upto = upto.as_nanos();
        if upto < self.reached || upto < self.clock.now() || upto == u64::MAX {
            return Err(std::io::Error::other(
                "PV execution boundary is backwards or exceeds finite time range",
            ));
        }
        match self.execute(upto) {
            Ok(reason) => Ok(reason),
            Err(error) => {
                self.state = State::Invalid;
                drop(self.worker.take());
                Err(std::io::Error::other(error))
            }
        }
    }

    fn crash(&mut self) -> Result<(), std::io::Error> {
        if matches!(self.state, State::Exited | State::Crashed | State::Invalid) {
            return Err(std::io::Error::other(
                "PV crash requires a valid live machine",
            ));
        }
        self.state = State::Invalid;
        if let Some(mut worker) = self.worker.take() {
            worker.terminate().map_err(std::io::Error::other)?;
        }
        self.state = State::Crashed;
        Ok(())
    }

    fn snapshot(&mut self) -> Result<Vec<u8>, std::io::Error> {
        self.capture().map_err(std::io::Error::other)
    }

    /// This diskless reset discards RAM/devices. Boot entropy is a fresh draw
    /// from the machine's deterministic stream, which snapshots preserve.
    fn reboot(&mut self, at: Timestamp) -> Result<(), std::io::Error> {
        let at = at.as_nanos();
        if self.state == State::Invalid {
            return Err(std::io::Error::other("cannot reboot an invalid PV machine"));
        }
        if at != self.clock.now() || at < self.reached || at == u64::MAX {
            return Err(std::io::Error::other(
                "PV reboot requires the current finite simulation barrier",
            ));
        }
        self.state = State::Invalid;
        if let Some(mut worker) = self.worker.take() {
            worker.terminate().map_err(std::io::Error::other)?;
        }
        let seed = self.boot_rng.next();
        let mut replacement = Self::construct(
            &self.executable,
            self.kernel.clone(),
            self.initrd.clone(),
            seed,
            self.clock.clone(),
        )
        .map_err(std::io::Error::other)?;
        replacement.boot_rng = self.boot_rng.clone();
        replacement.console = std::mem::take(&mut self.console);
        *self = replacement;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;
