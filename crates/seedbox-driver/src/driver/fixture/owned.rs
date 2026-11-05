//! Driver unit-test fixtures: owned machines, caller contexts and mock artifact bytes.
//! This module exists only under cfg(test); no fixture is a production API.
use crate::driver::*;
use crate::persistence::{Decoder, Encoder, Wire};
use crate::pv::{CommandResult, Input, Output, SimulationControlId, Stream};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
pub(in crate::driver) type Io = std::io::Error;
pub(in crate::driver) fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
pub(in crate::driver) fn key(s: &str) -> InputKey {
    InputKey::new(s).unwrap()
}
pub(in crate::driver) fn io(s: &str) -> Io {
    Io::other(s)
}
pub(in crate::driver) fn nz(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).unwrap()
}
pub(in crate::driver) fn call(n: u32, fields: &str) -> Response {
    Response::SimulationControl {
        id: SimulationControlId {
            session: nz(1),
            request: nz(n),
        },
        payload: format!("{{\"version\":1,\"id\":{n},{fields}}}").into_bytes(),
    }
}
pub(in crate::driver) fn output(bytes: &[u8]) -> Response {
    Response::ApplicationOutput {
        stream: Stream::Stdout,
        bytes: bytes.to_vec(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::driver) struct State {
    pub(in crate::driver) name: String,
    pub(in crate::driver) reached: u64,
    pub(in crate::driver) busy: bool,
    pub(in crate::driver) dead: bool,
    pub(in crate::driver) entropy: Vec<u8>,
    pub(in crate::driver) fills: usize,
    pub(in crate::driver) sends: VecDeque<(u64, Response)>,
    pub(in crate::driver) received: Vec<Input>,
    pub(in crate::driver) fail_capture: bool,
    pub(in crate::driver) fail_run: bool,
}
pub(in crate::driver) struct Mock {
    pub(in crate::driver) state: State,
    pub(in crate::driver) dropped: Option<Arc<AtomicUsize>>,
    pub(in crate::driver) captures: Arc<AtomicUsize>,
    pub(in crate::driver) saved_drops: Option<Arc<AtomicUsize>>,
}
// Intentionally no Clone implementation or bound on the opaque snapshot.
pub(in crate::driver) struct Saved(
    pub(in crate::driver) State,
    pub(in crate::driver) Option<Arc<AtomicUsize>>,
);
impl Drop for Saved {
    fn drop(&mut self) {
        if let Some(n) = &self.1 {
            n.fetch_add(1, Ordering::SeqCst);
        }
    }
}
pub(in crate::driver) struct Context(pub(in crate::driver) Option<Arc<AtomicUsize>>);
impl Drop for Context {
    fn drop(&mut self) {
        if let Some(n) = &self.0 {
            n.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Drop for Mock {
    fn drop(&mut self) {
        if let Some(n) = &self.dropped {
            n.fetch_add(1, Ordering::SeqCst);
        }
    }
}
impl Mock {
    pub(in crate::driver) fn new(state: State) -> Self {
        Self {
            state,
            dropped: None,
            captures: Arc::new(AtomicUsize::new(0)),
            saved_drops: None,
        }
    }
    pub(in crate::driver) fn restore(saved: &Saved) -> Result<Self, Io> {
        Ok(Self::new(saved.0.clone()))
    }
}
impl Machine for Mock {
    type Context = Context;
    type Error = Io;
    type Snapshot = Saved;
    fn run(&mut self, c: &mut RunContext<'_, Context>, upto: Timestamp) -> Result<StopReason, Io> {
        let s = &mut self.state;
        if s.fail_run {
            return Err(io("run failure"));
        }
        let now = c.clock.now();
        s.reached = s.reached.max(now);
        while let Some(input) = c.inbox.take() {
            match &input.payload {
                Request::Crash => s.dead = true,
                Request::Reboot => {
                    s.dead = false;
                }
                Request::Application { id, .. } | Request::StepRealtime { id, .. } => {
                    // Delayed completion leaves a real accepted command in flight.
                    let item = (
                        now + 3,
                        Response::CommandAcknowledged {
                            id: *id,
                            result: CommandResult::Completed,
                        },
                    );
                    let index = s.sends.partition_point(|(t, _)| *t <= item.0);
                    s.sends.insert(index, item);
                }
                _ => {}
            }
            s.received.push(input);
        }
        if s.dead {
            return Ok(StopReason::Exited {
                reached: at(s.reached),
                reason: ExitReason::Crashed,
            });
        }
        let end = upto.as_nanos();
        while s.sends.front().is_some_and(|(t, _)| *t <= end) {
            let (t, payload) = s.sends.pop_front().unwrap();
            s.reached = s.reached.max(t.max(now));
            c.outbox
                .emit(Output {
                    at: at(t.max(now)),
                    payload,
                })
                .unwrap();
        }
        if s.busy {
            if end > now {
                let width = [1, 7, 31, 2, 33][s.fills % 5];
                let mut bytes = vec![0; width];
                c.rng.fill_bytes(&mut bytes);
                s.entropy.extend_from_slice(&bytes);
                s.fills += 1;
                c.outbox
                    .emit(Output {
                        at: upto,
                        payload: output(&bytes),
                    })
                    .unwrap();
            }
            s.reached = end;
            Ok(StopReason::BoundaryReached)
        } else {
            Ok(StopReason::Blocked {
                reached: at(s.reached),
                deadline: s.sends.front().map(|(t, _)| at(*t)),
            })
        }
    }
    fn snapshot(&mut self) -> Result<Saved, Io> {
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.state.fail_capture {
            Err(io("capture failure"))
        } else {
            Ok(Saved(self.state.clone(), self.saved_drops.clone()))
        }
    }
}
pub(in crate::driver) fn spec(index: usize, busy: bool, sends: Vec<(u64, Response)>) -> State {
    State {
        name: format!("fixture-{index}"),
        reached: 0,
        busy,
        dead: false,
        entropy: vec![],
        fills: 0,
        sends: sends.into(),
        received: vec![],
        fail_capture: false,
        fail_run: false,
    }
}
pub(in crate::driver) fn build(specs: Vec<State>, spawn: Vec<State>, seeded: bool) -> Driver<Mock> {
    build_network(specs, spawn, seeded, false)
}
pub(in crate::driver) fn build_network(
    specs: Vec<State>,
    spawn: Vec<State>,
    seeded: bool,
    networked: bool,
) -> Driver<Mock> {
    build_network_workers(specs, spawn, seeded, networked, 1)
}
pub(in crate::driver) fn build_network_workers(
    specs: Vec<State>,
    spawn: Vec<State>,
    seeded: bool,
    networked: bool,
    workers: usize,
) -> Driver<Mock> {
    let definition = |index| {
        let mut d = fixture::definition(index);
        if networked {
            d.network = Some(
                crate::definition::NetworkInterface::new(
                    crate::definition::MacAddress::new(mac(index)).unwrap(),
                    std::net::Ipv4Addr::new(10, 0, 0, (index + 1) as u8),
                    24,
                )
                .unwrap(),
            );
        }
        d
    };
    let config = if networked {
        NetworkConfig::new(Duration::from_nanos(5), Duration::from_nanos(3), 100_000).unwrap()
    } else {
        NetworkConfig::default()
    };
    let mut definitions: Vec<_> = (0..specs.len()).map(definition).collect();
    let oracle = definitions.remove(0);
    let spawnable = (specs.len()..specs.len() + spawn.len())
        .map(definition)
        .collect();
    let all: BTreeMap<_, _> = specs
        .into_iter()
        .chain(spawn)
        .map(|s| (s.name.clone(), s))
        .collect();
    let factory = move |d: &MachineDefinition| Ok(Mock::new(all[&d.name].clone()));
    if seeded {
        Driver::new_seeded(
            at(0),
            Duration::from_nanos(10),
            workers,
            51,
            || Ok(Context(None)),
            oracle,
            definitions,
            spawnable,
            config,
            factory,
        )
        .unwrap()
    } else {
        Driver::new(
            at(0),
            Duration::from_nanos(10),
            workers,
            SeededRng::new(51),
            || Ok(Context(None)),
            oracle,
            definitions,
            spawnable,
            config,
            factory,
        )
        .unwrap()
    }
}
pub(in crate::driver) fn mac(index: usize) -> [u8; 6] {
    [2, 0, 0, 0, 0, (index + 1) as u8]
}
pub(in crate::driver) fn frame(to: [u8; 6], tag: u8) -> Response {
    let mut bytes = to.to_vec();
    bytes.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0x88, 0xb5, tag]);
    Response::TransmitFrame { bytes }
}
/// A UDP DNS query for an A record, sent to the simulated nameserver.
pub(in crate::driver) fn dns_query(mac: [u8; 6], ip: [u8; 4], name: &str, id: u16) -> Vec<u8> {
    use seedbox_engine_core::dns;
    let mut message = id.to_be_bytes().to_vec();
    message.extend_from_slice(&[1, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    for label in name.split('.') {
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.extend_from_slice(&[0, 0, 1, 0, 1]);
    let mut bytes = dns::MAC.to_vec();
    bytes.extend_from_slice(&mac);
    bytes.extend_from_slice(&[8, 0, 0x45, 0]);
    bytes.extend_from_slice(&((28 + message.len()) as u16).to_be_bytes());
    bytes.extend_from_slice(&[0, 0, 0, 0, 64, 17, 0, 0]);
    bytes.extend_from_slice(&ip);
    bytes.extend_from_slice(&dns::ADDRESS);
    bytes.extend_from_slice(&[0xc0, 0x00, 0, 53]);
    bytes.extend_from_slice(&((8 + message.len()) as u16).to_be_bytes());
    bytes.extend_from_slice(&[0, 0]);
    bytes.extend_from_slice(&message);
    bytes
}
/// A nameserver reply's (ID, RCODE, A address if any).
pub(in crate::driver) fn dns_reply(bytes: &[u8]) -> (u16, u8, Option<[u8; 4]>) {
    assert_eq!(bytes[6..12], seedbox_engine_core::dns::MAC);
    let message = &bytes[42..];
    let address = (message[7] == 1).then(|| message[message.len() - 4..].try_into().unwrap());
    (
        u16::from_be_bytes([message[0], message[1]]),
        message[3] & 15,
        address,
    )
}
pub(in crate::driver) fn fresh(d: &MachineDefinition) -> Result<Mock, Io> {
    let index = d.name.strip_prefix("fixture-").unwrap().parse().unwrap();
    Ok(Mock::new(spec(index, true, vec![(0, Response::Ready)])))
}
pub(in crate::driver) fn restore(c: &Checkpoint<Saved>, workers: usize) -> Driver<Mock> {
    Driver::from_checkpoint(c, workers, || Ok(Context(None)), Mock::restore, fresh).unwrap()
}
pub(in crate::driver) fn status(d: &mut Driver<Mock>) -> PlaybackStatus {
    d.with_barrier(|b| Ok(b.playback())).unwrap()
}
pub(in crate::driver) fn states(d: &Driver<Mock>) -> Vec<State> {
    d.execution
        .machines
        .iter()
        .map(|e| e.machine.as_ref().unwrap().state.clone())
        .collect()
}
/// Compare a live continuation against an imported one with the same NEW
/// observer session. Test-only: production capture leaves source history intact.
/// Reset no execution state, native state, queues, clocks or RNG cursors.
pub(in crate::driver) fn reference_observer_session(d: &mut Driver<Mock>) {
    d.drain_observations();
    d.observations = ObservationState::default();
}
pub(in crate::driver) fn advance(d: &mut Driver<Mock>, end: u64) -> Vec<Event> {
    let mut events = d.drain_events();
    for _ in 0..1000 {
        if d.now() >= at(end) || d.execution.finished.is_some() {
            return events;
        }
        let outcome = d.step(at(end)).unwrap();
        events.extend(d.drain_events());
        assert!(!matches!(
            outcome,
            StepOutcome::Waiting { .. }
                | StepOutcome::BeforeFault { .. }
                | StepOutcome::Stopped { .. }
        ));
    }
    panic!("mock continuation did not finish");
}

wire_struct!(State {
    name,
    reached,
    busy,
    dead,
    entropy,
    fills,
    sends,
    received,
    fail_capture,
    fail_run
});
pub(in crate::driver) fn limits() -> DecodeLimits {
    DecodeLimits {
        encoded_bytes: 32 << 20,
        allocation_bytes: 64 << 20,
        collection_items: 4 << 20,
        total_items: 32 << 20,
        string_bytes: 1 << 20,
        blob_bytes: 8 << 20,
        depth: 128,
        archive_entries: 10000,
        archive_path_bytes: 1 << 20,
        archive_expanded_bytes: 8 << 20,
    }
}
pub(in crate::driver) type Store = BTreeMap<[u8; 32], Vec<u8>>;
pub(in crate::driver) fn artifact(c: Checkpoint<Saved>) -> (Vec<u8>, Store) {
    let mut store = Store::new();
    let refs = c
        .try_map_snapshots(|saved| {
            let mut e = Encoder { output: Vec::new() };
            saved.0.put(&mut e).unwrap();
            let bytes = e.output;
            let digest = Sha256::digest(&bytes).into();
            let reference = SnapshotReference::new(digest, bytes.len() as u64);
            store.insert(digest, bytes);
            Ok::<_, Io>(reference)
        })
        .unwrap();
    let mut bytes = Vec::new();
    refs.write_to(&mut bytes).unwrap();
    (bytes, store)
}
pub(in crate::driver) fn decode(bytes: &[u8], store: &Store) -> Checkpoint<Saved> {
    let refs = Checkpoint::read_from(&mut &bytes[..], limits()).unwrap();
    let mut encoded = Vec::new();
    refs.write_to(&mut encoded).unwrap();
    assert_eq!(encoded, bytes);
    refs.try_map_snapshots(|r| {
        let bytes = &store[r.digest()];
        assert_eq!(r.encoded_length(), bytes.len() as u64);
        assert_eq!(r.digest(), &<[u8; 32]>::from(Sha256::digest(bytes)));
        let mut d = Decoder::new(&bytes[..], bytes.len() as u64, limits());
        let state = State::get(&mut d).unwrap();
        assert_eq!(d.remaining, 0);
        Ok::<_, Io>(Saved(state, None))
    })
    .unwrap()
}

pub(in crate::driver) mod network_fixture;
