//! Backend-neutral real-Linux network scenario: two machines run the shared
//! `network-probe` (tests/fixtures/network-probe.c) as server and client and
//! exchange ARP, UDP and TCP through frames this harness routes itself.
//!
//! The harness is a minimal stand-in for the Driver's network: both machines
//! advance in lockstep quanta of `LATENCY`; a frame sent at `s` is offered to
//! its receiver at the first quantum start at or after `s + LATENCY`, so it
//! never arrives in the receiver's past. Broadcast reaches the other machine;
//! unicast reaches its MAC's owner; anything else is dropped. Only machine
//! contracts are exercised here, not Driver routing.
use crate::{
    clock::SimulationClock,
    definition::{
        DiskImage, FilesystemArchive, Launch, MacAddress, MachineDefinition, NetworkInterface,
        PreparedApplication, Resources,
    },
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Input, Outbox, Request, Response},
    rng::SeededRng,
};
use seedbox_test_time::HangGuard;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, fmt::Debug, net::Ipv4Addr, time::Duration};

/// Minimum one-way delay, and the lockstep quantum.
pub const LATENCY: u64 = 1_000_000;
/// Simulated bound for the whole exchange.
const LIMIT: u64 = 120_000_000_000;
/// Host diagnostic only; it cannot interrupt a call that never returns.
const HOST_LIMIT: Duration = Duration::from_secs(900);
pub const PORT: &str = "7000";
pub const BYTES: &str = "262144";

pub fn mac(index: u8) -> [u8; 6] {
    [2, 0, 0, 0, 0, index + 1]
}

/// The address of the machine at `index` in [`definitions`].
pub fn ipv4(index: u8) -> Ipv4Addr {
    Ipv4Addr::new(10, 0, 0, index + 1)
}

/// One Ethernet/IPv4/UDP frame from `from` to `to`, each a MAC, address and
/// port. The IPv4 header checksum is correct; the UDP checksum is 0, which
/// IPv4 permits. Test traffic that no probe mode sends, such as an oversized
/// token.
pub fn udp_frame(
    from: ([u8; 6], Ipv4Addr, u16),
    to: ([u8; 6], Ipv4Addr, u16),
    payload: &[u8],
) -> Vec<u8> {
    let udp_length = u16::try_from(8 + payload.len()).unwrap();
    let total = udp_length.checked_add(20).unwrap();
    let mut frame = Vec::with_capacity(14 + total as usize);
    frame.extend(to.0);
    frame.extend(from.0);
    frame.extend(0x0800u16.to_be_bytes());
    let header = frame.len();
    frame.extend([0x45, 0]);
    frame.extend(total.to_be_bytes());
    frame.extend([0, 0, 0, 0]); // Identification, flags and fragment offset.
    frame.extend([64, 17, 0, 0]); // TTL, UDP, checksum placeholder.
    frame.extend(from.1.octets());
    frame.extend(to.1.octets());
    let sum = frame[header..header + 20]
        .chunks(2)
        .map(|word| u32::from(u16::from_be_bytes([word[0], word[1]])))
        .sum::<u32>();
    let folded = (sum & 0xffff) + (sum >> 16);
    let checksum = !(((folded & 0xffff) + (folded >> 16)) as u16);
    frame[header + 10..header + 12].copy_from_slice(&checksum.to_be_bytes());
    frame.extend(from.2.to_be_bytes());
    frame.extend(to.2.to_be_bytes());
    frame.extend(udp_length.to_be_bytes());
    frame.extend([0, 0]);
    frame.extend(payload);
    assert!(crate::pv::frame_in_bounds(&frame), "{} bytes", frame.len());
    frame
}

/// A root application running the probe with `arguments` on a NIC.
pub fn definition(
    name: &str,
    probe: &[u8],
    arguments: &[&str],
    index: u8,
    memory_bytes: u64,
) -> MachineDefinition {
    definition_with_disk(name, probe, arguments, index, memory_bytes, None)
}

