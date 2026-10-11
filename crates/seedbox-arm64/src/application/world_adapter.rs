//! Stopped ARM worker primitives for the shared world coordinator.
use super::{
    log_capture::{self, Chunk, Stream},
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
    cell::Cell,
    collections::BTreeMap,
    fs::File,
    io::Write,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{Arc, Mutex},
};

// Retention limits never limit framing, device processing or witness input.
struct Capture<W, I = W> {
    writer: W,
    index: Option<I>,
    remaining: usize,
    offset: u64,
    records: usize,
    record_limit: usize,
}
trait IndexWriter: Write {
    fn truncate(&mut self, length: u64);
}
struct IndexFile {
    file: File,
    path: PathBuf,
}
impl IndexFile {
    fn open(path: PathBuf) -> Option<Self> {
        File::create(&path).ok().map(|file| Self { file, path })
    }
}
impl Write for IndexFile {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.file.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}
impl IndexWriter for IndexFile {
    fn truncate(&mut self, length: u64) {
        if length == 0 || self.file.set_len(length).is_err() {
            // A missing index is explicit unavailable timing. A malformed one
            // would make the retained run unreadable, so remove failed prefixes.
            let _ = std::fs::remove_file(&self.path);
        }
    }
}
#[cfg(test)]
impl IndexWriter for Vec<u8> {
    fn truncate(&mut self, length: u64) {
        self.truncate(length as usize);
    }
}
impl<W: Write, I: IndexWriter> Capture<W, I> {
    fn new(writer: W, mut index: Option<I>, remaining: usize, record_limit: usize) -> Self {
        if let Some(sink) = &mut index {
            if sink.write_all(log_capture::MAGIC).is_err() {
                sink.truncate(0);
                index = None;
            }
        }
        Self {
            writer,
            index,
            remaining,
            offset: 0,
            records: 0,
            record_limit,
        }
    }
    fn write_chunk(
        &mut self,
        bytes: &[u8],
        time_ns: u64,
        incarnation: u32,
        order: u64,
    ) -> std::io::Result<()> {
        let length = bytes.len().min(self.remaining);
        if length == 0 {
            return Ok(());
        }
        self.writer.write_all(&bytes[..length])?;
        let record = [
            time_ns,
            incarnation as u64,
            order,
            self.offset,
            length as u64,
        ];
        self.remaining -= length;
        self.offset += length as u64;
        if self.records < self.record_limit {
            if let Some(index) = &mut self.index {
                let mut encoded = [0u8; 40];
                for (field, value) in encoded.as_chunks_mut::<8>().0.iter_mut().zip(record) {
                    field.copy_from_slice(&value.to_le_bytes());
                }
                // A failed/partial record must never leave a malformed index.
                // File sinks roll back to the last complete prefix below.
                if index.write_all(&encoded).is_err() {
                    index.truncate(8 + 40 * self.records as u64);
                    self.index = None;
                } else {
                    self.records += 1;
                }
            }
        }
        Ok(())
    }
}
struct Captures {
    console: Capture<File, IndexFile>,
    management: Capture<File, IndexFile>,
    order: u64,
}
impl Captures {
    fn append(
        &mut self,
        chunks: &[Chunk],
        console: &[u8],
        management: &[u8],
        incarnation: u32,
    ) -> Result<()> {
        for chunk in chunks {
            let (capture, bytes) = match chunk.stream {
                Stream::Console => (&mut self.console, console),
                Stream::Management => (&mut self.management, management),
            };
            let offset = chunk.offset as usize;
            let end = offset + chunk.length as usize;
            capture
                .write_chunk(&bytes[offset..end], chunk.time_ns, incarnation, self.order)
                .map_err(|e| format!("ARM output capture: {e}"))?;
            self.order = self.order.saturating_add(1);
        }
        Ok(())
    }
}
/// One machine's slice horizons and causal events, in its own order. Each
/// machine hashes only what it did, so the world witness does not depend on
/// which thread ran it or when; the world combines them in machine order.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Transcript {
    #[serde(with = "seedbox_engine_core::checkpoint::sha256")]
    pub horizons: Sha256,
    #[serde(with = "seedbox_engine_core::checkpoint::sha256")]
    pub events: Sha256,
}
impl Transcript {
    fn new() -> Self {
        Self {
            horizons: Sha256::new_with_prefix(b"seedbox-arm64-machine-horizons-v1\0"),
            events: Sha256::new_with_prefix(b"seedbox-arm64-machine-events-v1\0"),
        }
    }
}

