//! Stopped ARM worker primitives for the shared world coordinator.
use super::{
    worker::{
        Answer, Boot, Identity, Operation, OwnedWorker, StoppedObservation, TransferDescriptor,
    },
    Event, Evidence,
};
use crate::{control::ManagementFramer, machine::MachineStop, Result};
use seedbox_engine_core::world::{MachineState, Reseed, SliceRequest, State, Stop, WorldMachine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    fs::File,
    io::Write,
    ops::{Deref, DerefMut},
    path::PathBuf,
    rc::Rc,
};

// Retention limits never limit framing, device processing or witness input.
struct Capture<W> {
    writer: W,
    remaining: usize,
}
impl<W: Write> Capture<W> {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let length = bytes.len().min(self.remaining);
        self.writer.write_all(&bytes[..length])?;
        self.remaining -= length;
        Ok(())
    }
}
struct Captures {
    console: Capture<File>,
    management: Capture<File>,
}
pub(crate) struct Transcript {
    pub horizons: Sha256,
    pub events: Sha256,
}

/// Process and output resources; contains no world scheduling policy.
pub struct Factory {
    executable: PathBuf,
    output: PathBuf,
    names: BTreeMap<String, usize>,
    captures: Vec<Rc<RefCell<Captures>>>,
    generation: Cell<u64>,
    full_state: bool,
    pub(crate) transcript: Rc<RefCell<Transcript>>,
}
impl Factory {
    pub fn new(
        executable: PathBuf,
        output: PathBuf,
        names: &[String],
        console_limit: usize,
        full_state: bool,
    ) -> Result<Self> {
        let mut indexes = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            if name.is_empty()
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || indexes.insert(name.clone(), index).is_some()
            {
                return Err("invalid ARM machine capture names".into());
            }
        }
        std::fs::create_dir_all(&output).map_err(|e| format!("world output: {e}"))?;
        let output =
            std::fs::canonicalize(&output).map_err(|e| format!("world output path: {e}"))?;
        let mut captures = Vec::new();
        for name in names {
            let console = File::create(output.join(format!("{name}.console")))
                .map_err(|e| format!("console output: {e}"))?;
            let management = File::create(output.join(format!("{name}.mgmt")))
                .map_err(|e| format!("management output: {e}"))?;
            captures.push(Rc::new(RefCell::new(Captures {
                console: Capture {
                    writer: console,
                    remaining: console_limit,
                },
                management: Capture {
                    writer: management,
                    remaining: console_limit,
                },
            })));
        }
        Ok(Self {
            executable,
            output,
            names: indexes,
            captures,
            generation: Cell::new(0),
            full_state,
            transcript: Rc::new(RefCell::new(Transcript {
                horizons: Sha256::new_with_prefix(b"seedbox-arm64-world-horizons-v1\0"),
                events: Sha256::new_with_prefix(b"seedbox-arm64-world-events-v1\0"),
            })),
        })
    }
}