/// [`definition`] with an optional single disk.
pub fn definition_with_disk(
    name: &str,
    probe: &[u8],
    arguments: &[&str],
    index: u8,
    memory_bytes: u64,
    disk: Option<DiskImage>,
) -> MachineDefinition {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(probe.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", probe).unwrap();
    let mut argv = vec!["/application".to_owned()];
    argv.extend(arguments.iter().map(|argument| argument.to_string()));
    MachineDefinition {
        name: name.into(),
        resources: Resources { memory_bytes },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv,
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk,
        network: Some(
            NetworkInterface::new(MacAddress::new(mac(index)).unwrap(), ipv4(index), 24).unwrap(),
        ),
    }
}

/// Server at 10.0.0.1 and client at 10.0.0.2.
pub fn definitions(probe: &[u8], memory_bytes: u64) -> [MachineDefinition; 2] {
    [
        definition("server", probe, &["server", PORT], 0, memory_bytes),
        definition(
            "client",
            probe,
            &["client", "10.0.0.1", PORT, BYTES],
            1,
            memory_bytes,
        ),
    ]
}

/// One routed frame: send time, sender, receiver, offer time and SHA-256.
pub type Routed = (u64, usize, Option<usize>, Option<u64>, [u8; 32]);

/// Disk size of the recording server: a zeroed 1 MiB image.
pub const DISK_BYTES: u64 = 1 << 20;

/// [`definitions`] with the server recording progress on its own zeroed disk
/// (`server-disk`); the client is unchanged.
pub fn disk_definitions(probe: &[u8], memory_bytes: u64) -> [MachineDefinition; 2] {
    let [_, client] = definitions(probe, memory_bytes);
    [
        definition_with_disk(
            "server",
            probe,
            &["server-disk", PORT],
            0,
            memory_bytes,
            Some(DiskImage::zeroed(DISK_BYTES).unwrap()),
        ),
        client,
    ]
}

fn fnv(bytes: impl IntoIterator<Item = u8>) -> u64 {
    bytes.into_iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ byte as u64).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The `server-disk` record of the client's stream: `BYTES:CHECKSUM`, the
/// FNV-1a 64 checksum of the probe's fixed pattern, computed independently.
pub fn tcp_record() -> String {
    let bytes: usize = BYTES.parse().unwrap();
    let pattern = (0..bytes).map(|at| (at.wrapping_mul(131) + (at >> 8).wrapping_mul(7) + 1) as u8);
    format!("{bytes}:{:016x}", fnv(pattern))
}

/// The `server-disk` record of the client's UDP token: `LENGTH:CHECKSUM` of
/// the probe's datagram, its NUL terminator included, computed independently.
pub fn token_record() -> String {
    let token = b"seedbox network probe\0";
    format!("{}:{:016x}", token.len(), fnv(token.iter().copied()))
}

/// Everything observable about one exchange, for repeatability checks.
#[derive(Debug, PartialEq, Eq)]
pub struct Transcript {
    /// Application stdout of each machine.
    pub output: [String; 2],
    /// Every frame either machine transmitted, in publication order.
    pub frames: Vec<Routed>,
    /// When the exchange completed, not necessarily when the client printed
    /// `NETPROBE done`: a manual [`Exchange`] stores the end of the quantum in
    /// which it first saw that line; a Driver adapter stores the client's
    /// exit event time.
    pub done: u64,
}

struct Peer<M> {
    machine: M,
    clock: SimulationClock,
    rng: SeededRng,
    inbox: VecDeque<Input>,
    output: String,
    console: String,
}

/// Two machines and the frames between them, advanced in lockstep quanta.
pub struct Exchange<M> {
    peers: [Peer<M>; 2],
    /// Undelivered frames: offer time, receiver, bytes.
    in_flight: Vec<(u64, usize, Vec<u8>)>,
    frames: Vec<Routed>,
    now: u64,
    guard: HangGuard,
}

impl<M: Machine<Context = ()>> Exchange<M>
where
    M::Error: Debug,
{
    pub fn new(machines: [M; 2]) -> Self {
        Self {
            peers: machines.map(|machine| Peer {
                machine,
                clock: SimulationClock::new(0),
                rng: SeededRng::new(41),
                inbox: VecDeque::new(),
                output: String::new(),
                console: String::new(),
            }),
            in_flight: Vec::new(),
            frames: Vec::new(),
            now: 0,
            guard: HangGuard::new(
                "native network exchange must produce a response",
                HOST_LIMIT,
            ),
        }
    }
    pub fn now(&self) -> u64 {
        self.now
    }
    pub fn output(&self, index: usize) -> &str {
        &self.peers[index].output
    }
    pub fn machines(&mut self) -> [&mut M; 2] {
        let [a, b] = &mut self.peers;
        [&mut a.machine, &mut b.machine]
    }
    /// The same external state (outputs, RNGs, frames in flight) around
    /// other machines, such as ones restored from snapshots of these.
    pub fn branch<N: Machine<Context = ()>>(&self, machines: [N; 2]) -> Exchange<N>
    where
        N::Error: Debug,
    {
        let mut machines = machines.into_iter();
        Exchange {
            peers: std::array::from_fn(|index| {
                let peer = &self.peers[index];
                Peer {
                    machine: machines.next().unwrap(),
                    clock: SimulationClock::new(self.now),
                    rng: peer.rng.clone(),
                    inbox: peer.inbox.clone(),
                    output: peer.output.clone(),
                    console: peer.console.clone(),
                }
            }),
            in_flight: self.in_flight.clone(),
            frames: self.frames.clone(),
            now: self.now,
            guard: HangGuard::new(
                "restored network exchange must produce a response",
                HOST_LIMIT,
            ),
        }
    }
    /// Queue `request` for machine `index` at the current time.
    pub fn inject(&mut self, index: usize, request: Request) {
        self.peers[index].inbox.push_back(Input {
            at: Timestamp::from_nanos(self.now),
            payload: request,
        });
    }
    /// Frames sent but not yet offered to their receiver: the external
    /// network state a branch carries over.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }
    pub fn done(&self) -> bool {
        self.peers[1].output.contains("NETPROBE done")
    }
    /// Advance both machines by one quantum.
    pub fn step(&mut self) {
        let now = self.now;
        assert!(
            now < LIMIT,
            "no exchange within the simulated bound:\n{:?}\n{:?}\n{}",
            self.peers[0].output,
            self.peers[1].output,
            self.peers[1].console
        );
        self.guard
            .remaining()
            .expect("network exchange infrastructure hang");
        // Diagnostics can arrive before the application outcome in a later
        // run. Validate that typed outcome below, including its exit code;
        // negative disk cases require both the diagnostic and exit 70.
        // Offer due frames in arrival, then emission, order.
        self.in_flight.sort_by_key(|(arrival, ..)| *arrival);
        let due = self
            .in_flight
            .partition_point(|(arrival, ..)| *arrival <= now);
        for (_, to, bytes) in self.in_flight.drain(..due) {
            self.peers[to].inbox.push_back(Input {
                at: Timestamp::from_nanos(now),
                payload: Request::ReceiveFrame { bytes },
            });
        }
        let upto = now + LATENCY;
        for index in 0..2 {
            let peer = &mut self.peers[index];
            peer.clock.set(now);
            let mut staged = Vec::new();
            let stop = peer
                .machine
                .run(
                    &mut RunContext {
                        worker: &mut (),
                        clock: &peer.clock,
                        rng: &mut peer.rng,
                        execution: Default::default(),
                        inbox: Inbox::new(&mut peer.inbox, Timestamp::from_nanos(now)),
                        outbox: Outbox::new(
                            &mut staged,
                            Timestamp::from_nanos(now),
                            Timestamp::from_nanos(upto),
                        ),
                    },
                    Timestamp::from_nanos(upto),
                )
                .unwrap();
            assert!(peer.inbox.is_empty(), "every due input is taken");
            for output in staged {
                let at = output.at.as_nanos();
                match output.payload {
                    Response::ApplicationOutput { bytes, .. } => {
                        peer.output.push_str(&String::from_utf8_lossy(&bytes))
                    }
                    Response::BootConsole { bytes } => {
                        peer.console.push_str(&String::from_utf8_lossy(&bytes))
                    }
                    // Only the client finishes, successfully, after its report.
                    Response::ApplicationOutcome { code } => assert!(
                        index == 1 && code == 0 && peer.output.ends_with("NETPROBE done\n"),
                        "{index} probe exited {code}: {:?}",
                        peer.output
                    ),
                    Response::TransmitFrame { bytes } => {
                        assert!(crate::pv::frame_in_bounds(&bytes));
                        let to = route(index, &bytes);
                        let arrival = to.map(|_| (at + LATENCY).next_multiple_of(LATENCY));
                        self.frames
                            .push((at, index, to, arrival, Sha256::digest(&bytes).into()));
                        if let (Some(to), Some(arrival)) = (to, arrival) {
                            self.in_flight.push((arrival, to, bytes));
                        }
                    }
                    _ => (),
                }
            }
            if let StopReason::Exited { .. } = stop {
                panic!("{index} exited: {:?}\n{}", peer.output, peer.console);
            }
        }
        self.now = upto;
    }
    /// Step until `condition` holds.
    pub fn until(&mut self, condition: impl Fn(&Self) -> bool) {
        while !condition(self) {
            self.step();
        }
    }
    /// Run until the client prints `NETPROBE done`.
    pub fn finish(mut self) -> Transcript {
        self.until(Self::done);
        let [server, client] = self.peers;
        Transcript {
            output: [server.output, client.output],
            frames: self.frames,
            done: self.now,
        }
    }
}

/// Run the whole exchange from boot.
pub fn exchange<M: Machine<Context = ()>>(machines: [M; 2]) -> Transcript
where
    M::Error: Debug,
{
    Exchange::new(machines).finish()
}

/// The receiver of a frame from `from`, by its destination MAC.
fn route(from: usize, bytes: &[u8]) -> Option<usize> {
    let destination: [u8; 6] = bytes[..6].try_into().unwrap();
    let other = 1 - from;
    (destination == [0xff; 6] || destination == mac(other as u8)).then_some(other)
}

/// Semantic assertions shared by every backend.
pub fn check(transcript: &Transcript) {
    let [server, _] = &transcript.output;
    assert_eq!(server, "NETPROBE ready port=7000\n", "{transcript:?}");
    check_client(transcript);
}

/// [`check`] for a `server-disk` exchange from its first boot: every record
/// is written before the network step it describes.
pub fn check_disk(transcript: &Transcript) {
    let [server, _] = &transcript.output;
    assert_eq!(
        *server,
        format!(
            "NETPROBE disk boot=1 token=none tcp=none\n\
             NETPROBE ready port=7000\n\
             NETPROBE disk token={}\n\
             NETPROBE disk tcp={}\n",
            token_record(),
            tcp_record()
        ),
        "{transcript:?}"
    );
    check_client(transcript);
}

/// The client's report and the traffic both ways.
pub fn check_client(transcript: &Transcript) {
    let [_, client] = &transcript.output;
    let lines: Vec<&str> = client.lines().collect();
    assert_eq!(lines.len(), 4, "{client:?}");
    assert!(
        lines[0].starts_with("NETPROBE udp ok attempts="),
        "{client:?}"
    );
    assert_eq!(
        lines[1],
        "NETPROBE arp ok address=10.0.0.1 mac=02:00:00:00:00:01"
    );
    assert!(
        lines[2].starts_with(&format!("NETPROBE tcp ok bytes={BYTES} connects=")),
        "{client:?}"
    );
    assert_eq!(lines[3], "NETPROBE done");
    // Both sides spoke: ARP requests are broadcast, replies unicast.
    let routed = |from, to| {
        transcript
            .frames
            .iter()
            .filter(|frame| frame.1 == from && frame.2 == Some(to))
            .count()
    };
    assert!(routed(0, 1) > 0 && routed(1, 0) > 0);
    // TCP carried at least the echoed payload in each direction.
    assert!(routed(1, 0) * 1514 >= 262_144 && routed(0, 1) * 1514 >= 262_144);
}

// PV and TCG exercise the same network assertions.
// Only construction and restore vary; these test-only callbacks keep those
// differences out of the scenario bodies, as `disk_scenario::Backend` does.
/// Construction and restore for the shared direct-`Machine` scenarios.
pub struct Backend<M: Machine> {
    /// Log prefix naming the backend.
    pub name: &'static str,
    /// A new factory over the backend's assets.
    pub factory: fn() -> super::disk_scenario::Create<M>,
    /// Reconstruct a machine from its snapshot.
    pub restore: fn(&M::Snapshot) -> Result<M, M::Error>,
    /// The static `network-probe` guest application.
    pub probe: std::path::PathBuf,
    pub memory_bytes: u64,
}

impl<M: Machine<Context = ()>> Backend<M>
where
    M::Error: Debug,
{
    fn probe(&self) -> Vec<u8> {
        std::fs::read(&self.probe).unwrap()
    }
    /// Build the server and client of `definitions` with one new factory.
    fn create(&self, definitions: [MachineDefinition; 2]) -> [M; 2] {
        let mut factory = (self.factory)();
        definitions.map(|definition| factory(&definition).unwrap())
    }
    fn machines(&self) -> [M; 2] {
        self.create(definitions(&self.probe(), self.memory_bytes))
    }
    fn disk_machines(&self) -> [M; 2] {
        self.create(disk_definitions(&self.probe(), self.memory_bytes))
    }
    /// Snapshot both machines and restore each from its snapshot.
    fn restored(&self, exchange: &mut Exchange<M>) -> [M; 2] {
        exchange.machines().map(|machine| {
            let snapshot = machine.snapshot().unwrap();
            (self.restore)(&snapshot).unwrap()
        })
    }
}

/// Two exchanges from boot are identical and pass [`check`].
pub fn plain_exchange_is_repeatable<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let first = exchange(backend.machines());
    check(&first);
    let second = exchange(backend.machines());
    assert_eq!(first, second);
    eprintln!(
        "{} exchange: {} frames, done at {} ns, client {:?}",
        backend.name,
        first.frames.len(),
        first.done,
        first.output[1]
    );
}

