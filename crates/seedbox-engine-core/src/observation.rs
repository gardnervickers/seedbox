//! Worker-side observation delivery. No delivery outcome can change guest
//! state, a witness, or an execution result: the coordinator publishes between
//! rounds, so waiting for the observer only delays the next round.
use seedbox_engine_protocol::{
    application_log,
    observation::{receiver_full, Kind, Observation, MAX_DATAGRAM_BYTES},
};
use std::{
    os::unix::net::UnixDatagram,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

/// Runtime-only observations waiting for coordinator commit.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub time_ns: u64,
    pub incarnation: u32,
    pub kind: Kind,
}

/// Bound both slice and round buffering independently of reader speed. What a
/// buffer keeps depends only on the machine's committed output.
#[derive(Clone, Debug, Default)]
pub(crate) struct Buffer {
    entries: Vec<Entry>,
    bytes: usize,
    lost: Option<Entry>,
}

/// Checkpoints are committed only after observation publication. Never archive
/// pending log bytes or publish them again on a restored timeline.
pub(crate) mod empty_checkpoint {
    use super::Buffer;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(buffer: &Buffer, serializer: S) -> Result<S::Ok, S::Error> {
        if !buffer.is_empty() {
            return Err(serde::ser::Error::custom(
                "checkpoint contains unpublished observations",
            ));
        }
        serializer.serialize_unit()
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Buffer, D::Error> {
        <()>::deserialize(deserializer)?;
        Ok(Buffer::default())
    }
}

impl Buffer {
    /// Records one machine may publish per round.
    const CAPACITY: usize = 8192;
    /// Application-log bytes one machine may publish per round: 2048 full
    /// chunks, bounding round memory independently of chunk sizes.
    const BYTES: usize = 2048 * application_log::MAX_BYTES;

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.lost.is_none()
    }

    pub fn push(&mut self, entry: Entry) {
        let bytes = match &entry.kind {
            Kind::Log(chunk) => chunk.bytes.len(),
            _ => 0,
        };
        if self.entries.len() < Self::CAPACITY
            && self.bytes + bytes <= Self::BYTES
            && self.lost.is_none()
        {
            self.bytes += bytes;
            self.entries.push(entry);
            return;
        }
        let records = match entry.kind {
            Kind::LogGap { records } => records,
            _ => 1,
        };
        let previous = match self.lost.as_ref().map(|entry| &entry.kind) {
            Some(Kind::LogGap { records }) => *records,
            _ => 0,
        };
        self.lost = Some(Entry {
            kind: Kind::LogGap {
                records: previous.saturating_add(records),
            },
            ..entry
        });
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Entry> + '_ {
        self.bytes = 0;
        self.entries.drain(..).chain(self.lost.take())
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
        self.lost = None;
    }
}

/// How long one mandatory observation waits for a live observer to drain
/// earlier datagrams before it is dropped as a source gap.
const STALL: Duration = Duration::from_secs(2);

struct Sender {
    socket: UnixDatagram,
    sequence: u64,
    stall: Duration,
    /// The observer let a datagram time out. Later sends try once until one is
    /// accepted, so a stopped observer delays a worker at most once.
    stalled: bool,
}

impl Sender {
    /// Wait for a full receiver only when asked and not already stalled.
    fn send(&mut self, bytes: &[u8], wait: bool) -> bool {
        let mut deadline = None;
        let mut pause = Duration::from_micros(20);
        loop {
            match self.socket.send(bytes) {
                Ok(_) => {
                    self.stalled = false;
                    return true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) if wait && !self.stalled && receiver_full(&error) => {
                    let now = Instant::now();
                    let deadline = *deadline.get_or_insert(now + self.stall);
                    if now >= deadline {
                        self.stalled = true;
                        return false;
                    }
                    std::thread::sleep(pause.min(deadline - now));
                    pause = (pause * 2).min(Duration::from_millis(1));
                }
                Err(_) => return false,
            }
        }
    }
}

pub struct Publisher(Mutex<Sender>);

impl Drop for Publisher {
    fn drop(&mut self) {
        // Best effort, like every observation. If this final watermark is lost
        // the receiver explicitly reports an uncertain tail after worker exit.
        self.publish("", 0, 0, Kind::Closed);
    }
}

impl Publisher {
    pub fn connect(path: &Path) -> std::io::Result<Arc<Self>> {
        Self::connect_with_stall(path, STALL)
    }

    fn connect_with_stall(path: &Path, stall: Duration) -> std::io::Result<Arc<Self>> {
        let socket = UnixDatagram::unbound()?;
        seedbox_engine_protocol::observation::configure_socket(&socket)?;
        socket.connect(path)?;
        socket.set_nonblocking(true)?;
        Ok(Arc::new(Self(Mutex::new(Sender {
            socket,
            sequence: 0,
            stall,
            stalled: false,
        }))))
    }

