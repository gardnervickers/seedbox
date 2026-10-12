//! QEMU TCG reference implementation of the machine execution contract.
//!
//! One RV64 CPU runs with one simulation nanosecond per instruction. A small
//! QEMU patch caps its native instruction budget and returns when that budget
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

use super::{ExitReason, Machine, StopReason, Timestamp};
use crate::clock::Clock;
use seedbox_engine_core::rng::Rng;
use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
    boot_seed: u64,
    boot_rng: Rng,
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
    clock: Arc<dyn Clock + Send + Sync>,
    manifest: Manifest,
    reached: Timestamp,
    state: State,
    process: Option<Process>,
    executable: PathBuf,
    boot_seed: u64,
    boot_rng: Rng,
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
    /// Start stopped, using the project's patched qemu-system-riscv64 binary.
    pub fn new(
        qemu: &Path,
        manifest: Manifest,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, Error> {
        let reached = Timestamp::from_nanos(clock.now());
        if reached.as_nanos() > i64::MAX as u64 {
            return Err(Error("clock exceeds QEMU's time range".into()));
        }
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
        let mut process = Process::start(qemu, &manifest, None, 0)?;
        process.advance(reached)?;
        Ok(Self {
            clock,
            manifest,
            reached,
            state: State::Ready,
            process: Some(process),
            executable: qemu.to_owned(),
            boot_seed: 0,
            boot_rng: Rng::new(0, "tcg/reboot-entropy"),
            console: Vec::new(),
        })
    }

    /// Reconstruct a snapshot without the source machine or its temporary files.
    /// The clock belongs to the caller's environment and is supplied again;
    /// restoring machine state does not rewind that clock.
    pub fn from_snapshot(
        qemu: &Path,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, Error> {
        let process = snapshot
            .image
            .as_deref()
            .map(|image| Process::start(qemu, &snapshot.manifest, Some(image), snapshot.boot_seed))
            .transpose()?;
        Ok(Self {
            clock,
            manifest: snapshot.manifest.clone(),
            reached: snapshot.reached,
            state: snapshot.state,
            process,
            executable: qemu.to_owned(),
            boot_seed: snapshot.boot_seed,
            boot_rng: snapshot.boot_rng.clone(),
            console: Vec::new(),
        })
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
        let now = Timestamp::from_nanos(self.clock.now());
        if upto < self.reached || upto < now || upto.as_nanos() > i64::MAX as u64 {
            return Err(Error(
                "execution boundary is backwards or exceeds QEMU's time range".into(),
            ));
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
            self.console.extend(process.console()?);
            process.terminate()?;
        }
        self.state = State::Exited(ExitReason::Crashed);
        Ok(())
    }

    /// This diskless reset destroys RAM/devices. Each reboot draws a fresh
    /// deterministic QEMU entropy seed from a snapshot-owned boot stream.
    fn reboot(&mut self, at: Timestamp) -> Result<(), Error> {
        if self.state == State::Invalid {
            return Err(Error("cannot reboot an invalid machine".into()));
        }
        if at.as_nanos() != self.clock.now() || at < self.reached || at.as_nanos() > i64::MAX as u64
        {
            return Err(Error(
                "reboot requires the current simulation barrier within QEMU's time range".into(),
            ));
        }
        self.state = State::Invalid;
        if let Some(mut process) = self.process.take() {
            self.console.extend(process.console()?);
            process.terminate()?;
        }
        let boot_seed = self.boot_rng.next();
        let mut process = Process::start(&self.executable, &self.manifest, None, boot_seed)?;
        process.advance(at)?;
        self.process = Some(process);
        self.boot_seed = boot_seed;
        self.reached = at;
        self.state = State::Ready;
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
            boot_seed: self.boot_seed,
            boot_rng: self.boot_rng.clone(),
        })
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod lifecycle_tests;

#[cfg(test)]
mod elapsed_tests;