/// Snapshots taken mid-stream, with frames in flight and NIC state in the
/// machines, continue exactly like the uninterrupted machines.
pub fn plain_snapshot_continuation<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let mut source = Exchange::new(backend.machines());
    // Mid-TCP: UDP is done and the stream has started.
    source.until(|e| e.output(1).contains("udp ok"));
    let resume = source.now() + 20 * LATENCY;
    source.until(|e| e.now() >= resume);
    assert!(!source.output(1).contains("tcp ok"), "still mid-TCP");
    assert!(source.in_flight() > 0, "frames in flight at capture");
    eprintln!("frames in flight at capture: {}", source.in_flight());
    let restored = backend.restored(&mut source);
    let branch = source.branch(restored);
    let original = source.finish();
    check(&original);
    assert_eq!(branch.finish(), original);
}

/// A server crashed and rebooted while the client probes: frames for the
/// ended boot are dropped, the new boot's NIC starts empty, and the client's
/// retries reach it.
pub fn server_crash_and_reboot<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let run = || {
        let mut exchange = Exchange::new(backend.machines());
        exchange.until(|e| e.output(0).contains("ready"));
        // The client has not reached the server yet; its probes now meet a
        // crashed, then rebooting, server.
        assert!(!exchange.output(1).contains("udp ok"));
        exchange.inject(0, Request::Crash);
        exchange.inject(0, Request::Reboot);
        exchange.finish()
    };
    let first = run();
    assert_eq!(
        first.output[0],
        "NETPROBE ready port=7000\nNETPROBE ready port=7000\n"
    );
    let client: Vec<&str> = first.output[1].lines().collect();
    assert_eq!(client.len(), 4, "{client:?}");
    assert!(client[2].starts_with("NETPROBE tcp ok bytes=262144"));
    assert_eq!(client[3], "NETPROBE done");
    eprintln!(
        "{} crash/reboot: done at {} ns, client {:?}",
        backend.name, first.done, client
    );
    assert_eq!(run(), first);
}

