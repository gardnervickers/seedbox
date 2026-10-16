//! Owned single-machine worker. HVF permits one VM per process; the world
//! coordinator serializes these workers through a bounded, numbered protocol.
use super::{Event, Evidence};
use crate::{
    backend::Backend,
    boot::BootSpec,
    disk::{Disk, DiskFaultModel},
    machine::{Machine, MachineStop},
    Result,
};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

const MAX_MESSAGE: usize = 16 << 20;
pub const CONTRACT: &str = "arm64-machine-v2";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boot {
    pub kernel: PathBuf,
    pub initrd: PathBuf,
    pub seed: u64,
    pub memory_bytes: u64,
    pub command_line: String,
    pub mac: [u8; 6],
    pub disk_image: Option<PathBuf>,
    pub disk_sectors: Option<u64>,
    pub disk_fault_model: DiskFaultModel,
    #[serde(default)]
    pub timer_policy: Option<super::TimerPolicy>,
    #[serde(default)]
    pub progress: Option<super::Progress>,
}
impl Boot {
    fn machine<B: Backend>(&self, disk: Option<Disk>) -> Result<Machine<B>> {
        let kernel = std::fs::read(&self.kernel)
            .map_err(|e| format!("kernel {}: {e}", self.kernel.display()))?;
        let initrd = std::fs::read(&self.initrd)
            .map_err(|e| format!("initrd {}: {e}", self.initrd.display()))?;
        let machine = Machine::for_application(
            BootSpec::for_application(
                &kernel,
                &initrd,
                self.seed,
                self.memory_bytes,
                &self.command_line,
            )?,
            self.mac,
            disk,
        )?;
        let machine = match &self.progress {
            Some(progress) => machine.with_progress(progress)?,
            None => machine,
        };
        match &self.timer_policy {
            Some(policy) => machine.with_timer_policy(policy.clone()),
            None => Ok(machine),
        }
    }
    fn disk(&self) -> Result<Option<Disk>> {
        if self.disk_image.is_none() && self.disk_sectors.is_none() {
            return Ok(None);
        }
        let image = self
            .disk_image
            .as_ref()
            .map(|path| std::fs::read(path).map_err(|e| format!("disk {}: {e}", path.display())))
            .transpose()?;
        Disk::new(image, self.disk_sectors)
            .map(|disk| Some(disk.with_fault_model(self.disk_fault_model)))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub name: String,
    pub generation: u64,
    pub incarnation: u32,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferDescriptor {
    pub transaction: u64,
    pub predecessor: Identity,
    pub successor: Identity,
    pub artifact: PathBuf,
    pub at: u64,
}
impl TransferDescriptor {
    pub fn validate(&self) -> Result<()> {
        if self.predecessor.name != self.successor.name
            || self.predecessor.generation == self.successor.generation
            || self.predecessor.incarnation.checked_add(1) != Some(self.successor.incarnation)
            || self.at == u64::MAX
            || !self.artifact.is_absolute()
        {
            return Err("invalid succession transaction".into());
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportedTransfer {
    pub descriptor: TransferDescriptor,
    pub disk_digest: Option<String>,
    pub artifact_sha256: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoppedObservation {
    pub time_ns: u64,
    pub pending_irq: bool,
    pub next_deadline: u64,
    pub owed_io: Option<u64>,
    pub abi: u64,
    pub exits: u64,
    pub powered_off: bool,
    pub exit: Option<(u8, bool)>,
    pub barrier: bool,
    pub has_control: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    pub artifact: PathBuf,
    pub identity: Identity,
    pub observation: StoppedObservation,
    pub console_offset: usize,
    pub sha256: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointHeader {
    identity: Identity,
    observation: StoppedObservation,
    console_offset: usize,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum Operation {
    Boot(Boot),
    Advance {
        hard_limit: u64,
        early_slack: u64,
        observe_console: bool,
    },
    Frame(Vec<u8>),
    Control(Vec<u8>),
    Release {
        at: u64,
        entropy: [u32; 8],
    },
    ResumeAt(u64),
    Park,
    SeedTimer {
        seed: u64,
        domain: String,
    },
    SeedProgressJitter {
        seed: u64,
    },
    Throttle {
        factor_permille: u64,
    },
    DiskErrors(Option<seedbox_engine_core::virtio_block::InjectedErrors>),
    HoldBlock(Option<seedbox_engine_core::virtio_block::Hold>),
    ReseedGuest {
        entropy: [u32; 8],
    },
    Evidence {
        full_state: bool,
        #[serde(default)]
        ram_pages: Option<PathBuf>,
    },
    CrashExport {
        run_seed: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch_identity: Option<String>,
        full_state: bool,
        transfer: TransferDescriptor,
    },
    InstallSuccession(ExportedTransfer),
    Snapshot {
        artifact: PathBuf,
    },
    Restore(Checkpoint),
    Quit,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub sequence: u64,
    pub expected: Identity,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum Answer {
    Ready,
    Snapshot(Checkpoint),
    Restored(Checkpoint),
    Advanced {
        stop: MachineStop,
        time_ns: u64,
        events: Vec<Event>,
        console: Vec<u8>,
        log_chunks: Vec<super::log_capture::Chunk>,
    },
    Accepted(bool),
    DiskErrors(Option<seedbox_engine_core::virtio_block::ErrorCounts>),
    Evidence(Evidence),
    Crashed {
        evidence: Evidence,
        lost_sectors: u64,
        transfer: ExportedTransfer,
    },
    Installed {
        transfer: ExportedTransfer,
    },
    Error(String),
    Bye,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub sequence: u64,
    pub before: Option<Identity>,
    pub after: Option<Identity>,
    pub observation: Option<StoppedObservation>,
    pub answer: Answer,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferHeader {
    descriptor: TransferDescriptor,
    disk_digest: Option<String>,
}

pub fn read_message<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0u8; 4];
    loop {
        match input.read(&mut length[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => break,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("worker header: {e}")),
            _ => unreachable!(),
        }
    }
    input
        .read_exact(&mut length[1..])
        .map_err(|e| format!("worker header: {e}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    let mut bytes = vec![0; length];
    input
        .read_exact(&mut bytes)
        .map_err(|e| format!("worker body: {e}"))?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| format!("worker message: {e}"))
}
pub fn write_message<T: Serialize>(output: &mut impl Write, value: &T) -> Result<()> {
    let bytes = serde_json::to_vec(value).map_err(|e| format!("worker message: {e}"))?;
    if bytes.is_empty() || bytes.len() > MAX_MESSAGE {
        return Err("worker message exceeds protocol limit".into());
    }
    output
        .write_all(&(bytes.len() as u32).to_le_bytes())
        .and_then(|_| output.write_all(&bytes))
        .and_then(|_| output.flush())
        .map_err(|e| format!("worker output: {e}"))
}
fn artifact_hash(file: &mut std::fs::File) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::{Seek, SeekFrom};
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("transfer seek: {e}"))?;
    let mut hash = Sha256::new();
    let mut bytes = [0; 64 << 10];
    loop {
        let count = file
            .read(&mut bytes)
            .map_err(|e| format!("transfer hash: {e}"))?;
        if count == 0 {
            break;
        }
        hash.update(&bytes[..count]);
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("transfer seek: {e}"))?;
    Ok(hex::encode(hash.finalize()))
}

/// All requests complete stopped. Identity and sequence are checked before any
/// physical action; errors terminate this worker rather than replaying mutation.
pub fn serve<B: Backend>(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut sequence = 0u64;
    let mut identity: Option<Identity> = None;
    let mut machine: Option<Machine<B>> = None;
    let mut console_offset = 0usize;
    let mut install_available = false;
    let mut boot_inputs: Option<Boot> = None;
    while let Some(request) = read_message::<Request>(input)? {
        if request.sequence != sequence {
            return Err("worker request sequence mismatch".into());
        }
        let before = identity.clone();
        match &identity {
            Some(current) if current != &request.expected => {
                return Err("worker request identity mismatch".into())
            }
            None if !matches!(request.operation, Operation::Boot(_)) => {
                return Err("unbound worker requires boot".into())
            }
            _ => (),
        }
        if request.expected.name.is_empty() {
            return Err("empty worker machine identity".into());
        }
        let terminal = matches!(
            request.operation,
            Operation::Quit | Operation::CrashExport { .. }
        );
        let result = (|| -> Result<Answer> {
            if let Operation::Boot(boot) = &request.operation {
                if identity.is_some() || request.expected.incarnation != 0 {
                    return Err("worker already bound or invalid initial incarnation".into());
                }
                machine = Some(boot.machine(boot.disk()?)?);
                boot_inputs = Some(boot.clone());
                identity = Some(request.expected.clone());
                install_available = true;
                return Ok(Answer::Ready);
            }
            if matches!(request.operation, Operation::Quit) {
                machine = None;
                return Ok(Answer::Bye);
            }
            if let Operation::Restore(saved) = &request.operation {
                if saved.identity.name != request.expected.name || !saved.artifact.is_absolute() {
                    return Err("checkpoint identity/path mismatch".into());
                }
                let mut file = std::fs::File::open(&saved.artifact)
                    .map_err(|e| format!("open checkpoint: {e}"))?;
                if artifact_hash(&mut file)? != saved.sha256 {
                    return Err("checkpoint artifact hash mismatch".into());
                }
                let header = read_message::<CheckpointHeader>(&mut file)?
                    .ok_or("missing checkpoint header")?;
                if header.identity != saved.identity
                    || header.observation != saved.observation
                    || header.console_offset != saved.console_offset
                {
                    return Err("checkpoint header binding mismatch".into());
                }
                let boot = boot_inputs
                    .as_ref()
                    .ok_or("missing checkpoint boot inputs")?;
                // HVF allows one VM per process and requires architectural restore
                // into a never-entered replacement. Failure terminates this worker.
                drop(machine.take());
                let restored = boot
                    .machine::<B>(boot.disk()?)?
                    .restore_checkpoint(&mut file)?;
                if restored.stopped_observation() != saved.observation
                    || saved.console_offset > restored.console().len()
                {
                    return Err("checkpoint restored observation mismatch".into());
                }
                machine = Some(restored);
                identity = Some(Identity {
                    incarnation: saved.identity.incarnation,
                    ..request.expected.clone()
                });
                console_offset = saved.console_offset;
                install_available = false;
                return Ok(Answer::Restored(saved.clone()));
            }
            let vm = machine.as_mut().ok_or("worker has no live machine")?;
            match request.operation {
                Operation::Advance {
                    hard_limit,
                    early_slack,
                    observe_console,
                } => {
                    install_available = false;
                    let stop = vm.run_slice(hard_limit, early_slack, observe_console)?;
                    let console = vm.console()[console_offset..].to_vec();
                    console_offset = vm.console().len();
                    Ok(Answer::Advanced {
                        stop,
                        time_ns: vm.time_ns(),
                        events: vm.take_events(),
                        console,
                        log_chunks: vm.take_log_chunks(),
                    })
                }
                Operation::Frame(bytes) => {
                    install_available = false;
                    Ok(Answer::Accepted(vm.receive_frame(&bytes)?))
                }
                Operation::Control(bytes) => {
                    install_available = false;
                    vm.receive_control(&bytes)?;
                    Ok(Answer::Ready)
                }
                Operation::Release { at, entropy } => {
                    install_available = false;
                    if !vm.stopped_observation().barrier {
                        return Err("ARM time-fork release/reseed is unsupported".into());
                    }
                    vm.resume_at(at)?;
                    vm.release_barrier(entropy)?;
                    Ok(Answer::Ready)
                }
                Operation::ResumeAt(at) => {
                    install_available = false;
                    vm.resume_at(at)?;
                    Ok(Answer::Ready)
                }
                Operation::Park => {
                    install_available = false;
                    vm.park()?;
                    Ok(Answer::Ready)
                }
                Operation::SeedTimer { seed, domain } => {
                    vm.seed_timer(seed, &domain)?;
                    Ok(Answer::Ready)
                }
                Operation::SeedProgressJitter { seed } => {
                    vm.seed_progress_jitter(seed)?;
                    Ok(Answer::Ready)
                }
                Operation::Throttle { factor_permille } => {
                    vm.throttle(factor_permille)?;
                    Ok(Answer::Ready)
                }
                Operation::DiskErrors(errors) => Ok(Answer::DiskErrors(vm.disk_errors(errors)?)),
                Operation::HoldBlock(hold) => {
                    vm.hold_block(hold)?;
                    Ok(Answer::Ready)
                }
                Operation::ReseedGuest { entropy } => {
                    vm.reseed_guest(entropy)?;
                    Ok(Answer::Ready)
                }
                Operation::Evidence {
                    full_state,
                    ram_pages,
                } => {
                    let evidence = vm.application_evidence(full_state)?;
                    if let Some(path) = ram_pages {
                        let mut output = std::fs::File::create(path).map_err(|e| e.to_string())?;
                        vm.write_application_page_hashes(&mut output)?;
                    }
                    Ok(Answer::Evidence(evidence))
                }
                Operation::CrashExport {
                    run_seed,
                    branch_identity,
                    full_state,
                    transfer,
                } => {
                    transfer.validate()?;
                    if transfer.predecessor != request.expected || transfer.at < vm.time_ns() {
                        return Err("crash transaction predecessor/time mismatch".into());
                    }
                    let evidence = vm.application_evidence(full_state)?;
                    let (disk, lost_sectors) = vm
                        .crashed_disk(
                            run_seed,
                            branch_identity.as_deref(),
                            &request.expected.name,
                            request.expected.incarnation,
                        )
                        .map(|(d, n)| (Some(d), n))
                        .unwrap_or((None, 0));
                    let disk_digest = disk.as_ref().map(Disk::digest);
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .open(&transfer.artifact)
                        .map_err(|e| {
                            format!("crash transfer {}: {e}", transfer.artifact.display())
                        })?;
                    write_message(
                        &mut file,
                        &TransferHeader {
                            descriptor: transfer.clone(),
                            disk_digest: disk_digest.clone(),
                        },
                    )?;
                    if let Some(disk) = disk {
                        disk.write_crash_transfer(&mut file)?;
                    }
                    file.flush()
                        .map_err(|e| format!("crash transfer flush: {e}"))?;
                    let artifact_sha256 = artifact_hash(&mut file)?;
                    machine = None;
                    Ok(Answer::Crashed {
                        evidence,
                        lost_sectors,
                        transfer: ExportedTransfer {
                            descriptor: transfer,
                            disk_digest,
                            artifact_sha256,
                        },
                    })
                }
                Operation::InstallSuccession(transfer) => {
                    transfer.descriptor.validate()?;
                    let next = &transfer.descriptor.successor;
                    if !install_available
                        || request.expected.incarnation != 0
                        || next.name != request.expected.name
                        || next.generation != request.expected.generation
                    {
                        return Err("successor is not a fresh uninstalled worker".into());
                    }
                    let mut file = std::fs::File::open(&transfer.descriptor.artifact)
                        .map_err(|e| format!("successor transfer: {e}"))?;
                    if artifact_hash(&mut file)? != transfer.artifact_sha256 {
                        return Err("successor artifact hash mismatch".into());
                    }
                    let header = read_message::<TransferHeader>(&mut file)?
                        .ok_or("missing successor transfer header")?;
                    if header.descriptor != transfer.descriptor
                        || header.disk_digest != transfer.disk_digest
                    {
                        return Err("successor transaction binding mismatch".into());
                    }
                    vm.install_crash_disk(&mut file, transfer.disk_digest.as_deref())?;
                    vm.resume_at(transfer.descriptor.at)?;
                    install_available = false;
                    identity = Some(next.clone());
                    Ok(Answer::Installed { transfer })
                }
                Operation::Snapshot { artifact } => {
                    use std::os::unix::fs::OpenOptionsExt;
                    if !artifact.is_absolute() {
                        return Err("checkpoint path must be absolute".into());
                    }
                    let snapshot = vm.snapshot()?;
                    let observation = vm.stopped_observation();
                    let mut file = std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&artifact)
                        .map_err(|e| format!("create checkpoint {}: {e}", artifact.display()))?;
                    write_message(
                        &mut file,
                        &CheckpointHeader {
                            identity: request.expected.clone(),
                            observation: observation.clone(),
                            console_offset,
                        },
                    )?;
                    snapshot.write_checkpoint(&mut file)?;
                    let sha256 = artifact_hash(&mut file)?;
                    Ok(Answer::Snapshot(Checkpoint {
                        artifact,
                        identity: request.expected.clone(),
                        observation,
                        console_offset,
                        sha256,
                    }))
                }
                _ => Err("invalid worker operation".into()),
            }
        })();
        let failed = result.is_err();
        let answer = result.unwrap_or_else(Answer::Error);
        let observation = if failed {
            None
        } else {
            machine.as_ref().map(Machine::stopped_observation)
        };
        write_message(
            output,
            &Response {
                sequence,
                before,
                after: identity.clone(),
                observation,
                answer,
            },
        )?;
        if terminal || failed {
            return Ok(());
        }
        sequence = sequence.checked_add(1).ok_or("worker sequence overflow")?;
    }
    Ok(())
}

/// Owned process with sticky protocol failure. No uncertain mutation is retried.
pub struct OwnedWorker {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    sequence: u64,
    identity: Identity,
    bound: bool,
    observation: Option<StoppedObservation>,
    poison: Option<String>,
}
impl OwnedWorker {
    pub fn spawn(executable: &std::path::Path, identity: Identity) -> Result<Self> {
        let mut child = Command::new(executable)
            .args(["__engine", CONTRACT])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("machine worker {}: {e}", executable.display()))?;
        let pipes = child.stdin.take().zip(child.stdout.take());
        match pipes {
            Some((input, output)) => Ok(Self {
                child,
                input: Some(input),
                output: Some(output),
                sequence: 0,
                identity,
                bound: false,
                observation: None,
                poison: None,
            }),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                Err("machine worker pipes unavailable".into())
            }
        }
    }
    pub fn ensure_active(&self) -> Result<()> {
        match &self.poison {
            Some(error) => Err(format!("machine worker is terminal: {error}")),
            None => Ok(()),
        }
    }
    pub fn identity(&self) -> &Identity {
        &self.identity
    }
    pub fn observation(&self) -> Option<&StoppedObservation> {
        self.observation.as_ref()
    }
    pub fn invalidate(&mut self, error: &str) {
        self.poison.get_or_insert_with(|| error.into());
        if let Err(cleanup) = self.terminate() {
            self.poison = Some(format!("{error}; {cleanup}"));
        }
    }
    /// Poison first, close both pipes, then kill and join the owned process.
    /// Idempotent and never sends a protocol operation during uncertain cleanup.
    pub fn terminate(&mut self) -> Result<()> {
        self.poison
            .get_or_insert_with(|| "worker terminated".into());
        self.input = None;
        self.output = None;
        match self
            .child
            .try_wait()
            .map_err(|e| format!("worker status: {e}"))?
        {
            Some(_) => Ok(()),
            None => {
                if let Err(kill) = self.child.kill() {
                    if self
                        .child
                        .try_wait()
                        .map_err(|e| format!("worker status after kill: {e}"))?
                        .is_none()
                    {
                        return Err(format!("worker kill: {kill}"));
                    }
                }
                self.child.wait().map_err(|e| format!("worker reap: {e}"))?;
                Ok(())
            }
        }
    }
    pub fn call(&mut self, operation: Operation) -> Result<Answer> {
        if let Some(error) = &self.poison {
            return Err(format!("machine worker is terminal: {error}"));
        }
        match self.exchange(operation) {
            Ok(answer) => Ok(answer),
            Err(error) => {
                self.invalidate(&error);
                Err(self.poison.clone().unwrap_or(error))
            }
        }
    }
    fn exchange(&mut self, operation: Operation) -> Result<Answer> {
        if !self.bound && !matches!(operation, Operation::Boot(_)) {
            return Err("unbound worker requires boot".into());
        }
        if self.bound && matches!(operation, Operation::Boot(_)) {
            return Err("worker is already bound".into());
        }
        if let Operation::InstallSuccession(transfer) = &operation {
            transfer.descriptor.validate()?;
            if self.identity.incarnation != 0
                || transfer.descriptor.successor.name != self.identity.name
                || transfer.descriptor.successor.generation != self.identity.generation
            {
                return Err("worker succession identity mismatch".into());
            }
        }
        if let Operation::CrashExport { transfer, .. } = &operation {
            transfer.validate()?;
            if transfer.predecessor != self.identity {
                return Err("worker crash predecessor mismatch".into());
            }
        }
        if let Operation::Advance { hard_limit, .. } = &operation {
            if *hard_limit == u64::MAX
                || self
                    .observation
                    .as_ref()
                    .is_some_and(|o| o.time_ns > *hard_limit)
            {
                return Err("invalid worker slice hard limit".into());
            }
        }
        let request = Request {
            sequence: self.sequence,
            expected: self.identity.clone(),
            operation,
        };
        write_message(self.input.as_mut().ok_or("worker input closed")?, &request)?;
        let response =
            read_message::<Response>(self.output.as_mut().ok_or("worker output closed")?)?
                .ok_or("machine worker closed its output")?;
        self.validate_response(&request, &response)?;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("worker sequence overflow")?;
        self.identity = response.after.clone().ok_or("missing worker identity")?;
        self.bound = true;
        self.observation = response.observation;
        if matches!(response.answer, Answer::Crashed { .. } | Answer::Bye) {
            self.terminate()
                .map_err(|e| format!("acknowledged terminal operation; {e}"))?;
        }
        Ok(response.answer)
    }
    fn validate_response(&self, request: &Request, response: &Response) -> Result<()> {
        let expected_before = self.bound.then_some(&self.identity);
        let restored_identity = match &request.operation {
            Operation::Restore(saved) => Some(Identity {
                incarnation: saved.identity.incarnation,
                ..self.identity.clone()
            }),
            _ => None,
        };
        let expected_after = match &request.operation {
            Operation::InstallSuccession(t) => &t.descriptor.successor,
            Operation::Restore(_) => restored_identity.as_ref().expect("restore identity"),
            _ => &self.identity,
        };
        // A failed operation leaves the worker's identity as it was; report
        // the worker's own error rather than the identity it kept.
        if let Answer::Error(error) = &response.answer {
            if response.sequence == request.sequence && response.before.as_ref() == expected_before
            {
                return Err(error.clone());
            }
        }
        if response.sequence != request.sequence
            || response.before.as_ref() != expected_before
            || response.after.as_ref() != Some(expected_after)
        {
            let op = serde_json::to_value(&request.operation)
                .ok()
                .and_then(|v| match v {
                    serde_json::Value::String(name) => Some(name),
                    serde_json::Value::Object(map) => map.keys().next().cloned(),
                    _ => None,
                })
                .unwrap_or_default();
            return Err(format!(
                "worker response stamp mismatch for {op}: expected {expected_before:?} -> {expected_after:?}, got {:?} -> {:?}",
                response.before, response.after
            ));
        }
        if let Answer::Error(error) = &response.answer {
            return Err(error.clone());
        }
        let kind_matches = matches!(
            (&request.operation, &response.answer),
            (
                Operation::Boot(_)
                    | Operation::Control(_)
                    | Operation::Release { .. }
                    | Operation::ResumeAt(_)
                    | Operation::Park
                    | Operation::SeedTimer { .. }
                    | Operation::SeedProgressJitter { .. }
                    | Operation::Throttle { .. }
                    | Operation::HoldBlock(_)
                    | Operation::ReseedGuest { .. },
                Answer::Ready
            ) | (Operation::Advance { .. }, Answer::Advanced { .. })
                | (Operation::Frame(_), Answer::Accepted(_))
                | (Operation::DiskErrors(_), Answer::DiskErrors(_))
                | (Operation::Evidence { .. }, Answer::Evidence(_))
                | (Operation::CrashExport { .. }, Answer::Crashed { .. })
                | (Operation::InstallSuccession(_), Answer::Installed { .. })
                | (Operation::Snapshot { .. }, Answer::Snapshot(_))
                | (Operation::Restore(_), Answer::Restored(_))
                | (Operation::Quit, Answer::Bye)
        );
        if !kind_matches {
            return Err("unexpected worker answer kind".into());
        }
        let terminal = matches!(
            request.operation,
            Operation::CrashExport { .. } | Operation::Quit
        );
        if terminal != response.observation.is_none() {
            return Err("worker stopped observation missing or unexpected".into());
        }
        if let Some(observation) = &response.observation {
            if observation.time_ns == u64::MAX
                || (!matches!(request.operation, Operation::Restore(_))
                    && self.observation.as_ref().is_some_and(|old| {
                        observation.time_ns < old.time_ns || observation.exits < old.exits
                    }))
            {
                return Err("worker stopped observation moved backwards".into());
            }
            if observation.owed_io.is_some() {
                return Err("ARM worker unexpectedly owes delayed I/O".into());
            }
            if observation.powered_off as u8
                + observation.exit.is_some() as u8
                + observation.barrier as u8
                > 1
            {
                return Err("worker reported conflicting physical stop states".into());
            }
            match &request.operation {
                Operation::Snapshot { artifact } => {
                    let Answer::Snapshot(saved) = &response.answer else {
                        unreachable!()
                    };
                    if saved.artifact != *artifact
                        || saved.identity != request.expected
                        || &saved.observation != observation
                        || self.observation.as_ref() != Some(observation)
                        || saved.sha256.len() != 64
                        || !saved.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                    {
                        return Err("checkpoint acknowledgement mismatch".into());
                    }
                }
                Operation::Restore(saved) => {
                    if saved.identity.name != request.expected.name
                        || &saved.observation != observation
                        || !matches!(&response.answer, Answer::Restored(ack) if ack == saved)
                    {
                        return Err("checkpoint restore acknowledgement mismatch".into());
                    }
                }
                Operation::Boot(_) => {
                    if observation.time_ns != 0
                        || observation.exits != 0
                        || observation.abi != 0
                        || observation.powered_off
                        || observation.exit.is_some()
                        || observation.barrier
                    {
                        return Err("invalid initial worker observation".into());
                    }
                }
                Operation::InstallSuccession(transfer) => {
                    if observation.time_ns != transfer.descriptor.at
                        || observation.exits != 0
                        || observation.abi != 0
                        || observation.powered_off
                        || observation.exit.is_some()
                        || observation.barrier
                    {
                        return Err("invalid fresh successor observation".into());
                    }
                    if !matches!(&response.answer, Answer::Installed { transfer:ack } if ack == transfer)
                    {
                        return Err("successor acknowledgement transfer mismatch".into());
                    }
                }
                Operation::Advance { .. } => {
                    let old = self
                        .observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?;
                    if observation.has_control != old.has_control
                        || !(observation.abi == old.abi
                            || (old.abi == 0 && observation.abi == crate::ABI))
                    {
                        return Err("worker slice changed control capability or invalid ABI".into());
                    }
                }
                operation => {
                    let old = self
                        .observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?;
                    let expected_time = match operation {
                        Operation::ResumeAt(at) | Operation::Release { at, .. } => *at,
                        _ => old.time_ns,
                    };
                    let expected_barrier = if matches!(operation, Operation::Release { .. }) {
                        false
                    } else {
                        old.barrier
                    };
                    let preserves_deadline = matches!(
                        operation,
                        Operation::Frame(_)
                            | Operation::Control(_)
                            | Operation::Park
                            | Operation::SeedTimer { .. }
                            | Operation::SeedProgressJitter { .. }
                            | Operation::Throttle { .. }
                            | Operation::DiskErrors(_)
                            | Operation::HoldBlock(_)
                            | Operation::ReseedGuest { .. }
                            | Operation::Evidence { .. }
                    );
                    let preserves_irq = matches!(
                        operation,
                        Operation::Park
                            | Operation::SeedTimer { .. }
                            | Operation::SeedProgressJitter { .. }
                            | Operation::Throttle { .. }
                            | Operation::DiskErrors(_)
                            | Operation::HoldBlock(_)
                            | Operation::ReseedGuest { .. }
                            | Operation::Evidence { .. }
                    );
                    if (preserves_deadline && observation.next_deadline != old.next_deadline)
                        || (preserves_irq && observation.pending_irq != old.pending_irq)
                    {
                        return Err(
                            "worker mutation changed unrelated interrupt observations".into()
                        );
                    }
                    if observation.time_ns != expected_time
                        || observation.exits != old.exits
                        || observation.abi != old.abi
                        || observation.powered_off != old.powered_off
                        || observation.exit != old.exit
                        || observation.barrier != expected_barrier
                        || observation.has_control != old.has_control
                    {
                        return Err("worker mutation changed unrelated stopped facts".into());
                    }
                }
            }
            if let (Operation::Evidence { full_state, .. }, Answer::Evidence(evidence)) =
                (&request.operation, &response.answer)
            {
                validate_evidence(evidence, observation, *full_state)?;
            }
            if let Operation::Advance {
                hard_limit,
                observe_console,
                ..
            } = &request.operation
            {
                let Answer::Advanced {
                    stop,
                    time_ns,
                    console,
                    events,
                    log_chunks,
                } = &response.answer
                else {
                    unreachable!()
                };
                if observation.time_ns != *time_ns || *time_ns > *hard_limit {
                    return Err(
                        "worker slice exceeds hard limit or disagrees with observation".into(),
                    );
                }
                let management = events.iter().try_fold(0usize, |length, event| {
                    length
                        .checked_add(match event {
                            Event::Control(bytes) => bytes.len(),
                            _ => 0,
                        })
                        .ok_or("worker management output length overflow")
                })?;
                super::log_capture::validate(
                    log_chunks,
                    console.len(),
                    management,
                    self.observation
                        .as_ref()
                        .ok_or("missing previous worker observation")?
                        .time_ns,
                    *time_ns,
                    *observe_console,
                )?;
                let stop_valid = match stop {
                    MachineStop::Horizon { time_ns: t } => {
                        *t == *time_ns
                            && *t == *hard_limit
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Device { time_ns: t } => {
                        *t == *time_ns
                            && !events.is_empty()
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Console { time_ns: t } => {
                        *t == *time_ns
                            && *observe_console
                            && console.len() == 1
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::Held {
                        time_ns: t,
                        trigger,
                    } => {
                        *t == *time_ns
                            && events.contains(&Event::Held { trigger: *trigger })
                            && !observation.powered_off
                            && observation.exit.is_none()
                            && !observation.barrier
                    }
                    MachineStop::PoweredOff => observation.powered_off,
                    MachineStop::ApplicationExit { code, setup_error } => {
                        observation.exit == Some((*code, *setup_error))
                    }
                    MachineStop::Barrier => observation.barrier,
                };
                if !stop_valid {
                    return Err("worker stop and physical observation disagree".into());
                }
            }
        }
        if let (
            Operation::CrashExport {
                transfer: expected, ..
            },
            Answer::Crashed { transfer, .. },
        ) = (&request.operation, &response.answer)
        {
            expected.validate()?;
            if &transfer.descriptor != expected
                || expected.predecessor != self.identity
                || !valid_hash(&transfer.artifact_sha256)
                || transfer
                    .disk_digest
                    .as_ref()
                    .is_some_and(|hash| !valid_hash(hash))
            {
                return Err("worker crash transfer binding mismatch".into());
            }
        }
        if let (Operation::CrashExport { full_state, .. }, Answer::Crashed { evidence, .. }) =
            (&request.operation, &response.answer)
        {
            validate_evidence(
                evidence,
                self.observation
                    .as_ref()
                    .ok_or("missing crash observation")?,
                *full_state,
            )?;
        }
        if let Operation::InstallSuccession(transfer) = &request.operation {
            transfer.descriptor.validate()?;
            if self.identity.incarnation != 0
                || transfer.descriptor.successor.name != self.identity.name
                || transfer.descriptor.successor.generation != self.identity.generation
            {
                return Err("worker succession identity mismatch".into());
            }
        }
        Ok(())
    }
}
fn validate_evidence(
    evidence: &Evidence,
    observation: &StoppedObservation,
    full_state: bool,
) -> Result<()> {
    if evidence.time_ns != observation.time_ns
        || evidence.exits != observation.exits
        || evidence.format != 1
        || evidence.state_digest.is_some() != full_state
        || evidence.state_digest_parts.is_some() != full_state
        || !valid_hash(&evidence.witness)
        || !valid_hash(&evidence.trace_hash)
        || !valid_hash(&evidence.console_hash)
        || !valid_hash(&evidence.kernel_sha256)
        || !valid_hash(&evidence.initrd_sha256)
        || evidence
            .disk_digest
            .as_ref()
            .is_some_and(|hash| !valid_hash(hash))
        || evidence
            .state_digest
            .as_ref()
            .is_some_and(|hash| !valid_hash(hash))
        || evidence
            .state_digest_parts
            .as_ref()
            .is_some_and(|parts| parts.values().any(|hash| !valid_hash(hash)))
    {
        return Err("worker evidence disagrees with acknowledged physical state".into());
    }
    Ok(())
}
fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
impl Drop for OwnedWorker {
    fn drop(&mut self) {
        self.invalidate("worker dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(name: &str, generation: u64, incarnation: u32) -> Identity {
        Identity {
            name: name.into(),
            generation,
            incarnation,
        }
    }
    #[test]
    fn transfer_identity_requires_same_machine_distinct_generation_and_checked_successor() {
        let mut descriptor = TransferDescriptor {
            transaction: 1,
            predecessor: identity("a", 1, 0),
            successor: identity("a", 2, 1),
            artifact: PathBuf::from("/tmp/owned-transfer"),
            at: 10,
        };
        descriptor.validate().unwrap();
        descriptor.successor.name = "b".into();
        assert!(descriptor.validate().is_err());
        descriptor.successor.name = "a".into();
        descriptor.successor.generation = 1;
        assert!(descriptor.validate().is_err());
        descriptor.successor.generation = 2;
        descriptor.predecessor.incarnation = u32::MAX;
        descriptor.successor.incarnation = 0;
        assert!(descriptor.validate().is_err());
    }
    #[test]
    fn bounded_frames_reject_truncation_before_deserialization() {
        for bytes in [
            vec![1],
            vec![5, 0, 0, 0, b'{'],
            vec![0; 4],
            ((MAX_MESSAGE + 1) as u32).to_le_bytes().to_vec(),
        ] {
            assert!(read_message::<Request>(&mut bytes.as_slice()).is_err());
        }
        assert!(read_message::<Request>(&mut [].as_slice())
            .unwrap()
            .is_none());
        let mut bytes = Vec::new();
        write_message(
            &mut bytes,
            &Request {
                sequence: 3,
                expected: identity("a", 2, 0),
                operation: Operation::Quit,
            },
        )
        .unwrap();
        let request = read_message::<Request>(&mut bytes.as_slice())
            .unwrap()
            .unwrap();
        assert_eq!(request.sequence, 3);
        assert_eq!(request.expected, identity("a", 2, 0));
    }
    #[test]
    fn structured_disk_fault_policy_survives_worker_boot_serialization() {
        for probability in [0.0, 0.5, 1.0] {
            let model = crate::disk::parse_disk_fault_model(
                &serde_json::json!({"sector_bytes":512,"survival_probability":probability}),
            )
            .unwrap();
            let boot = Boot {
                kernel: "kernel".into(),
                initrd: "initrd".into(),
                seed: 7,
                memory_bytes: 128 << 20,
                command_line: String::new(),
                mac: [2, 0, 0, 0, 0, 1],
                disk_image: None,
                disk_sectors: Some(2),
                disk_fault_model: model,
                timer_policy: None,
                progress: None,
            };
            let boot: Boot = serde_json::from_slice(&serde_json::to_vec(&boot).unwrap()).unwrap();
            let mut disk = boot.disk().unwrap().unwrap();
            disk.write(0, &[1; 512]).unwrap();
            disk.flush();
            disk.write(1, &[2; 512]).unwrap();
            let (first, _) = disk.crashed_seeded(1009, "a", 0);
            let (again, _) = disk.crashed_seeded(1009, "a", 0);
            assert_eq!(first.digest(), again.digest());
            assert_eq!(first.fault_model(), model);
        }
    }
}
