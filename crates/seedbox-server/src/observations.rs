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

    #[cfg(test)]
    fn with_sink(emit: impl Fn(LogEntry) + Send + 'static) -> io::Result<Self> {
        Self::with_sinks(emit, |_, _| {}, |_, _| {})
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
                                    counts,
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
                                        counts: counts.map(Box::new),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn machine_events_and_uncertain_tail_are_streamed_without_inventing_a_verdict() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = received.clone();
        let batches = Arc::new(Mutex::new(Vec::new()));
        let captured_frames = batches.clone();
        let receiver = Receiver::with_sinks(
            |_| {},
            move |time, event| {
                captured.lock().unwrap().push((time, event));
            },
            move |time, frames| {
                captured_frames.lock().unwrap().push((time, frames));
            },
        )
        .unwrap();
        let frame = seedbox_api::Frame {
            from: "oracle".into(),
            to: "db".into(),
            sent_ns: 25,
            due_ns: Some(26),
            bytes: 60,
            fate: seedbox_api::FrameFate::Scheduled,
        };
        let stop = seedbox_api::FaultAction::Stop {
            machine: "oracle".into(),
        };
        let sender = UnixDatagram::unbound().unwrap();
        sender.connect(receiver.path()).unwrap();
        for (sequence, simulated_time_ns, kind) in [
            (1, 10, Kind::MachineReady),
            (
                2,
                15,
                Kind::MachineLifecycle {
                    transition: seedbox_engine_protocol::observation::MachineTransition::Paused,
                },
            ),
            (4, 20, Kind::MachineExited { code: 7 }),
            (
                5,
                30,
                Kind::FaultChanged {
                    counts: None,
                    id: 1,
                    action: "stop".into(),
                    machine: Some("oracle".into()),
                    status: seedbox_api::FaultStatus::Unavailable,
                    applied_time_ns: None,
                    plan: Some(Box::new(stop.clone())),
                    input_key: None,
                },
            ),
            (
                6,
                30,
                Kind::Frames {
                    frames: vec![frame.clone()],
                },
            ),
        ] {
            sender
                .send(
                    &serde_json::to_vec(&Observation {
                        sequence,
                        simulated_time_ns,
                        machine: "oracle".into(),
                        incarnation: 2,
                        kind,
                    })
                    .unwrap(),
                )
                .unwrap();
        }
        drop(receiver);
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                (
                    10,
                    EventKind::MachineReady {
                        machine: "oracle".into(),
                        incarnation: 2
                    }
                ),
                (
                    15,
                    EventKind::MachineLifecycle {
                        machine: "oracle".into(),
                        incarnation: 2,
                        transition: seedbox_api::MachineTransition::Paused
                    }
                ),
                (20, EventKind::SourceGap { first: 3, last: 3 }),
                (
                    20,
                    EventKind::MachineExited {
                        machine: "oracle".into(),
                        incarnation: 2,
                        code: 7
                    }
                ),
                (
                    30,
                    EventKind::FaultChanged {
                        counts: None,
                        id: 1,
                        action: "stop".into(),
                        machine: Some("oracle".into()),
                        status: seedbox_api::FaultStatus::Unavailable,
                        applied_time_ns: None,
                        plan: Some(Box::new(serde_json::to_value(stop).unwrap())),
                        input_key: None,
                    }
                ),
                (30, EventKind::SourceUnavailable),
            ]
        );
        assert_eq!(*batches.lock().unwrap(), vec![(30, vec![frame])]);
    }

    #[test]
    fn drain_preserves_binary_output_and_reports_interior_and_terminal_loss() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let captured = received.clone();
        let receiver =
            Receiver::with_sink(move |entry| captured.lock().unwrap().push(entry)).unwrap();
        let sender = UnixDatagram::unbound().unwrap();
        sender.connect(receiver.path()).unwrap();
        for (sequence, kind) in [
            (
                1,
                Kind::Log(application_log::Chunk {
                    stream: application_log::Stream::Stderr,
                    bytes: vec![0, 255, 10],
                }),
            ),
            (3, Kind::LogGap { records: 17 }),
            (4, Kind::Closed),
        ] {
            let observation = Observation {
                sequence,
                machine: "oracle".into(),
                incarnation: 2,
                simulated_time_ns: 123,
                kind,
            };
            sender
                .send(&serde_json::to_vec(&observation).unwrap())
                .unwrap();
        }
        drop(receiver);
        assert_eq!(
            *received.lock().unwrap(),
            vec![
                LogEntry::Bytes {
                    machine: "oracle".into(),
                    incarnation: 2,
                    simulated_time_ns: 123,
                    stream: LogStream::Stderr,
                    bytes: vec![0, 255, 10]
                },
                LogEntry::SourceGap { first: 2, last: 2 },
                LogEntry::MachineGap {
                    machine: "oracle".into(),
                    incarnation: 2,
                    simulated_time_ns: 123,
                    records: 17,
                },
            ]
        );
        received.lock().unwrap().clear();
        let captured = received.clone();
        let receiver =
            Receiver::with_sink(move |entry| captured.lock().unwrap().push(entry)).unwrap();
        drop(receiver);
        assert_eq!(*received.lock().unwrap(), vec![LogEntry::SourceUnavailable]);
    }
}