/// The server fsyncs a disk record before each network step it describes,
/// so disk I/O interleaves with live NIC traffic; repeated runs agree.
pub fn disk_exchange_is_repeatable<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let first = exchange(backend.disk_machines());
    check_disk(&first);
    assert_eq!(exchange(backend.disk_machines()), first);
    eprintln!(
        "{} disk exchange: {} frames, done at {} ns",
        backend.name,
        first.frames.len(),
        first.done
    );
}

/// Snapshots mid-TCP of a server with a disk, with the external frames,
/// RNG streams and clock carried over, continue like the source.
pub fn disk_snapshot_continuation<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let mut source = Exchange::new(backend.disk_machines());
    source.until(|e| e.output(0).contains("disk token="));
    let resume = source.now() + 20 * LATENCY;
    source.until(|e| e.now() >= resume);
    assert!(!source.output(0).contains("disk tcp="), "still mid-TCP");
    assert!(source.in_flight() > 0, "frames in flight at capture");
    eprintln!("frames in flight at capture: {}", source.in_flight());
    let restored = backend.restored(&mut source);
    let branch = source.branch(restored);
    let original = source.finish();
    check_disk(&original);
    assert_eq!(branch.finish(), original);
}

/// A server crashed and rebooted after Ready, or after recording the UDP
/// token, reports the durable records of its earlier boot and completes.
pub fn disk_server_crash_keeps_flushed_records<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let run = |marker: &'static str| {
        let mut exchange = Exchange::new(backend.disk_machines());
        exchange.until(|e| e.output(0).contains(marker));
        exchange.inject(0, Request::Crash);
        exchange.inject(0, Request::Reboot);
        exchange.finish()
    };
    let token = format!("NETPROBE disk token={}", token_record());
    let tcp = format!("NETPROBE disk tcp={}", tcp_record());
    // Each token line must be the client's exact datagram. Their number is
    // not a protocol contract: the server records every datagram it reads
    // before echoing it, the client sends one per attempt and stops at the
    // first echo it reads, and datagrams its kernel queued behind ARP arrive
    // together. Some may still be unread when the stream ends, so the count
    // lies between one and the client's attempts.
    let attempts = |transcript: &Transcript| -> usize {
        transcript.output[1]
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("NETPROBE udp ok attempts="))
            .unwrap()
            .parse()
            .unwrap()
    };
    let after_ready = run("NETPROBE ready");
    check_client(&after_ready);
    let server: Vec<&str> = after_ready.output[0].lines().collect();
    assert_eq!(
        server[..4],
        [
            "NETPROBE disk boot=1 token=none tcp=none",
            "NETPROBE ready port=7000",
            "NETPROBE disk boot=2 token=none tcp=none",
            "NETPROBE ready port=7000",
        ]
    );
    let tokens = &server[4..server.len() - 1];
    assert!(tokens.iter().all(|line| *line == token), "{server:?}");
    assert!((1..=attempts(&after_ready)).contains(&tokens.len()));
    assert_eq!(*server.last().unwrap(), tcp);
    eprintln!(
        "{} disk crash after ready: {} tokens, client {:?}",
        backend.name,
        tokens.len(),
        after_ready.output[1]
    );
    assert_eq!(run("NETPROBE ready"), after_ready);

    // The token's bytes, not just its length, survive the crash.
    let after_token = run("NETPROBE disk token=");
    check_client(&after_token);
    let server: Vec<&str> = after_token.output[0].lines().collect();
    assert_eq!(
        server[..5],
        [
            "NETPROBE disk boot=1 token=none tcp=none",
            "NETPROBE ready port=7000",
            &token,
            &format!("NETPROBE disk boot=2 token={} tcp=none", token_record()),
            "NETPROBE ready port=7000",
        ]
    );
    let tokens: Vec<_> = server
        .iter()
        .filter(|line| line.starts_with("NETPROBE disk token="))
        .collect();
    assert!(tokens.iter().all(|line| **line == token), "{server:?}");
    assert!((1..=attempts(&after_token)).contains(&tokens.len()));
    assert_eq!(server[5..server.len() - 1].len(), tokens.len() - 1);
    assert_eq!(*server.last().unwrap(), tcp);
    assert_eq!(run("NETPROBE disk token="), after_token);
    eprintln!(
        "{} disk crash after token: {:?}",
        backend.name, after_token.output
    );
}

