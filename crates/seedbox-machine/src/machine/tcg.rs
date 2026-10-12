//! QEMU TCG reference implementation of the machine execution contract.
//!
//! One RV64 CPU runs with one simulation nanosecond per instruction. A small
//! QEMU patch caps its native instruction budget and returns when that budget
//! expires, the CPU becomes idle, or the guest shuts down. Idle time advances
//! only through an explicit [`TcgMachine::wake`] from the simulation driver.
//! There are no per-instruction host round trips or guest-specific yield calls.
//!
//! Both flat programs and Linux boot inputs are supported. Snapshots own the
//! boot inputs, RAM, CPU/device state, virtual timers, and lifecycle position.
//! Restore requires the caller to provide a compatible patched QEMU executable.
//! This first implementation has no attached disk or network and supports one
//! CPU only. It uses software emulation on macOS and Linux.

use super::{ExitReason, Machine, StopReason, Timestamp};
use serde_json::Value;
use std::fmt;
use std::path::Path;

mod qemu;
use qemu::Process;

/// Owned boot inputs, independent of their original files.
#[derive(Clone, Debug)]
pub enum Boot {
    /// Flat machine code loaded at the start of RAM, with no firmware.
    Program(Vec<u8>),
    /// A Linux kernel, initramfs, and OpenSBI firmware for QEMU's RV64 virt board.
    Linux {
        kernel: Vec<u8>,
        initramfs: Vec<u8>,
        firmware: Vec<u8>,
        command_line: String,
    },
}

/// Complete initial configuration of this diskless, single-CPU machine.
#[derive(Clone, Debug)]
pub struct Manifest {
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
    /// Start stopped, using the project's patched qemu-system-riscv64 binary.
    pub fn new(qemu: &Path, manifest: Manifest) -> Result<Self, Error> {
        if manifest.memory_mib == 0 {
            return Err(Error("guest memory must be nonzero".into()));
        }
        if let Boot::Program(program) = &manifest.boot {
            if program.is_empty()
                || program.len() as u64 > u64::from(manifest.memory_mib) * 1024 * 1024
            {
                return Err(Error(
                    "program must be nonempty and fit in guest RAM".into(),
                ));
            }
        }
        let process = Process::start(qemu, &manifest, None)?;
        Ok(Self {
            manifest,
            reached: Timestamp::from_nanos(0),
            state: State::Ready,
            process: Some(process),
        })
    }

    /// Reconstruct a snapshot without the source machine or its temporary files.
    pub fn from_snapshot(qemu: &Path, snapshot: &Snapshot) -> Result<Self, Error> {
        let process = snapshot
            .image
            .as_deref()
            .map(|image| Process::start(qemu, &snapshot.manifest, Some(image)))
            .transpose()?;
        Ok(Self {
            manifest: snapshot.manifest.clone(),
            reached: snapshot.reached,
            state: snapshot.state,
            process,
        })
    }

    /// Advance a blocked machine to a coordinator-selected wakeup time.
    ///
    /// Due virtual timers are delivered without executing guest instructions.
    /// The caller must not skip the returned timer deadline. Advancing to an
    /// earlier external event is allowed, but time alone need not make the CPU
    /// runnable: `run` may report `Blocked` again.
    pub fn wake(&mut self, at: Timestamp) -> Result<(), Error> {
        let State::Blocked { deadline } = self.state else {
            return Err(Error("wake requires a blocked machine".into()));
        };
        if at < self.reached
            || deadline.is_some_and(|deadline| at > deadline)
            || at.as_nanos() > i64::MAX as u64
        {
            return Err(Error(
                "wake must not move backwards or skip a timer deadline".into(),
            ));
        }
        if let Err(error) = self.process()?.advance(at) {
            self.state = State::Invalid;
            drop(self.process.take());
            return Err(error);
        }
        self.reached = at;
        self.state = State::Ready;
        Ok(())
    }

    /// Read the serial output produced by this instance since creation/restore.
    /// Already emitted output is external observation, not replayed by restore.
    pub fn console(&self) -> Result<Vec<u8>, Error> {
        let process = self
            .process
            .as_ref()
            .ok_or_else(|| Error("machine has no execution process".into()))?;
        process.console()
    }

    fn process(&mut self) -> Result<&mut Process, Error> {
        self.process
            .as_mut()
            .ok_or_else(|| Error("machine has no execution process".into()))
    }

    fn execute(&mut self, upto: Timestamp) -> Result<(), Error> {
        let start = self.reached;
        let result = self.process()?.execute(start, upto)?;
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
            _ => return Err(Error(format!("unexpected QEMU stop: {result}"))),
        };
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

    fn run(&mut self, _: &mut (), upto: Timestamp) -> Result<StopReason, Error> {
        if matches!(self.state, State::Exited(_) | State::Invalid) {
            return Err(Error("machine is exited or invalid".into()));
        }
        if upto < self.reached || upto.as_nanos() > i64::MAX as u64 {
            return Err(Error(
                "execution boundary is backwards or exceeds QEMU's time range".into(),
            ));
        }
        if self.reached < upto && self.state == State::Ready {
            if let Err(error) = self.execute(upto) {
                self.state = State::Invalid;
                // A transport failure may leave QEMU executing. Reap it before
                // returning so the failed machine cannot keep producing effects.
                drop(self.process.take());
                return Err(error);
            }
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

    /// Destroy the process. This diskless machine retains no guest RAM after
    /// crash; previously captured snapshots remain independent.
    fn crash(&mut self) -> Result<(), Error> {
        if matches!(self.state, State::Exited(_) | State::Invalid) {
            return Err(Error(
                "crash requires a valid machine that has not exited".into(),
            ));
        }
        self.state = State::Invalid;
        if let Some(mut process) = self.process.take() {
            process.terminate()?;
        }
        self.state = State::Exited(ExitReason::Crashed);
        Ok(())
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
        })
    }
}

#[cfg(test)]
mod tests;
