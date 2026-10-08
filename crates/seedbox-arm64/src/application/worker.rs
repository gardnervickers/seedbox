//! Owned single-machine worker. HVF permits one VM per process; the world
//! coordinator serializes these workers through a bounded, numbered protocol.
use super::{Event, Evidence};
use crate::{
    backend::Backend,
    boot::BootSpec,
    disk::Disk,
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
pub const CONTRACT: &str = "arm64-machine-v1";

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
    pub torn_writes: bool,
    #[serde(default)]
    pub timer_policy: Option<super::TimerPolicy>,
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
            .map(|disk| Some(disk.with_torn_writes(self.torn_writes)))
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Operation {
    Boot(Boot),
    Advance {
        horizon_ns: u64,
    },
    Frame(Vec<u8>),
    Control(Vec<u8>),
    Release([u32; 8]),
    ResumeAt(u64),
    Evidence {
        full_state: bool,
    },
    Crash {
        run_seed: u64,
        name: String,
        incarnation: u32,
        full_state: bool,
    },
    Reboot {
        boot: Boot,
        run_seed: u64,
        name: String,
        incarnation: u32,
    },
    Quit,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub sequence: u64,
    pub operation: Operation,
}
#[derive(Debug, Serialize, Deserialize)]
pub enum Answer {
    Ready,
    Advanced {
        stop: MachineStop,
        time_ns: u64,
        events: Vec<Event>,
        console: Vec<u8>,
    },
    Accepted(bool),
    Evidence(Evidence),
    Rebooted {
        lost_sectors: u64,
    },
    Crashed {
        evidence: Evidence,
        lost_sectors: u64,
        surviving_disk: Option<String>,
    },
    Error(String),
    Bye,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub sequence: u64,
    pub answer: Answer,
}

pub fn read_message<T: serde::de::DeserializeOwned>(input: &mut impl Read) -> Result<Option<T>> {
    let mut length = [0u8; 4];
    // EOF is legal only between messages. Short headers/bodies are failures.
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

/// The boundary supplies I/O; this library never reads arguments/environment
/// or writes diagnostics. Each operation completes with the vCPU stopped.
pub fn serve<B: Backend>(input: &mut impl Read, output: &mut impl Write) -> Result<()> {
    let mut sequence = 0u64;
    let mut machine: Option<Machine<B>> = None;
    let mut retained_disk: Option<Disk> = None;
    let mut has_crashed = false;
    let mut console_offset = 0usize;
    while let Some(request) = read_message::<Request>(input)? {
        if request.sequence != sequence {
            return Err("worker request sequence mismatch".into());
        }
        let quit = matches!(request.operation, Operation::Quit);
        let result = (|| -> Result<Answer> {
            match request.operation {
                Operation::Boot(boot) => {
                    if machine.is_some() {
                        return Err("worker already owns a machine".into());
                    }
                    let disk = if has_crashed {
                        retained_disk.take()
                    } else {
                        boot.disk()?
                    };
                    machine = Some(boot.machine(disk)?);
                    has_crashed = false;
                    console_offset = 0;
                    Ok(Answer::Ready)
                }
                Operation::Quit => {
                    machine = None;
                    Ok(Answer::Bye)
                }
                operation => {
                    let vm = machine.as_mut().ok_or("worker has no machine")?;
                    match operation {
                        Operation::Advance { horizon_ns } => {
                            let stop = vm.run_until(horizon_ns, || {})?;
                            let console = vm.console()[console_offset..].to_vec();
                            console_offset = vm.console().len();
                            Ok(Answer::Advanced {
                                stop,
                                time_ns: vm.time_ns(),
                                events: vm.take_events(),
                                console,
                            })
                        }
                        Operation::Frame(bytes) => Ok(Answer::Accepted(vm.receive_frame(&bytes)?)),
                        Operation::Control(bytes) => {
                            vm.receive_control(&bytes)?;
                            Ok(Answer::Ready)
                        }
                        Operation::Release(words) => {
                            vm.release_barrier(words)?;
                            Ok(Answer::Ready)
                        }
                        Operation::ResumeAt(time) => {
                            vm.resume_at(time)?;
                            Ok(Answer::Ready)
                        }
                        Operation::Evidence { full_state } => {
                            Ok(Answer::Evidence(vm.application_evidence(full_state)?))
                        }
                        Operation::Crash {
                            run_seed,
                            name,
                            incarnation,
                            full_state,
                        } => {
                            let evidence = vm.application_evidence(full_state)?;
                            let (disk, lost) = vm
                                .crashed_disk(run_seed, &name, incarnation)
                                .map(|(disk, lost)| (Some(disk), lost))
                                .unwrap_or((None, 0));
                            let surviving_disk = disk.as_ref().map(crate::disk::Disk::digest);
                            retained_disk = disk;
                            has_crashed = true;
                            machine = None;
                            Ok(Answer::Crashed {
                                evidence,
                                lost_sectors: lost,
                                surviving_disk,
                            })
                        }
                        Operation::Reboot {
                            boot,
                            run_seed,
                            name,
                            incarnation,
                        } => {
                            let (disk, lost) = vm
                                .crashed_disk(run_seed, &name, incarnation)
                                .map(|(disk, lost)| (Some(disk), lost))
                                .unwrap_or((None, 0));
                            // Destroy the previous HVF VM before creating its successor.
                            machine = None;
                            machine = Some(boot.machine(disk)?);
                            console_offset = 0;
                            Ok(Answer::Rebooted { lost_sectors: lost })
                        }
                        _ => Err("invalid worker operation".into()),
                    }
                }
            }
        })();
        let answer = result.unwrap_or_else(Answer::Error);
        write_message(output, &Response { sequence, answer })?;
        if quit {
            return Ok(());
        }
        sequence = sequence.checked_add(1).ok_or("worker sequence overflow")?;
    }
    Ok(())
}

/// A worker inherits the coordinator's process group. Public timeout/cancel
/// therefore terminates every VM; ordinary cleanup kills and reaps this child.
pub struct OwnedWorker {
    child: Child,
    input: ChildStdin,
    output: ChildStdout,
    sequence: u64,
}
impl OwnedWorker {
    pub fn spawn(executable: &std::path::Path) -> Result<Self> {
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
                input,
                output,
                sequence: 0,
            }),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                Err("machine worker pipes unavailable".into())
            }
        }
    }
    pub fn call(&mut self, operation: Operation) -> Result<Answer> {
        write_message(
            &mut self.input,
            &Request {
                sequence: self.sequence,
                operation,
            },
        )?;
        let response = read_message::<Response>(&mut self.output)?
            .ok_or("machine worker closed its output")?;
        if response.sequence != self.sequence {
            return Err("worker response sequence mismatch".into());
        }
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("worker sequence overflow")?;
        match response.answer {
            Answer::Error(error) => Err(error),
            answer => Ok(answer),
        }
    }
}
impl Drop for OwnedWorker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                operation: Operation::Quit,
            },
        )
        .unwrap();
        assert_eq!(
            read_message::<Request>(&mut bytes.as_slice())
                .unwrap()
                .unwrap()
                .sequence,
            3
        );
    }
}