/// The exchange with the server's disk starting as `image`.
fn exchange_from_image<M: Machine<Context = ()>>(backend: &Backend<M>, image: Vec<u8>)
where
    M::Error: Debug,
{
    let probe = backend.probe();
    let [_, client] = definitions(&probe, backend.memory_bytes);
    let server = definition_with_disk(
        "server",
        &probe,
        &["server-disk", PORT],
        0,
        backend.memory_bytes,
        Some(DiskImage::from_bytes(image).unwrap()),
    );
    Exchange::new(backend.create([server, client])).finish();
}

/// An existing nonzero record that is not the probe's fails the server
/// before Ready; it is never reformatted. Entries expect the panic
/// `probe exited 70: "NETPROBE fail disk boot record errno=74`.
pub fn disk_rejects_a_malformed_boot_record<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let mut image = vec![0; DISK_BYTES as usize];
    image[..8].copy_from_slice(b"NOTPROBE");
    exchange_from_image(backend, image);
}

/// A boot count at its maximum fails rather than wrapping to zero. Entries
/// expect the panic `probe exited 70: "NETPROBE fail disk boot count errno=75`.
pub fn disk_rejects_an_exhausted_boot_count<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let mut image = vec![0; DISK_BYTES as usize];
    image[..8].copy_from_slice(b"SBXNETD1");
    image[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    exchange_from_image(backend, image);
}