    pub(crate) fn publish(
        &self,
        machine: &str,
        incarnation: u32,
        simulated_time_ns: u64,
        kind: Kind,
    ) {
        let Ok(mut sender) = self.0.lock() else {
            return;
        };
        let Some(sequence) = sender.sequence.checked_add(1) else {
            return;
        };
        let optional_frames = matches!(kind, Kind::Frames { .. });
        if !optional_frames {
            sender.sequence = sequence;
        }
        let observation = Observation {
            sequence,
            machine: machine.to_owned(),
            incarnation,
            simulated_time_ns,
            kind,
        };
        // Mandatory observations wait for a live observer; optional frame
        // batches never delay a round.
        if let Ok(bytes) = serde_json::to_vec(&observation) {
            if bytes.len() <= MAX_DATAGRAM_BYTES
                && sender.send(&bytes, !optional_frames)
                && optional_frames
            {
                sender.sequence = sequence;
            }
        }
    }
}

/// Per-machine decoder. Snapshot/restore copies only the incomplete frame;
/// publisher delivery sequence and socket ownership remain outside snapshots.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "DecoderState", into = "DecoderState")]
pub struct Decoder {
    line: Vec<u8>,
    oversized: bool,
}

const MAX_LINE: usize = 16_384 + 64;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DecoderState {
    line: Vec<u8>,
    oversized: bool,
}
impl From<Decoder> for DecoderState {
    fn from(decoder: Decoder) -> Self {
        Self {
            line: decoder.line,
            oversized: decoder.oversized,
        }
    }
}
impl TryFrom<DecoderState> for Decoder {
    type Error = String;
    fn try_from(saved: DecoderState) -> Result<Self, Self::Error> {
        if saved.line.len() > MAX_LINE
            || saved.line.contains(&b'\n')
            || (saved.oversized && saved.line.len() != MAX_LINE)
        {
            return Err("invalid checkpoint application-log frame".into());
        }
        Ok(Self {
            line: saved.line,
            oversized: saved.oversized,
        })
    }
}

impl Decoder {
    /// Semantic output triggers consume the same decoded frames even without
    /// an observation socket. Publication never selects execution behavior.
    pub fn commit(
        &mut self,
        state: &mut crate::world::MachineState,
        publisher: Option<&Publisher>,
        time_ns: u64,
        bytes: &[u8],
    ) {
        self.receive(bytes, |kind| {
            if state.observe_console {
                if let Kind::Log(chunk) = &kind {
                    state.committed_output.push((time_ns, chunk.bytes.clone()));
                }
            }
            if publisher.is_some() {
                state.observations.push(Entry {
                    time_ns,
                    incarnation: state.incarnation(),
                    kind,
                });
            }
        });
    }