/// Process and output resources; contains no world scheduling policy.
pub struct Factory {
    observations: Option<Arc<seedbox_engine_core::observation::Publisher>>,
    executable: PathBuf,
    output: PathBuf,
    names: std::cell::RefCell<BTreeMap<String, usize>>,
    /// Shared by a machine and its crash successor; only the live one writes.
    captures: std::cell::RefCell<Vec<Option<Arc<Mutex<Captures>>>>>,
    console_limit: usize,
    generation: Cell<u64>,
    full_state: bool,
}
impl Factory {
    pub(crate) fn with_observations(
        mut self,
        observations: Option<Arc<seedbox_engine_core::observation::Publisher>>,
    ) -> Self {
        self.observations = observations;
        self
    }
    pub fn new(
        executable: PathBuf,
        output: PathBuf,
        names: &[String],
        console_limit: usize,
        full_state: bool,
    ) -> Result<Self> {
        Self::build(executable, output, names, console_limit, full_state, true)
    }

    pub(crate) fn live(
        executable: PathBuf,
        output: PathBuf,
        names: &[String],
        full_state: bool,
    ) -> Result<Self> {
        Self::build(executable, output, names, 0, full_state, false)
    }

    fn build(
        executable: PathBuf,
        output: PathBuf,
        names: &[String],
        console_limit: usize,
        full_state: bool,
        retain: bool,
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
            captures.push(if retain {
                Some(machine_captures(
                    &output,
                    name,
                    console_limit,
                    log_capture::stream_limit(names.len()),
                )?)
            } else {
                None
            });
        }
        Ok(Self {
            executable,
            observations: None,
            output,
            names: std::cell::RefCell::new(indexes),
            captures: std::cell::RefCell::new(captures),
            console_limit,
            generation: Cell::new(0),
            full_state,
        })
    }

    fn add_spawn(&self, name: &str, index: usize) -> Result<()> {
        let capture = if self.observations.is_some() {
            None
        } else {
            Some(machine_captures(
                &self.output,
                name,
                self.console_limit,
                log_capture::stream_limit(index + 1),
            )?)
        };
        let mut captures = self.captures.borrow_mut();
        if index > captures.len() {
            return Err("spawned ARM machine index differs from factory".into());
        }
        if index == captures.len() {
            captures.push(capture);
        } else {
            captures[index] = capture;
        }
        let mut names = self.names.borrow_mut();
        names.retain(|_, slot| *slot != index);
        names.insert(name.into(), index);
        Ok(())
    }
}