/// A datagram longer than a token record holds, sent after Ready, fails the
/// server with EOVERFLOW before it records or echoes anything. No probe mode
/// sends one, so the frame is crafted; without the guard the exchange would
/// finish normally and the entry's expected panic would not occur.
pub fn disk_rejects_an_oversized_token<M: Machine<Context = ()>>(backend: &Backend<M>)
where
    M::Error: Debug,
{
    let mut exchange = Exchange::new(backend.disk_machines());
    exchange.until(|e| e.output(0).contains("NETPROBE ready"));
    let bytes = udp_frame(
        (mac(1), ipv4(1), 40000),
        (mac(0), ipv4(0), 7000),
        &[0x5a; 501],
    );
    exchange.inject(0, Request::ReceiveFrame { bytes });
    exchange.finish();
}

#[test]
fn udp_frames_carry_a_valid_ipv4_header_and_the_whole_payload() {
    let payload: Vec<u8> = (0..501).map(|i| i as u8).collect();
    let frame = udp_frame((mac(1), ipv4(1), 40000), (mac(0), ipv4(0), 7000), &payload);
    assert_eq!(frame.len(), 14 + 20 + 8 + 501);
    assert_eq!(frame[..6], mac(0));
    assert_eq!(frame[6..12], mac(1));
    assert_eq!(frame[12..14], [0x08, 0x00]);
    let ip = &frame[14..34];
    assert_eq!(ip[0], 0x45);
    assert_eq!(u16::from_be_bytes([ip[2], ip[3]]), 20 + 8 + 501);
    assert_eq!(ip[9], 17);
    assert_eq!(ip[12..16], [10, 0, 0, 2]);
    assert_eq!(ip[16..20], [10, 0, 0, 1]);
    // A header including its checksum sums to all ones.
    let sum = ip
        .chunks(2)
        .map(|word| u32::from(u16::from_be_bytes([word[0], word[1]])))
        .sum::<u32>();
    assert_eq!((sum & 0xffff) + (sum >> 16), 0xffff);
    let udp = &frame[34..42];
    assert_eq!(u16::from_be_bytes([udp[0], udp[1]]), 40000);
    assert_eq!(u16::from_be_bytes([udp[2], udp[3]]), 7000);
    assert_eq!(u16::from_be_bytes([udp[4], udp[5]]), 8 + 501);
    assert_eq!(udp[6..8], [0, 0]);
    assert_eq!(frame[42..], payload);
}

/// One machine as a Driver lane keeps it: its stop, the frames routed to it
/// and not yet taken, and what it has shown.
struct Lane<M> {
    machine: M,
    clock: SimulationClock,
    rng: SeededRng,
    stopped: StopReason,
    reached: u64,
    /// Due inputs, at or before the next run's start.
    inbox: VecDeque<Input>,
    /// Routed frames not yet offered: arrival time and bytes, in order.
    arrivals: VecDeque<(u64, Vec<u8>)>,
    output: String,
    console: String,
}

/// What one schedule of [`timed_inbox_matches_one_run_per_arrival`] showed
/// at one barrier.
#[derive(PartialEq, Eq)]
struct Look {
    at: u64,
    output: [String; 2],
    console: [String; 2],
    frames: Vec<Routed>,
    witness: [Option<crate::machine::ExecutionEvidence>; 2],
    state: Option<[Option<crate::machine::StateEvidence>; 2]>,
}

/// Runs, frames delivered inside runs, and the looks of one schedule.
struct Schedule {
    looks: Vec<Look>,
    runs: usize,
    timed: usize,
}

