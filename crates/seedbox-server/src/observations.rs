//! One private observation socket per execution. The reader outlives the worker
//! and drains its final datagrams before the simulation's public streams close.
use crate::manager::Manager;
use seedbox_api::{EventKind, LogEntry, LogStream, SimulationId};
use seedbox_engine_protocol::{
    application_log,
    observation::{Kind, Observation, MAX_DATAGRAM_BYTES},
};
use std::{
    io,
    os::unix::net::UnixDatagram,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};

pub(crate) struct Receiver {
    stop: Arc<AtomicBool>,
    task: Option<thread::JoinHandle<()>>,
    directory: tempfile::TempDir,
}

impl Receiver {
    pub fn start(manager: Arc<Manager>, id: SimulationId) -> io::Result<Self> {
        let events = manager.clone();
        let frames = manager.clone();
        let event_id = id.clone();
        let frame_id = id.clone();
        Self::with_sinks(
            move |entry| manager.publish_log(&id, entry),
            move |time, event| events.publish_engine_event(&event_id, time, event),
            move |time, batch| frames.publish_frames(&frame_id, time, batch),
        )
    }

    fn with_sinks(
        emit: impl Fn(LogEntry) + Send + 'static,
        event: impl Fn(u64, EventKind) + Send + 'static,
        frames: impl Fn(u64, Vec<seedbox_api::Frame>) + Send + 'static,
    ) -> io::Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("seedbox-observe-")
            .tempdir_in("/tmp")?;
        let socket = UnixDatagram::bind(directory.path().join("socket"))?;
        seedbox_engine_protocol::observation::configure_socket(&socket)?;
        socket.set_read_timeout(Some(Duration::from_millis(20)))?;
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let task = thread::Builder::new()
            .name("seedbox-observations".into())
            .spawn(move || {
                let mut last = 0;
                let mut closed = false;
                let mut last_time = 0;
                let mut buffer = [0; MAX_DATAGRAM_BYTES + 1];
                loop {
                    match socket.recv(&mut buffer) {
                        Ok(count) => {
                            let observation = if count <= MAX_DATAGRAM_BYTES {
                                serde_json::from_slice::<Observation>(&buffer[..count]).ok()
                            } else {
                                None
                            };
                            let Some(observation) = observation.filter(|o| o.sequence > last)
                            else {
                                emit(LogEntry::SourceUnavailable);
                                event(last_time, EventKind::SourceUnavailable);
                                continue;
                            };
                            if !matches!(observation.kind, Kind::Closed) {
                                last_time = observation.simulated_time_ns;
                            }
                            if observation.sequence > last + 1 {
                                event(
                                    last_time,
                                    EventKind::SourceGap {
                                        first: last + 1,
                                        last: observation.sequence - 1,
                                    },
                                );
                                emit(LogEntry::SourceGap {
                                    first: last + 1,
                                    last: observation.sequence - 1,
                                });
                            }
                            last = observation.sequence;
                            match observation.kind {
                                Kind::OverrideRejected {
                                    input_key,
                                    at_ns,
                                    reason,
                                } => event(
                                    last_time,
                                    EventKind::OverrideRejected {
                                        input_key,
                                        at_ns,
                                        reason,
                                    },
                                ),
                                Kind::FaultChanged {
                                    id,
                                    action,
                                    machine,
                                    status,
                                    applied_time_ns,
                                    plan,
                                    input_key,
                                } => event(
                                    last_time,
                                    EventKind::FaultChanged {
                                        id,
                                        action,
                                        machine,
                                        status,
                                        applied_time_ns,
                                        plan: plan.map(|plan| {
                                            Box::new(
                                                serde_json::to_value(plan)
                                                    .expect("action serializes"),
                                            )
                                        }),
                                        input_key,
                                    },
                                ),
                                Kind::Frames { frames: batch } => frames(last_time, batch),
                                Kind::MachineLifecycle { transition } => event(
                                    last_time,
                                    EventKind::MachineLifecycle {
                                        machine: observation.machine,
                                        incarnation: observation.incarnation,
                                        transition,
                                    },
                                ),
                                Kind::MachineReady => event(
                                    last_time,
                                    EventKind::MachineReady {
                                        machine: observation.machine,
                                        incarnation: observation.incarnation,
                                    },
                                ),
                                Kind::MachineExited { code } => event(
                                    last_time,
                                    EventKind::MachineExited {
                                        machine: observation.machine,
                                        incarnation: observation.incarnation,
                                        code,
                                    },
                                ),
                                Kind::Closed => {
                                    closed = true;
                                    break;
                                }
                                Kind::LogGap { records } => emit(LogEntry::MachineGap {
                                    machine: observation.machine,
                                    incarnation: observation.incarnation,
                                    simulated_time_ns: observation.simulated_time_ns,
                                    records,
                                }),
                                Kind::MalformedLog => emit(LogEntry::SourceUnavailable),
                                Kind::Log(chunk)
                                    if chunk.bytes.len() > application_log::MAX_BYTES =>
                                {
                                    emit(LogEntry::SourceUnavailable);
                                }
                                Kind::Log(chunk) => emit(LogEntry::Bytes {
                                    machine: observation.machine,
                                    incarnation: observation.incarnation,
                                    simulated_time_ns: observation.simulated_time_ns,
                                    stream: match chunk.stream {
                                        application_log::Stream::Stdout => LogStream::Stdout,
                                        application_log::Stream::Stderr => LogStream::Stderr,
                                    },
                                    bytes: chunk.bytes,
                                }),
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error)
                            if matches!(
                                error.kind(),
                                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                            ) =>
                        {
                            if done.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                if !closed {
                    emit(LogEntry::SourceUnavailable);
                    event(last_time, EventKind::SourceUnavailable);
                }
            })?;
        Ok(Self {
            stop,
            task: Some(task),
            directory,
        })
    }
    pub fn path(&self) -> std::path::PathBuf {
        self.directory.path().join("socket")
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}