pub(crate) struct Retired {
    pub evidence: Evidence,
    pub incarnation: u32,
    pub booted_at: u64,
}
/// One scheduler-visible record and one owned, stopped physical worker.
pub struct ArmMachine {
    world: MachineState,
    worker: OwnedWorker,
    physical: StoppedObservation,
    captures: Rc<RefCell<Captures>>,
    transcript: Rc<RefCell<Transcript>>,
    index: usize,
    output: PathBuf,
    full_state: bool,
    framer: ManagementFramer,
    pub(crate) crashed: Vec<Value>,
    pub(crate) retired: Option<Retired>,
}
impl Deref for ArmMachine {
    type Target = MachineState;
    fn deref(&self) -> &MachineState {
        &self.world
    }
}
impl DerefMut for ArmMachine {
    fn deref_mut(&mut self) -> &mut MachineState {
        &mut self.world
    }
}
impl ArmMachine {
    fn reject<T>(&mut self, error: String) -> Result<T> {
        self.worker.invalidate(&error);
        Err(error)
    }
    fn ready(&mut self, operation: Operation) -> Result<()> {
        if !matches!(self.worker.call(operation)?, Answer::Ready) {
            return self.reject("invalid ARM physical operation response".into());
        }
        self.physical = self
            .worker
            .observation()
            .cloned()
            .ok_or("missing stopped ARM observation")?;
        Ok(())
    }
    pub(crate) fn evidence(&mut self) -> Result<Evidence> {
        if let Some(retired) = &self.retired {
            return Ok(retired.evidence.clone());
        }
        match self.worker.call(Operation::Evidence {
            full_state: self.full_state,
        })? {
            Answer::Evidence(evidence) => Ok(evidence),
            _ => self.reject("invalid machine evidence response".into()),
        }
    }
    pub(crate) fn exit(&self) -> Option<(u8, bool)> {
        if self.down {
            return None;
        }
        let observation = &self.physical;
        observation
            .exit
            .or(observation.powered_off.then_some((255, true)))
    }
    fn physical_time(&self) -> Result<u64> {
        self.worker
            .observation()
            .map(|o| o.time_ns)
            .ok_or("missing stopped ARM observation".into())
    }
}

/// No complete arbitrary ARM CPU/device snapshot has been qualified.
pub enum UnsupportedSnapshot {}
struct TransferGuard(PathBuf);
impl TransferGuard {
    fn cleanup(&mut self) -> Result<()> {
        match std::fs::remove_dir_all(&self.0) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("crash transfer cleanup {}: {e}", self.0.display())),
        }
    }
}
impl Drop for TransferGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl WorldMachine for ArmMachine {
    type Factory = Factory;
    type Boot = Boot;
    type Clock = ();
    type Snapshot = UnsupportedSnapshot;