/// Where two looks differ.
fn look_differences(own: &Look, other: &Look) -> String {
    let mut found = Vec::new();
    for index in 0..2 {
        if own.output[index] != other.output[index] {
            found.push(format!(
                "{index} output: {:?} against {:?}",
                own.output[index], other.output[index]
            ));
        }
        if own.console[index] != other.console[index] {
            found.push(format!("{index} console"));
        }
        if own.witness[index] != other.witness[index] {
            found.push(format!("{index} witness"));
        }
        if let (Some(a), Some(b)) = (&own.state, &other.state) {
            if let (Some(a), Some(b)) = (&a[index], &b[index]) {
                for ((name, x), (_, y)) in a.parts.iter().zip(&b.parts) {
                    if x != y {
                        found.push(format!("{index} state part {name}"));
                    }
                }
                if a.page_hashes != b.page_hashes {
                    found.push(format!("{index} RAM pages"));
                }
            }
        }
    }
    if own.frames != other.frames {
        let first = own
            .frames
            .iter()
            .zip(&other.frames)
            .position(|(a, b)| a != b);
        found.push(format!(
            "frames ({} against {}), first difference at {first:?}",
            own.frames.len(),
            other.frames.len()
        ));
    }
    found.join("\n")
}

/// Run the exchange from boot in windows of [`LATENCY`], as the Driver's
/// lanes would: a frame sent at `s` arrives at exactly `s + LATENCY`, inside
/// the next window, and each machine runs from its own next start, taking
/// frames at their arrival. With `timed`, a run takes every frame arriving
/// before the window's end at its own time; otherwise each run ends at the
/// next arrival. Looks at every window's end until `windows` (or, without
/// it, until the client is done), with full state every `state_every`
/// windows and at the last.
fn schedule<M: Machine<Context = ()>>(
    machines: [M; 2],
    timed: bool,
    windows: Option<usize>,
    state_every: usize,
) -> Schedule
where
    M::Error: Debug,
{
    let guard = HangGuard::new("timed inbox network exchange", HOST_LIMIT);
    let mut lanes = machines.map(|machine| Lane {
        machine,
        clock: SimulationClock::new(0),
        rng: SeededRng::new(41),
        stopped: StopReason::BoundaryReached,
        reached: 0,
        inbox: VecDeque::new(),
        arrivals: VecDeque::new(),
        output: String::new(),
        console: String::new(),
    });
    let mut frames = Vec::new();
    let (mut runs, mut taken) = (0, 0);
    let mut looks = Vec::new();
    for window in 0.. {
        let done = lanes[1].output.contains("NETPROBE done");
        if windows.map_or(done, |windows| window == windows) {
            break;
        }
        let end = (window as u64 + 1) * LATENCY;
        assert!(end < LIMIT, "no exchange within the simulated bound");
        guard.remaining().expect("timed inbox exchange hang");
        let mut routed = Vec::new();
        for (index, lane) in lanes.iter_mut().enumerate() {
            loop {
                let next = match lane.stopped {
                    StopReason::BoundaryReached => Some(lane.reached),
                    StopReason::Blocked { deadline, .. } => deadline.map(Timestamp::as_nanos),
                    StopReason::Exited { .. } => panic!("{index} exited: {:?}", lane.output),
                };
                let arrival = lane
                    .arrivals
                    .front()
                    .map(|&(at, _)| at)
                    .filter(|&at| at < end);
                if let Some(at) = arrival.filter(|&at| next.is_none_or(|start| start >= at)) {
                    // Frames arriving at `at` wait for the run from `at`; a
                    // blocked receiver becomes runnable then.
                    while lane.arrivals.front().is_some_and(|&(due, _)| due == at) {
                        let (_, bytes) = lane.arrivals.pop_front().unwrap();
                        lane.inbox.push_back(Input {
                            at: Timestamp::from_nanos(at),
                            payload: Request::ReceiveFrame { bytes },
                        });
                    }
                    if let StopReason::Blocked { .. } = lane.stopped {
                        lane.stopped = StopReason::BoundaryReached;
                        lane.reached = at;
                    }
                    continue;
                }
                let Some(start) = next.filter(|&start| start < end) else {
                    break;
                };
                let (upto, offered) = if timed {
                    let offered = lane
                        .arrivals
                        .iter()
                        .take_while(|&&(at, _)| at < end)
                        .count();
                    (end, offered)
                } else {
                    (arrival.map_or(end, |at| at.min(end)), 0)
                };
                let mut inbox = std::mem::take(&mut lane.inbox);
                for (at, bytes) in lane.arrivals.drain(..offered) {
                    inbox.push_back(Input {
                        at: Timestamp::from_nanos(at),
                        payload: Request::ReceiveFrame { bytes },
                    });
                }
                lane.clock.set(start);
                let mut staged = Vec::new();
                let view = Inbox::new(&mut inbox, Timestamp::from_nanos(start));
                let stop = lane
                    .machine
                    .run(
                        &mut RunContext {
                            worker: &mut (),
                            clock: &lane.clock,
                            rng: &mut lane.rng,
                            execution: Default::default(),
                            inbox: if timed {
                                view.with_timed(Timestamp::from_nanos(upto))
                            } else {
                                view
                            },
                            outbox: Outbox::new(
                                &mut staged,
                                Timestamp::from_nanos(start),
                                Timestamp::from_nanos(upto),
                            ),
                        },
                        Timestamp::from_nanos(upto),
                    )
                    .unwrap();
                runs += 1;
                // Frames the run did not reach go back, in order.
                let left = inbox.len();
                assert!(left <= offered, "{index} left due inputs");
                taken += offered - left;
                for input in inbox.into_iter().rev() {
                    let Request::ReceiveFrame { bytes } = input.payload else {
                        unreachable!("only frames are timed");
                    };
                    lane.arrivals.push_front((input.at.as_nanos(), bytes));
                }
                lane.reached = match stop {
                    StopReason::BoundaryReached => upto,
                    StopReason::Blocked { reached, .. } => reached.as_nanos(),
                    StopReason::Exited { .. } => panic!("{index} exited: {:?}", lane.output),
                };
                assert!(
                    lane.arrivals
                        .front()
                        .is_none_or(|&(at, _)| at >= lane.reached),
                    "{index} ran past a frame it did not take"
                );
                lane.stopped = stop;
                for output in staged {
                    let at = output.at.as_nanos();
                    match output.payload {
                        Response::ApplicationOutput { bytes, .. } => {
                            lane.output.push_str(&String::from_utf8_lossy(&bytes))
                        }
                        Response::BootConsole { bytes } => {
                            lane.console.push_str(&String::from_utf8_lossy(&bytes))
                        }
                        Response::ApplicationOutcome { code } => assert!(
                            index == 1 && code == 0 && lane.output.ends_with("NETPROBE done\n"),
                            "{index} probe exited {code}: {:?}",
                            lane.output
                        ),
                        Response::TransmitFrame { bytes } => {
                            let to = route(index, &bytes);
                            let arrival = to.map(|_| at + LATENCY);
                            frames.push((at, index, to, arrival, Sha256::digest(&bytes).into()));
                            if let (Some(to), Some(arrival)) = (to, arrival) {
                                routed.push((to, arrival, bytes));
                            }
                        }
                        _ => (),
                    }
                }
            }
        }
        // Every frame sent in this window arrives in the next one, after
        // those already routed to its receiver.
        for (to, arrival, bytes) in routed {
            assert!(arrival >= end);
            lanes[to].arrivals.push_back((arrival, bytes));
        }
        // The last window: the requested count, or the one the client
        // finished in.
        let last = windows.map_or_else(
            || lanes[1].output.contains("NETPROBE done"),
            |windows| window + 1 == windows,
        );
        let state = (window % state_every == 0 || last).then(|| {
            lanes
                .each_mut()
                .map(|lane| lane.machine.state_evidence().unwrap())
        });
        looks.push(Look {
            at: end,
            output: lanes.each_ref().map(|lane| lane.output.clone()),
            console: lanes.each_ref().map(|lane| lane.console.clone()),
            frames: frames.clone(),
            witness: lanes
                .each_mut()
                .map(|lane| lane.machine.execution_evidence().unwrap()),
            state,
        });
    }
    Schedule {
        looks,
        runs,
        timed: taken,
    }
}