    pub fn receive(&mut self, bytes: &[u8], mut emit: impl FnMut(Kind)) {
        for &byte in bytes {
            if byte == b'\n' {
                if self.oversized {
                    if self.line.starts_with(application_log::PREFIX) {
                        emit(Kind::MalformedLog);
                    }
                } else {
                    match application_log::decode(&self.line) {
                        Ok(Some(chunk)) => emit(Kind::Log(chunk)),
                        Ok(None) => {}
                        Err(_) => emit(Kind::MalformedLog),
                    }
                }
                self.line.clear();
                self.oversized = false;
            } else if self.line.len() < MAX_LINE {
                self.line.push(byte);
            } else {
                self.oversized = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use seedbox_engine_protocol::application_log::{Chunk, Stream};
    use seedbox_engine_protocol::observation::configure_socket;

    #[test]
    fn slice_and_round_overflow_preserve_exact_omission_count() {
        let per_slice = Buffer::CAPACITY as u64 + 36;
        let mut round = Buffer::default();
        for slice in 0..3 {
            let mut pending = Buffer::default();
            for offset in 0..per_slice {
                pending.push(Entry {
                    time_ns: slice * per_slice + offset,
                    incarnation: 2,
                    kind: Kind::MalformedLog,
                });
            }
            for entry in pending.drain() {
                round.push(entry);
            }
            assert!(pending.is_empty());
        }
        let entries: Vec<_> = round.drain().collect();
        assert_eq!(entries.len(), Buffer::CAPACITY + 1);
        let gap = entries.last().unwrap();
        assert_eq!(
            gap.kind,
            Kind::LogGap {
                records: 3 * per_slice - Buffer::CAPACITY as u64
            }
        );
        assert_eq!((gap.time_ns, gap.incarnation), (3 * per_slice - 1, 2));
        assert!(round.is_empty());
    }

    #[test]
    fn round_output_bytes_are_bounded_and_later_records_stay_behind_the_gap() {
        let chunk = |bytes| Entry {
            time_ns: 7,
            incarnation: 1,
            kind: Kind::Log(Chunk {
                stream: Stream::Stdout,
                bytes: vec![b'x'; bytes],
            }),
        };
        let mut round = Buffer::default();
        // Small chunks are bounded by record count, not bytes.
        for _ in 0..Buffer::CAPACITY / 2 {
            round.push(chunk(1));
        }
        assert_eq!(round.drain().count(), Buffer::CAPACITY / 2);
        let full = Buffer::BYTES / application_log::MAX_BYTES;
        for _ in 0..full {
            round.push(chunk(application_log::MAX_BYTES));
        }
        round.push(chunk(1));
        round.push(Entry {
            time_ns: 8,
            incarnation: 1,
            kind: Kind::MalformedLog,
        });
        let entries: Vec<_> = round.drain().collect();
        assert_eq!(entries.len(), full + 1);
        let gap = entries.last().unwrap();
        assert_eq!((gap.time_ns, &gap.kind), (8, &Kind::LogGap { records: 2 }));
        // Draining restores the whole byte budget for the next round.
        round.push(chunk(application_log::MAX_BYTES));
        assert_eq!(round.drain().count(), 1);
    }

    #[test]
    fn large_frame_batches_cross_darwin_default_limit_and_omissions_do_not_gap_logs() {
        use seedbox_engine_protocol::observation::{Frame, FrameFate};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observer");
        let receiver = UnixDatagram::bind(&path).unwrap();
        configure_socket(&receiver).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(1)))
            .unwrap();
        let publisher = Publisher::connect(&path).unwrap();
        let frame = Frame {
            from: "sender".into(),
            to: "receiver".into(),
            sent_ns: 1,
            due_ns: Some(2),
            bytes: 60,
            fate: FrameFate::Scheduled,
        };
        publisher.publish(
            "",
            0,
            1,
            Kind::Frames {
                frames: vec![frame.clone(); 96],
            },
        );
        let mut bytes = [0; MAX_DATAGRAM_BYTES];
        let count = receiver.recv(&mut bytes).unwrap();
        assert!(count > 2048);
        let observation: Observation = serde_json::from_slice(&bytes[..count]).unwrap();
        assert_eq!(observation.sequence, 1);
        // An omitted visualization batch has no application output to lose.
        publisher.publish(
            "",
            0,
            2,
            Kind::Frames {
                frames: vec![frame; 1000],
            },
        );
        publisher.publish("oracle", 0, 3, Kind::MachineReady);
        let count = receiver.recv(&mut bytes).unwrap();
        let observation: Observation = serde_json::from_slice(&bytes[..count]).unwrap();
        assert_eq!(observation.sequence, 2);
        assert_eq!(observation.kind, Kind::MachineReady);
        // Oversized mandatory observations still leave an explicit sequence gap.
        publisher.publish(&"x".repeat(MAX_DATAGRAM_BYTES), 0, 4, Kind::MachineReady);
        publisher.publish("oracle", 0, 5, Kind::MachineReady);
        let count = receiver.recv(&mut bytes).unwrap();
        let observation: Observation = serde_json::from_slice(&bytes[..count]).unwrap();
        assert_eq!(observation.sequence, 4);
    }

    #[test]
    fn semantic_output_commits_on_complete_frames_with_or_without_delivery() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observer");
        let receiver = UnixDatagram::bind(&path).unwrap();
        let publisher = Publisher::connect(&path).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let mut decoder = Decoder::default();
        let mut state = crate::world::MachineState::new("oracle".into());
        decoder.commit(&mut state, Some(&publisher), 1, b"SBX_LOG1 1 61\n");
        let mut datagram = [0; MAX_DATAGRAM_BYTES];
        assert_eq!(
            receiver.recv(&mut datagram).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let pending: Vec<_> = state.observations.drain().collect();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].time_ns, 1);
        assert_eq!(
            pending[0].kind,
            Kind::Log(Chunk {
                stream: Stream::Stdout,
                bytes: b"a".to_vec()
            })
        );
        drop(receiver); // Delivery fails; semantic consumption must still succeed.
        for sink in [None, Some(publisher.as_ref())] {
            let mut decoder = Decoder::default();
            let mut state = crate::world::MachineState::new("oracle".into());
            state.observe_console = true;
            decoder.commit(&mut state, sink, 2, b"SBX_LOG1 1 5245");
            assert!(state.committed_output.is_empty());
            decoder.commit(&mut state, sink, 4, b"414459\n");
            assert_eq!(state.committed_output, vec![(4, b"READY".to_vec())]);
            state.clear_effects();
            state.observe_console = false;
            decoder.commit(&mut state, sink, 5, b"SBX_LOG1 2 00ff\n");
            assert!(state.committed_output.is_empty());
        }
    }

    #[test]
    fn partial_frames_survive_snapshot_without_mixing_management_or_restarts() {
        let mut decoder = Decoder::default();
        let mut output = Vec::new();
        decoder.receive(b"SBX_RPC1 1 1 {}\nSBX_LOG1 2 00", |kind| output.push(kind));
        assert!(output.is_empty());
        let saved = serde_json::to_value(&decoder).unwrap();
        let mut restored: Decoder = serde_json::from_value(saved.clone()).unwrap();
        let mut invalid = saved;
        invalid["oversized"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Decoder>(invalid).is_err());
        restored.receive(b"ff\n", |kind| output.push(kind));
        assert_eq!(
            output,
            vec![Kind::Log(Chunk {
                stream: Stream::Stderr,
                bytes: vec![0, 255]
            })]
        );
        output.clear();
        let mut reboot = Decoder::default();
        reboot.receive(b"ff\nSBX_LOG1 1 61\n", |kind| output.push(kind));
        assert_eq!(
            output,
            vec![Kind::Log(Chunk {
                stream: Stream::Stdout,
                bytes: b"a".to_vec()
            })]
        );
    }

    #[test]
    fn corrupt_or_oversized_frames_recover_at_next_line() {
        let mut decoder = Decoder::default();
        let mut output = Vec::new();
        let bytes = [
            b"SBX_LOG1 1 ".as_slice(),
            &vec![b'0'; 32_768],
            b"\nSBX_LOG1 1 zz\nSBX_LOG1 2 62\n",
        ]
        .concat();
        decoder.receive(&bytes, |kind| output.push(kind));
        assert_eq!(
            output,
            vec![
                Kind::MalformedLog,
                Kind::MalformedLog,
                Kind::Log(Chunk {
                    stream: Stream::Stderr,
                    bytes: b"b".to_vec()
                })
            ]
        );
    }

    #[test]
    fn mandatory_observations_wait_for_a_slow_live_observer() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observations.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        configure_socket(&receiver).unwrap();
        receiver
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let publisher = Publisher::connect(&path).unwrap();
        // Far more than the receive buffer holds: the observer falls behind.
        const COUNT: u64 = 1024;
        let reader = std::thread::spawn(move || {
            let mut buffer = [0; MAX_DATAGRAM_BYTES];
            let mut sequences = Vec::new();
            while sequences.len() < COUNT as usize {
                let count = receiver.recv(&mut buffer).unwrap();
                let observation: Observation = serde_json::from_slice(&buffer[..count]).unwrap();
                sequences.push(observation.sequence);
                if sequences.len() % 64 == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
            sequences
        });
        for _ in 0..COUNT {
            publisher.publish(
                "app",
                1,
                9,
                Kind::Log(Chunk {
                    stream: Stream::Stdout,
                    bytes: vec![0xff; application_log::MAX_BYTES],
                }),
            );
        }
        assert_eq!(reader.join().unwrap(), (1..=COUNT).collect::<Vec<_>>());
    }

    #[test]
    fn a_stopped_observer_delays_once_and_reports_sequence_gaps() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("observations.sock");
        let receiver = UnixDatagram::bind(&path).unwrap();
        receiver.set_nonblocking(true).unwrap();
        let stall = std::time::Duration::from_millis(50);
        let publisher = Publisher::connect_with_stall(&path, stall).unwrap();
        let emit = || publisher.publish("oracle", 3, 42, Kind::MalformedLog);
        let started = Instant::now();
        for _ in 0..4096 {
            emit();
        }
        // One timed-out wait, not one per lost observation.
        assert!(started.elapsed() >= stall && started.elapsed() < stall * 20);
        let mut buffer = [0; MAX_DATAGRAM_BYTES];
        let mut last = 0;
        while let Ok(count) = receiver.recv(&mut buffer) {
            let observation: Observation = serde_json::from_slice(&buffer[..count]).unwrap();
            assert_eq!(observation.machine, "oracle");
            assert_eq!(observation.incarnation, 3);
            assert_eq!(observation.simulated_time_ns, 42);
            assert!(observation.sequence > last);
            last = observation.sequence;
        }
        assert!(last > 0 && last < 4096);
        emit();
        let count = receiver.recv(&mut buffer).unwrap();
        let observation: Observation = serde_json::from_slice(&buffer[..count]).unwrap();
        assert_eq!(observation.sequence, 4097);
    }
}