fn machine_captures(
    output: &std::path::Path,
    name: &str,
    console_limit: usize,
    record_limit: usize,
) -> Result<Arc<Mutex<Captures>>> {
    let console = File::create(output.join(format!("{name}.console")))
        .map_err(|e| format!("console output: {e}"))?;
    let management = File::create(output.join(format!("{name}.mgmt")))
        .map_err(|e| format!("management output: {e}"))?;
    Ok(Arc::new(Mutex::new(Captures {
        console: Capture::new(
            console,
            IndexFile::open(output.join(format!("{name}.console.chunks"))),
            console_limit,
            record_limit,
        ),
        management: Capture::new(
            management,
            IndexFile::open(output.join(format!("{name}.mgmt.chunks"))),
            console_limit,
            record_limit,
        ),
        order: 0,
    })))
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Retired {
    pub evidence: Evidence,
    pub incarnation: u32,
    pub booted_at: u64,
}
/// One scheduler-visible record and one owned, stopped physical worker.
pub struct ArmMachine {
    observations: Option<Arc<seedbox_engine_core::observation::Publisher>>,
    log_decoder: seedbox_engine_core::observation::Decoder,
    world: MachineState,
    worker: OwnedWorker,
    physical: StoppedObservation,
    captures: Option<Arc<Mutex<Captures>>>,
    transcript: Transcript,
    index: usize,
    output: PathBuf,
    full_state: bool,
    framer: ManagementFramer,
    pub(crate) crashed: Vec<Value>,
    pub(crate) retired: Option<Retired>,
    snapshot_sequence: u64,
    ram_pages: u64,
    /// The machine's configured CPU speed, which a throttle is a fraction of.
    cpu_speed_hz: u64,
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
            ram_pages: self
                .full_state
                .then(|| self.output.join(format!("{}.pages", self.world.name))),
        })? {
            Answer::Evidence(evidence) => Ok(evidence),
            _ => self.reject("invalid machine evidence response".into()),
        }
    }
    pub(crate) fn transcript(&self) -> &Transcript {
        &self.transcript
    }
    pub(crate) fn exit(&self) -> Option<(u8, bool)> {
        if self.is_down() {
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

/// A physical artifact plus adapter-owned semantic state. Delivery sockets and
/// their sequence numbers remain owned by the destination timeline.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    checkpoint: super::worker::Checkpoint,
    world: MachineState,
    transcript: Transcript,
    framer: ManagementFramer,
    log_decoder: seedbox_engine_core::observation::Decoder,
    crashed: Vec<Value>,
    retired: Option<Retired>,
}
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
    type Snapshot = Snapshot;

    fn restore_boundary(_: &mut Self::Clock) -> Result<()> {
        // The ARM adapter has no host clock state to discard at a fork point.
        Ok(())
    }

    fn boot(
        factory: &Factory,
        name: &str,
        boot: &Boot,
        barrier: bool,
        entropy: [u8; 32],
    ) -> Result<Self> {
        let index = *factory
            .names
            .borrow()
            .get(name)
            .ok_or("unknown ARM machine name")?;
        let captures = factory.captures.borrow()[index].clone();
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
        let cpu_speed_hz = boot
            .progress
            .as_ref()
            .map_or(0, |progress| progress.speed_hz);
        let mut boot = boot.clone();
        boot.seed = u64::from_le_bytes(
            entropy[..8]
                .try_into()
                .map_err(|_| "invalid boot entropy")?,
        );
        if barrier {
            boot.command_line.push_str(" seedbox.barrier=1");
        }
        let ram_pages = boot.memory_bytes.div_ceil(4096);
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
            captures,
            transcript: Transcript::new(),
            index,
            output: factory.output.clone(),
            full_state: factory.full_state,
            framer: ManagementFramer::default(),
            observations: factory.observations.clone(),
            log_decoder: seedbox_engine_core::observation::Decoder::default(),
            crashed: Vec::new(),
            retired: None,
            snapshot_sequence: 0,
            ram_pages,
            cpu_speed_hz,
        })
    }
    fn spawn_boot(
        factory: &Factory,
        base: &Boot,
        spec: &seedbox_engine_core::world::SpawnSpec<'_>,
    ) -> Result<Boot> {
        let bytes = std::fs::read(&base.initrd)
            .map_err(|e| format!("spawn {} initrd {}: {e}", spec.name, base.initrd.display()))?;
        let initrd = seedbox_engine_core::spawn_recipe::spawned_initrd(&bytes, spec)
            .map_err(|e| format!("spawn {}: {e}", spec.name))?;
        let path = factory.output.join(format!(".spawn-{}.initrd", spec.name));
        std::fs::write(&path, initrd)
            .map_err(|e| format!("spawn {} initrd {}: {e}", spec.name, path.display()))?;
        factory.add_spawn(spec.name, spec.index)?;
        let mut boot = base.clone();
        boot.initrd = path;
        boot.mac = spec.mac;
        boot.command_line = base
            .command_line
            .split_whitespace()
            .filter(|part| {
                *part != "seedbox.control=1"
                    && !part.starts_with("seedbox.index=")
                    && !part.starts_with("seedbox.name=")
            })
            .collect::<Vec<_>>()
            .join(" ");
        boot.command_line.push_str(&format!(
            " seedbox.index={} seedbox.name={}",
            spec.index, spec.name
        ));
        if let Some(policy) = &mut boot.timer_policy {
            policy.domain = format!("timer-jitter/{}", spec.name);
        }
        Ok(boot)
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
                    .incarnation()
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
            // The successor continues its predecessor's transcript.
            self.transcript = old.transcript.clone();
            let disk_loss = transfer.disk_digest.as_ref().map(|_| lost_sectors);
            self.crashed = old.crashed.clone();
            self.crashed.push(json!({"incarnation":old.incarnation(),"crashed_at_ns":at,"time_ns":evidence.time_ns,"witness":evidence.witness,"retired_state_digest":evidence.state_digest,"disk_lost_sectors":lost_sectors,"surviving_disk":transfer.disk_digest}));
            self.retired = Some(Retired {
                evidence,
                incarnation: old.incarnation(),
                booted_at: old.booted_at(),
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
    fn seed_progress_jitter(&mut self, seed: u64, domain: &str) -> Result<()> {
        self.ready(Operation::SeedProgressJitter {
            seed: crate::rng::Rng::new(seed, domain).next(),
        })
    }
    fn seed_progress_jitter_branch(
        &mut self,
        seed: u64,
        identity: Option<&str>,
        domain: &str,
    ) -> Result<()> {
        self.ready(Operation::SeedProgressJitter {
            seed: crate::rng::Rng::branch(seed, identity, domain).next(),
        })
    }
    fn supports_speed_reseed(&self) -> bool {
        false
    }
    fn supports_throttle(&self) -> bool {
        true
    }
    fn throttle(&mut self, speed_hz: Option<u64>) -> Result<Option<u64>> {
        let factor_permille = speed_hz.map_or(1000, |speed_hz| {
            seedbox_control::throttle_permille(self.cpu_speed_hz, speed_hz)
        });
        self.ready(Operation::Throttle { factor_permille })?;
        Ok(speed_hz
            .map(|_| seedbox_control::effective_throttle_speed(self.cpu_speed_hz, factor_permille)))
    }
    fn seed_speed(&mut self, _: u64, _: &str) -> Result<()> {
        Err("ARM machine speed reseeding is unsupported".into())
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
    fn checkpoint_boundary(_: &()) -> Result<()> {
        // ARM has no coordinator-owned host clock to quiesce. Snapshot RPCs
        // validate each stopped worker before reading its native state.
        Ok(())
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot> {
        self.snapshot_sequence = self
            .snapshot_sequence
            .checked_add(1)
            .ok_or("ARM checkpoint sequence overflow")?;
        let artifact = self.output.join(format!(
            ".arm-checkpoint-{}-{}-{}",
            std::process::id(),
            self.worker.identity().generation,
            self.snapshot_sequence
        ));
        let artifact =
            std::path::absolute(artifact).map_err(|e| format!("checkpoint path: {e}"))?;
        let Answer::Snapshot(checkpoint) = self.worker.call(Operation::Snapshot { artifact })?
        else {
            return self.reject("invalid ARM checkpoint response".into());
        };
        Ok(Snapshot {
            checkpoint,
            world: self.world.clone(),
            transcript: self.transcript.clone(),
            framer: self.framer.clone(),
            log_decoder: self.log_decoder.clone(),
            crashed: self.crashed.clone(),
            retired: self.retired.clone(),
        })
    }
    fn restore(&mut self, snapshot: &Self::Snapshot) -> Result<u64> {
        if self.name != snapshot.world.name {
            return Err("ARM checkpoint machine name mismatch".into());
        }
        if !matches!(
            self.worker
                .call(Operation::Restore(snapshot.checkpoint.clone()))?,
            Answer::Restored(_)
        ) {
            return self.reject("invalid ARM checkpoint restore response".into());
        }
        self.physical = self
            .worker
            .observation()
            .cloned()
            .ok_or("missing restored ARM observation")?;
        self.world = snapshot.world.clone();
        self.transcript = snapshot.transcript.clone();
        self.framer = snapshot.framer.clone();
        self.log_decoder = snapshot.log_decoder.clone();
        self.crashed = snapshot.crashed.clone();
        self.retired = snapshot.retired.clone();
        // ARM restoration copies its complete RAM mapping; expose the same page
        // accounting unit as the shared coordinator's x86 adapter.
        Ok(self.ram_pages)
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
            log_chunks,
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
            let mut event_hash = self.transcript.events.clone();
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
        let capture_result = self
            .captures
            .as_ref()
            .map(|captures| {
                captures
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .append(
                        &log_chunks,
                        &console,
                        &management,
                        self.worker.identity().incarnation,
                    )
            })
            .transpose();
        if let Err(error) = capture_result {
            return self.reject(error);
        }
        self.transcript
            .horizons
            .update(request.hard_limit.to_le_bytes());
        self.transcript.events = event_hash;
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
        let output_time = time_ns;
        self.log_decoder.commit(
            &mut self.world,
            self.observations.as_deref(),
            output_time,
            &management,
        );
        frames.extend(sent);
        Ok(semantic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_core::{
        spawn_recipe::{entry, find, Recipe},
        world::SpawnSpec,
    };
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn spawned_arm_boot_has_own_recipe_mac_index_and_output() {
        let directory = tempfile::tempdir().unwrap();
        let names = ["controller".into(), "db".into()];
        let factory =
            Factory::new("worker".into(), directory.path().into(), &names, 32, false).unwrap();
        let recipe = Recipe {
            ip: b"10.0.0.2/24".to_vec(),
            hosts: b"10.0.0.1 controller\n10.0.0.2 db\n".to_vec(),
            hostname: b"db".to_vec(),
            user: b"0:0".to_vec(),
            working_dir: b"/".to_vec(),
            max_map_count: 262144,
            wall_clock_epoch: 1_700_000_000,
            instructions_per_progress_call: 50000,
            progress_points_per_block: 1,
            argv: vec![b"/bin/db".to_vec()],
            environment: vec![b"ROLE=old".to_vec()],
            volumes: vec![],
            disk_volumes: vec![],
            configs: 0u32.to_le_bytes().to_vec(),
        };
        let mut archive = Vec::new();
        entry(
            &mut archive,
            "seedbox/launch.bin",
            1,
            0o100400,
            &recipe.encode(),
        );
        entry(&mut archive, "TRAILER!!!", 2, 0, &[]);
        let base_initrd = directory.path().join("base.initrd");
        std::fs::write(&base_initrd, archive).unwrap();
        let base = Boot {
            kernel: "kernel".into(),
            initrd: base_initrd,
            seed: 7,
            memory_bytes: 128 << 20,
            command_line: format!("{} seedbox.control=1", crate::boot::DEFAULT_COMMAND_LINE),
            mac: [2, 0, 0, 0, 0, 2],
            disk_image: None,
            disk_sectors: Some(4),
            disk_fault_model: crate::disk::parse_disk_fault_model(&Value::Null).unwrap(),
            timer_policy: Some(super::super::TimerPolicy {
                seed: 11,
                domain: "timer-jitter/db".into(),
                jitter_ns: 3,
            }),
            progress: Some(super::super::Progress {
                speed_hz: 800_000_000,
                ns: 0,
                jitter_ns: 0,
                seed: 5,
            }),
        };
        let env = BTreeMap::from([("ROLE".into(), "new".into())]);
        let argv = ["/bin/joiner".into()];
        let next = ["controller".into(), "db".into(), "db-2".into()];
        let spec = SpawnSpec {
            name: "db-2",
            index: 2,
            mac: [2, 0, 0, 0, 0, 3],
            address: "10.0.0.3".into(),
            names: &next,
            env: &env,
            argv: Some(&argv),
        };
        let boot = ArmMachine::spawn_boot(&factory, &base, &spec).unwrap();
        assert_eq!(boot.mac, spec.mac);
        assert_eq!(boot.memory_bytes, base.memory_bytes);
        assert_eq!(boot.progress, base.progress);
        assert!(boot
            .command_line
            .ends_with("seedbox.index=2 seedbox.name=db-2"));
        assert!(!boot.command_line.contains("seedbox.control=1"));
        assert_eq!(
            boot.timer_policy.as_ref().unwrap().domain,
            "timer-jitter/db-2"
        );
        let bytes = std::fs::read(&boot.initrd).unwrap();
        let got = Recipe::parse(find(&bytes, "seedbox/launch.bin").unwrap()).unwrap();
        assert_eq!(got.ip, b"10.0.0.3/24");
        assert_eq!(got.hostname, b"db-2");
        assert_eq!(got.environment, [b"ROLE=new".to_vec()]);
        assert_eq!(got.argv, [b"/bin/joiner".to_vec()]);
        assert_eq!(factory.names.borrow().get("db-2"), Some(&2));
        assert!(factory.captures.borrow()[2].is_some());
        assert!(directory.path().join("db-2.console").exists());
        assert!(directory.path().join("db-2.mgmt").exists());
    }

    fn records(bytes: &[u8]) -> Vec<[u64; 5]> {
        assert_eq!(&bytes[..8], log_capture::MAGIC);
        assert_eq!((bytes.len() - 8) % 40, 0);
        bytes[8..]
            .as_chunks::<40>()
            .0
            .iter()
            .map(|record| {
                std::array::from_fn(|i| {
                    u64::from_le_bytes(record[i * 8..i * 8 + 8].try_into().unwrap())
                })
            })
            .collect()
    }
    struct FailingIndex {
        bytes: Rc<RefCell<Vec<u8>>>,
        limit: usize,
    }
    impl Write for FailingIndex {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            let mut output = self.bytes.borrow_mut();
            let length = bytes.len().min(self.limit - output.len());
            if length == 0 {
                return Err(std::io::Error::other("injected index failure"));
            }
            output.extend_from_slice(&bytes[..length]);
            Ok(length)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl IndexWriter for FailingIndex {
        fn truncate(&mut self, length: u64) {
            self.bytes.borrow_mut().truncate(length as usize);
        }
    }
    #[test]
    fn index_limit_and_partial_sink_failure_keep_complete_prefix_without_limiting_raw_output() {
        let bytes = Rc::new(RefCell::new(Vec::new()));
        let mut capture = Capture::new(
            Vec::new(),
            Some(FailingIndex {
                bytes: Rc::clone(&bytes),
                limit: 8 + 40 + 3,
            }),
            100,
            10,
        );
        capture.write_chunk(b"abc", 2, 0, 0).unwrap();
        capture.write_chunk(b"def", 8, 0, 1).unwrap();
        capture.write_chunk(b"ghi", 9, 1, 2).unwrap();
        assert_eq!(capture.writer, b"abcdefghi");
        assert!(capture.index.is_none());
        assert_eq!(records(&bytes.borrow()), [[2, 0, 0, 0, 3]]);
        let mut bounded = Capture::new(Vec::new(), Some(Vec::new()), 100, 1);
        bounded.write_chunk(b"abc", 2, 0, 0).unwrap();
        bounded.write_chunk(b"def", 8, 1, 1).unwrap();
        assert_eq!(bounded.writer, b"abcdef");
        assert_eq!(records(bounded.index.as_ref().unwrap()), [[2, 0, 0, 0, 3]]);
    }
    #[test]
    fn machine_order_and_retained_offsets_continue_across_incarnations() {
        let root =
            std::env::temp_dir().join(format!("seedbox-arm-log-sink-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let stream = |name: &str| {
            Capture::new(
                File::create(root.join(name)).unwrap(),
                IndexFile::open(root.join(format!("{name}.chunks"))),
                5,
                10,
            )
        };
        let mut captures = Captures {
            console: stream("console"),
            management: stream("mgmt"),
            order: 0,
        };
        for (time, incarnation) in [(3, 0), (10, 1)] {
            let chunks = [
                Chunk {
                    stream: Stream::Console,
                    time_ns: time,
                    offset: 0,
                    length: 3,
                },
                Chunk {
                    stream: Stream::Management,
                    time_ns: time,
                    offset: 0,
                    length: 3,
                },
            ];
            captures
                .append(&chunks, b"abc", b"def", incarnation)
                .unwrap();
        }
        assert_eq!(std::fs::read(root.join("console")).unwrap(), b"abcab");
        assert_eq!(std::fs::read(root.join("mgmt")).unwrap(), b"defde");
        assert_eq!(
            records(&std::fs::read(root.join("console.chunks")).unwrap()),
            [[3, 0, 0, 0, 3], [10, 1, 2, 3, 2]]
        );
        assert_eq!(
            records(&std::fs::read(root.join("mgmt.chunks")).unwrap()),
            [[3, 0, 1, 0, 3], [10, 1, 3, 3, 2]]
        );
        drop(captures);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn capture_boundary_does_not_reject_or_retain_overflow() {
        let mut capture = Capture::new(Vec::new(), Some(Vec::new()), 5, 10);
        capture.write_chunk(b"abc", 2, 0, 0).unwrap();
        capture.write_chunk(b"defgh", 8, 1, 1).unwrap();
        capture.write_chunk(b"ignored", 9, 1, 2).unwrap();
        assert_eq!(capture.writer, b"abcde");
        assert_eq!(capture.remaining, 0);
    }
}