/// A machine that honours a timed inbox takes the frames arriving inside a
/// run at their own times: at every barrier its output, console, native
/// witness and (every `state_every` windows) full state equal those of one
/// run per arrival, and it makes fewer runs.
pub fn timed_inbox_matches_one_run_per_arrival<M: Machine<Context = ()>>(
    backend: &Backend<M>,
    state_every: usize,
) where
    M::Error: Debug,
{
    let machines = backend.machines();
    assert!(
        machines.iter().all(M::timed_inbox),
        "{} timed inbox",
        backend.name
    );
    let split = schedule(machines, false, None, state_every);
    let windows = split.looks.len();
    let timed = schedule(backend.machines(), true, Some(windows), state_every);
    let last = split.looks.last().expect("windows");
    let [server, client] = &last.output;
    assert_eq!(server, "NETPROBE ready port=7000\n");
    assert!(client.ends_with("NETPROBE done\n"), "{client:?}");
    assert!(
        last.state
            .as_ref()
            .is_some_and(|state| state.iter().all(Option::is_some)),
        "full state"
    );
    assert!(last.witness.iter().all(Option::is_some), "native witness");
    for (a, b) in split.looks.iter().zip(&timed.looks) {
        assert!(
            a == b,
            "first difference at {} ns ({} runs against {}):\n{}",
            a.at,
            timed.runs,
            split.runs,
            look_differences(a, b)
        );
    }
    eprintln!(
        "{}: {windows} windows, {} frames, {} runs one per arrival, {} with timed inboxes ({} frames inside runs)",
        backend.name,
        last.frames.len(),
        split.runs,
        timed.runs,
        timed.timed
    );
    assert_eq!(split.timed, 0);
    assert!(timed.timed > 0 && timed.runs < split.runs);
}