    fn boot(
        factory: &Factory,
        name: &str,
        boot: &Boot,
        barrier: bool,
        entropy: [u8; 32],
    ) -> Result<Self> {
        let index = *factory.names.get(name).ok_or("unknown ARM machine name")?;
        let generation = factory
            .generation
            .get()
            .checked_add(1)
            .ok_or("worker generation overflow")?;
        factory.generation.set(generation);
        let identity = Identity {
            name: name.into(),
            generation,
            incarnation: 0,
        };
        let mut worker = OwnedWorker::spawn(&factory.executable, identity)?;
        let mut boot = boot.clone();
        boot.seed = u64::from_le_bytes(
            entropy[..8]
                .try_into()
                .map_err(|_| "invalid boot entropy")?,
        );
        if barrier {
            boot.command_line.push_str(" seedbox.barrier=1");
        }
        if !matches!(worker.call(Operation::Boot(boot))?, Answer::Ready) {
            worker.invalidate("invalid ARM boot response");
            return Err("invalid ARM boot response".into());
        }
        let physical = worker
            .observation()
            .cloned()
            .ok_or("missing stopped ARM boot observation")?;
        Ok(Self {
            world: MachineState::new(name.into()),
            worker,
            physical,
            captures: factory.captures[index].clone(),
            transcript: factory.transcript.clone(),
            index,
            output: factory.output.clone(),
            full_state: factory.full_state,
            framer: ManagementFramer::default(),
            crashed: Vec::new(),
            retired: None,
        })
    }
    fn has_pending_irq(&self) -> bool {
        self.physical.pending_irq
    }
    fn next_deadline(&self) -> u64 {
        self.physical.next_deadline
    }
    fn owed_io(&self) -> Option<u64> {
        self.physical.owed_io
    }
    fn has_control(&self) -> bool {
        self.physical.has_control
    }
    fn deliver_control(&mut self, bytes: &[u8]) -> Result<()> {
        self.ready(Operation::Control(bytes.to_vec()))
    }
    fn deliver_serial(&mut self, _: &[u8]) -> Result<()> {
        Err("ARM application serial delivery is unsupported; use virtio console".into())
    }
    fn deliver_frame(&mut self, bytes: &[u8]) -> Result<bool> {
        match self.worker.call(Operation::Frame(bytes.to_vec()))? {
            Answer::Accepted(accepted) => {
                self.physical = self
                    .worker
                    .observation()
                    .cloned()
                    .ok_or("missing stopped ARM observation")?;
                Ok(accepted)
            }
            _ => self.reject("invalid frame response".into()),
        }
    }
    fn pause(&mut self) -> Result<()> {
        // ACKs prove a stopped vCPU. Pausing preserves its allowed completion.
        self.worker.ensure_active()?;
        self.world.pause()
    }
    fn resume(&mut self, at: u64) -> Result<()> {
        self.worker.ensure_active()?;
        // Keep the shared reference commit site: metadata precedes timer expiry.
        self.world.resume(at)?;
        self.ready(Operation::ResumeAt(self.time))
    }
    fn boot_at(&mut self, at: u64) -> Result<()> {
        self.worker.ensure_active()?;
        self.world.boot_at(at)?;
        self.ready(Operation::ResumeAt(at))?;
        self.retired = None;
        Ok(())
    }
    fn succeed(&mut self, old: &mut Self, at: u64, seed: u64) -> Result<Option<u64>> {
        let mut guard = None;
        let mut result: Result<Option<u64>> = (|| {
            self.worker.ensure_active()?;
            old.worker.ensure_active()?;
            let successor = Identity {
                name: self.name.clone(),
                generation: self.worker.identity().generation,
                incarnation: old
                    .incarnation
                    .checked_add(1)
                    .ok_or("incarnation overflow")?,
            };
            let directory = self.output.join(format!(
                ".arm-transfer-{}-{}",
                std::process::id(),
                successor.generation
            ));
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&directory)
                .map_err(|e| format!("crash transfer directory: {e}"))?;
            guard = Some(TransferGuard(directory.clone()));
            let descriptor = TransferDescriptor {
                transaction: successor.generation,
                predecessor: old.worker.identity().clone(),
                successor,
                artifact: directory.join("disk"),
                at,
            };
            let Answer::Crashed {
                evidence,
                lost_sectors,
                transfer,
            } = old.worker.call(Operation::CrashExport {
                run_seed: seed,
                full_state: self.full_state,
                transfer: descriptor.clone(),
            })?
            else {
                return Err("invalid crash export response".into());
            };
            if transfer.descriptor != descriptor {
                return Err("crash export transfer identity mismatch".into());
            }
            if !matches!(self.worker.call(Operation::InstallSuccession(transfer.clone()))?, Answer::Installed { transfer: installed } if installed == transfer)
            {
                return Err("invalid crash install response".into());
            }
            self.physical = self
                .worker
                .observation()
                .cloned()
                .ok_or("missing stopped ARM installation observation")?;
            self.world.succeed(&old.world, at)?;
            let disk_loss = transfer.disk_digest.as_ref().map(|_| lost_sectors);
            self.crashed = old.crashed.clone();
            self.crashed.push(json!({"incarnation":old.incarnation,"crashed_at_ns":at,"time_ns":evidence.time_ns,"witness":evidence.witness,"retired_state_digest":evidence.state_digest,"disk_lost_sectors":lost_sectors,"surviving_disk":transfer.disk_digest}));
            self.retired = Some(Retired {
                evidence,
                incarnation: old.incarnation,
                booted_at: old.booted_at,
            });
            Ok(disk_loss)
        })();
        if let Some(mut guard) = guard {
            if let Err(cleanup) = guard.cleanup() {
                result = Err(match result {
                    Ok(_) => cleanup,
                    Err(error) => format!("{error}; {cleanup}"),
                });
            }
        }
        if let Err(error) = &result {
            self.worker.invalidate(error);
            old.worker.invalidate(error);
        }
        result
    }
    fn park(&mut self) -> Result<()> {
        self.world.check_park(self.owed_io().is_some())?;
        self.ready(Operation::Park)?;
        self.world.commit_park();
        Ok(())
    }
    fn wake(&mut self, at: u64) -> Result<()> {
        self.ready(Operation::ResumeAt(at))?;
        self.world.time = self.physical_time()?;
        self.world.wake();
        Ok(())
    }
    fn release(&mut self, at: u64, entropy: [u32; 8]) -> Result<Reseed> {
        self.worker.ensure_active()?;
        if !self.physical.barrier {
            return Err("ARM time-park entropy reseeding is unsupported".into());
        }
        self.world.begin_release()?;
        self.ready(Operation::Release { at, entropy })?;
        self.world.time = self.physical_time()?;
        Ok(Reseed::Barrier)
    }
    fn abi(&self) -> u64 {
        self.physical.abi
    }
    fn seed_timer(&mut self, seed: u64, domain: &str) -> Result<()> {
        self.ready(Operation::SeedTimer {
            seed,
            domain: domain.into(),
        })
    }
    fn supports_speed_reseed(&self) -> bool {
        false
    }
    fn seed_speed(&mut self, _: u64, _: &str) -> Result<()> {
        Err("ARM machine speed reseeding is unsupported".into())
    }
    fn branches(&self) -> u64 {
        0
    }
    fn exits(&self) -> u64 {
        self.physical.exits
    }
    fn terminate_worker(&mut self) -> Result<()> {
        self.worker.terminate()
    }
    fn exit_witness(&mut self) -> Result<String> {
        Ok(self.evidence()?.witness)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot> {
        Err("ARM arbitrary machine snapshots are unsupported".into())
    }
    fn restore(&mut self, _: &Self::Snapshot) -> Result<u64> {
        Err("ARM arbitrary machine restore is unsupported".into())
    }

    fn advance(
        &mut self,
        _: &mut (),
        request: SliceRequest,
        frames: &mut Vec<Vec<u8>>,
    ) -> Result<Stop> {
        let Answer::Advanced {
            stop,
            time_ns,
            events,
            console,
        } = self.worker.call(Operation::Advance {
            hard_limit: request.hard_limit,
            early_slack: request.early_slack,
            observe_console: request.observe_console,
        })?
        else {
            return self.reject("invalid machine advancement response".into());
        };
        // Stage all parsing and projection before any scheduler/effect/capture mutation.
        let prepared = (|| {
            if time_ns < self.time
                || time_ns > request.hard_limit
                || self.physical_time()? != time_ns
            {
                return Err("machine escaped its world slice".into());
            }
            let physical = self
                .worker
                .observation()
                .ok_or("missing stopped ARM observation")?;
            let terminal = matches!(
                stop,
                MachineStop::PoweredOff | MachineStop::ApplicationExit { .. }
            );
            if physical.powered_off != matches!(stop, MachineStop::PoweredOff)
                || physical.exit.is_some() != matches!(stop, MachineStop::ApplicationExit { .. })
                || physical.barrier != matches!(stop, MachineStop::Barrier)
                || (!matches!(
                    stop,
                    MachineStop::Device { .. }
                        | MachineStop::ApplicationExit { .. }
                        | MachineStop::Barrier
                ) && !events.is_empty())
            {
                return Err("ARM terminal or causal state disagrees with stop".into());
            }
            let mut framer = self.framer.clone();
            let mut control = Vec::new();
            let mut effects = Vec::new();
            let mut sent = Vec::new();
            let mut management = Vec::new();
            let mut event_hash = self.transcript.borrow().events.clone();
            for event in &events {
                let bytes = serde_json::to_vec(event).map_err(|e| e.to_string())?;
                event_hash.update((self.index as u64).to_le_bytes());
                event_hash.update(time_ns.to_le_bytes());
                event_hash.update((bytes.len() as u64).to_le_bytes());
                event_hash.update(bytes);
                match event {
                    Event::Frame(bytes) => sent.push(bytes.clone()),
                    Event::Control(bytes) => {
                        management.extend_from_slice(bytes);
                        control.extend(
                            framer
                                .receive(bytes)?
                                .into_iter()
                                .map(|line| (time_ns, line)),
                        );
                    }
                    Event::Ready => effects.push((0, time_ns, true)),
                    Event::Effect { id, success } => effects.push((*id, time_ns, *success)),
                    Event::Exit { code, setup_error } => {
                        if !terminal || physical.exit != Some((*code, *setup_error)) {
                            return Err("ARM exit effect disagrees with stopped state".into());
                        }
                    }
                    Event::Barrier => {
                        if !physical.barrier {
                            return Err("ARM barrier effect without physical barrier".into());
                        }
                    }
                }
            }
            let observed = if request.observe_console {
                if console.len() > 1 {
                    return Err("ARM console observation passed its first byte".into());
                }
                console.first().map(|byte| (time_ns, *byte))
            } else {
                None
            };
            let (semantic, state) = match stop {
                MachineStop::Horizon { time_ns: stopped } if stopped == time_ns => {
                    (Stop::Reached, State::Running)
                }
                MachineStop::PoweredOff => (Stop::Exited, State::Exited(255)),
                MachineStop::ApplicationExit { code, .. } => (Stop::Exited, State::Exited(code)),
                MachineStop::Barrier => (Stop::Barrier, State::Running),
                MachineStop::Device { time_ns: stopped } if stopped == time_ns => (
                    if sent.is_empty() {
                        Stop::Control
                    } else {
                        Stop::Sent
                    },
                    State::Running,
                ),
                MachineStop::Console { time_ns: stopped }
                    if stopped == time_ns && observed.is_some() =>
                {
                    (Stop::Control, State::Running)
                }
                _ => return Err("ARM stop does not match committed observation".into()),
            };
            Ok((
                framer, control, effects, sent, management, event_hash, observed, semantic, state,
            ))
        })();
        let (framer, control, effects, sent, management, event_hash, observed, semantic, state) =
            match prepared {
                Ok(value) => value,
                Err(error) => return self.reject(error),
            };
        let capture_result: Result<()> = (|| {
            let mut capture = self.captures.borrow_mut();
            capture
                .console
                .write_all(&console)
                .map_err(|e| format!("console: {e}"))?;
            capture
                .management
                .write_all(&management)
                .map_err(|e| format!("management output: {e}"))?;
            Ok(())
        })();
        if let Err(error) = capture_result {
            return self.reject(error);
        }
        let mut transcript = self.transcript.borrow_mut();
        transcript
            .horizons
            .update((self.index as u64).to_le_bytes());
        transcript.horizons.update(request.hard_limit.to_le_bytes());
        transcript.events = event_hash;
        self.framer = framer;
        self.world.time = time_ns;
        self.physical = self
            .worker
            .observation()
            .cloned()
            .ok_or("missing stopped ARM observation")?;
        self.world.state = state;
        self.world.observe_console = request.observe_console;
        if semantic == Stop::Barrier {
            self.world.commit_park();
        }
        self.world.fault_effects = effects;
        self.world.control_requests = control;
        self.world.committed_console = observed;
        frames.extend(sent);
        Ok(semantic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_boundary_does_not_reject_or_retain_overflow() {
        let mut capture = Capture {
            writer: Vec::new(),
            remaining: 5,
        };
        capture.write_all(b"abc").unwrap();
        capture.write_all(b"defgh").unwrap();
        capture.write_all(b"ignored").unwrap();
        assert_eq!(capture.writer, b"abcde");
        assert_eq!(capture.remaining, 0);
    }
}
