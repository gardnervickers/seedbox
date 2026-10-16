//! Backend-neutral real-Linux disk scenarios: guest FLUSH and fsync
//! durability, loss of unflushed writes at every boot end, graceful-shutdown
//! sync, repeated boots and snapshots that never promote. Assertions read both
//! the guest's reports on later boots and the machine's own durable store.
//! Simulated bounds define the result; the host-time check only diagnoses a
//! hang. Each backend supplies a [`Backend`] and keeps its own entry points.
use crate::{
    clock::{Clock, SimulationClock},
    definition::{
        DiskImage, FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources,
    },
    machine::{disk::Store, ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{Inbox, Input, Outbox, Request, Response},
    rng::SeededRng,
};
use std::{
    fmt::Debug,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

/// A boxed backend factory, as the Driver takes it.
pub type Create<M> = Box<dyn FnMut(&MachineDefinition) -> Result<M, <M as Machine>::Error> + Send>;

// TODO: Re-evaluate: PV and TCG must exercise the same durability assertions.
// Only construction, stopped-store access and restore vary; these test-only
// callbacks keep those differences out of the scenario bodies.
/// Construction, restore and stopped-store operations for the shared scenarios.
pub struct Backend<M: Machine> {
    /// A new factory over the backend's assets.
    pub factory: fn() -> Create<M>,
    /// Reconstruct a machine from its snapshot.
    pub restore: fn(&M::Snapshot) -> Result<M, M::Error>,
    /// The machine's durable disk between boots.
    pub store: fn(&M) -> Option<&Store>,
    /// The static `disk-probe` guest application.
    pub probe: PathBuf,
    /// The empty ext4 image fixture.
    pub image: PathBuf,
    pub memory_bytes: u64,
}

pub const QUANTUM: u64 = 10_000_000;
/// Simulated bound for one boot to reach the awaited marker.
pub const BOOT_LIMIT: u64 = 60_000_000_000;
/// Host diagnostic only; it cannot interrupt a call that never returns.
pub const HOST_LIMIT: Duration = Duration::from_secs(600);
/// Probe sectors (see tests/fixtures/disk-probe.c).
const RECORD: u64 = 0;
const A: u64 = 8;
const B: u64 = 16;
const BASE: u64 = 24;

pub fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
/// The probe as a root application with `arguments`, over `disk`.
pub fn definition<M: Machine>(
    backend: &Backend<M>,
    name: &str,
    arguments: &[&str],
    disk: DiskImage,
) -> MachineDefinition {
    let binary = fs::read(&backend.probe).unwrap();
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary.as_slice())
        .unwrap();
    let mut argv = vec!["/application".to_owned()];
    argv.extend(arguments.iter().map(|argument| argument.to_string()));
    MachineDefinition {
        name: name.into(),
        resources: Resources {
            memory_bytes: backend.memory_bytes,
        },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv,
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: Some(disk),
        network: None,
    }
}

/// A 64 KiB image whose base sector reads 0x77; every other byte is zero.
pub fn raw_image() -> DiskImage {
    let mut bytes = vec![0; 64 << 10];
    bytes[BASE as usize * 512..(BASE as usize + 1) * 512].fill(0x77);
    DiskImage::from_bytes(bytes).unwrap()
}

/// First byte of `sector` in a store.
fn first(store: &Store, sector: u64) -> u8 {
    let mut bytes = [0; 512];
    store.read(sector, &mut bytes).unwrap();
    assert!(bytes.iter().all(|&byte| byte == bytes[0]), "uniform sector");
    bytes[0]
}
fn generation(store: &Store) -> u64 {
    let mut bytes = [0; 512];
    store.read(RECORD, &mut bytes).unwrap();
    assert_eq!(&bytes[..8], b"SBXPROBE");
    u64::from_le_bytes(bytes[8..16].try_into().unwrap())
}

/// One machine driven directly through the Machine contract.
struct Guest<'b, M: Machine> {
    backend: &'b Backend<M>,
    machine: M,
    clock: SimulationClock,
    rng: SeededRng,
    /// Application stdout since the current boot started.
    output: String,
    /// Kernel console, kept for failure diagnostics only.
    console: String,
}
impl<'b, M: Machine<Context = ()>> Guest<'b, M>
where
    M::Error: Debug,
{
    fn new(backend: &'b Backend<M>, machine: M) -> Self {
        Self {
            backend,
            machine,
            clock: SimulationClock::new(0),
            rng: SeededRng::new(23),
            output: String::new(),
            console: String::new(),
        }
    }
    fn now(&self) -> u64 {
        self.clock.now()
    }
    fn step(&mut self, requests: Vec<Request>, upto: u64) -> StopReason {
        let now = at(self.now());
        let mut inbox = requests
            .into_iter()
            .map(|payload| Input { at: now, payload })
            .collect();
        let mut staged = Vec::new();
        let stop = self
            .machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &self.clock,
                    rng: &mut self.rng,
                    execution: Default::default(),
                    inbox: Inbox::new(&mut inbox, now),
                    outbox: Outbox::new(&mut staged, now, at(upto)),
                },
                at(upto),
            )
            .unwrap();
        assert!(inbox.is_empty(), "every due request is accepted");
        for output in staged {
            match output.payload {
                Response::ApplicationOutput { bytes, .. } => {
                    self.output.push_str(&String::from_utf8_lossy(&bytes))
                }
                Response::BootConsole { bytes } => {
                    self.console.push_str(&String::from_utf8_lossy(&bytes))
                }
                Response::ApplicationOutcome { .. } => {
                    panic!("probe exited: {:?}\n{}", self.output, self.console)
                }
                _ => (),
            }
        }
        stop
    }
    /// Run until the application has printed `marker` in this boot.
    fn until(&mut self, marker: &str) {
        let started = Instant::now();
        let limit = self.now() + BOOT_LIMIT;
        while !self.output.contains(marker) {
            assert!(started.elapsed() < HOST_LIMIT, "host diagnostic: no marker");
            let upto = self.now() + QUANTUM;
            assert!(upto <= limit, "no {marker:?} within the simulated bound");
            match self.step(Vec::new(), upto) {
                StopReason::BoundaryReached => self.clock.set(upto),
                StopReason::Blocked {
                    deadline: Some(deadline),
                    ..
                } => self.clock.set(deadline.as_nanos()),
                StopReason::Blocked { reached, .. } => {
                    self.clock.set(reached.as_nanos());
                    assert!(
                        self.output.contains(marker),
                        "blocked without {marker:?}: {:?}",
                        self.output
                    );
                }
                stop @ StopReason::Exited { .. } => {
                    panic!("{stop:?} before {marker:?}: {:?}", self.output)
                }
            }
        }
    }
    /// Run until the guest ends its boot.
    fn until_exit(&mut self) -> ExitReason {
        let started = Instant::now();
        let limit = self.now() + BOOT_LIMIT;
        loop {
            assert!(started.elapsed() < HOST_LIMIT, "host diagnostic: no exit");
            let upto = self.now() + QUANTUM;
            assert!(upto <= limit, "no exit within the simulated bound");
            match self.step(Vec::new(), upto) {
                StopReason::BoundaryReached => self.clock.set(upto),
                StopReason::Blocked {
                    deadline: Some(deadline),
                    ..
                } => self.clock.set(deadline.as_nanos()),
                StopReason::Blocked { reached, .. } => self.clock.set(reached.as_nanos()),
                StopReason::Exited { reached, reason } => {
                    self.clock.set(reached.as_nanos());
                    return reason;
                }
            }
        }
    }
    /// The one report line of the current boot.
    fn report(&self) -> &str {
        self.output
            .lines()
            .find(|line| line.starts_with("DISK raw ") || line.starts_with("DISK ext4 "))
            .unwrap_or_else(|| panic!("no report: {:?}", self.output))
    }
    fn crash(&mut self) {
        let now = self.now();
        let stop = self.step(vec![Request::Crash], now);
        assert_eq!(
            stop,
            StopReason::Exited {
                reached: at(now),
                reason: ExitReason::Crashed
            }
        );
    }
    /// Request a fresh boot; the old boot, live or ended, ends first.
    fn reboot(&mut self) {
        let now = self.now();
        assert_eq!(
            self.step(vec![Request::Reboot], now),
            StopReason::BoundaryReached
        );
        self.output.clear();
    }
    /// The machine's durable disk between boots.
    fn store(&self) -> &Store {
        (self.backend.store)(&self.machine).expect("disk owned between boots")
    }
    /// Capture the machine with the caller-owned time, RNG and output.
    fn snapshot(&mut self) -> Saved<M> {
        Saved {
            snapshot: self.machine.snapshot().unwrap(),
            now: self.now(),
            rng: self.rng.clone(),
            output: self.output.clone(),
        }
    }
}
/// A machine snapshot plus the caller state needed to continue it.
struct Saved<M: Machine> {
    snapshot: M::Snapshot,
    now: u64,
    rng: SeededRng,
    output: String,
}
impl<M: Machine<Context = ()>> Saved<M>
where
    M::Error: Debug,
{
    fn restore<'b>(&self, backend: &'b Backend<M>) -> Guest<'b, M> {
        Guest {
            backend,
            machine: (backend.restore)(&self.snapshot).unwrap(),
            clock: SimulationClock::new(self.now),
            rng: self.rng.clone(),
            output: self.output.clone(),
            console: String::new(),
        }
    }
}

/// Raw boots ended alternately by Crash and by a live Reboot: every boot
/// sees the previous FLUSHed generation and never its unflushed write.
fn raw_generations<M: Machine<Context = ()>>(backend: &Backend<M>, disk: DiskImage, base: u8)
where
    M::Error: Debug,
{
    let bytes = disk.capacity_bytes();
    let mut factory = (backend.factory)();
    let mut guest = Guest::new(
        backend,
        factory(&definition(backend, "raw", &["raw"], disk)).unwrap(),
    );
    for boot in 0..4u64 {
        guest.until(&format!("DISK volatile {}\n", boot + 1));
        assert_eq!(
            guest.report(),
            format!("DISK raw boot={boot} a={boot:02x} b=00 base={base:02x} bytes={bytes}")
        );
        if boot % 2 == 0 {
            guest.crash();
            let store = guest.store();
            assert_eq!(generation(store), boot + 1);
            assert_eq!(first(store, A), boot as u8 + 1);
            assert_eq!(first(store, B), 0, "unflushed write lost at crash");
            assert_eq!(first(store, BASE), base);
            // A repeated Crash of the ended boot discards nothing more.
            let again = guest.store().clone();
            guest.crash();
            assert_eq!(
                guest.store().encode_overlays(),
                again.encode_overlays(),
                "repeated crash"
            );
        }
        guest.reboot();
    }
}

/// Raw generations over an image disk, then over a zeroed 1 GiB disk.
pub fn raw_flush_survives_boot_end_and_unflushed_writes_are_lost<M: Machine<Context = ()>>(
    backend: &Backend<M>,
) where
    M::Error: Debug,
{
    raw_generations(backend, raw_image(), 0x77);
    raw_generations(backend, DiskImage::zeroed(1 << 30).unwrap(), 0);
}

pub fn ext4_fsync_survives_crash_and_shutdown_sync_keeps_dirty_data<M: Machine<Context = ()>>(
    backend: &Backend<M>,
) where
    M::Error: Debug,
{
    let image = DiskImage::from_bytes(fs::read(&backend.image).unwrap()).unwrap();
    let mut factory = (backend.factory)();
    let mut guest = Guest::new(
        backend,
        factory(&definition(backend, "ext4", &["ext4"], image)).unwrap(),
    );
    guest.until("DISK dirty 1\n");
    assert_eq!(guest.report(), "DISK ext4 boot=0 synced=- dirty=-");
    guest.crash();
    guest.reboot();
    guest.until("DISK dirty 2\n");
    // Data the crash may or may not have kept is not asserted.
    assert!(
        guest
            .report()
            .starts_with("DISK ext4 boot=1 synced=1 dirty="),
        "{}",
        guest.report()
    );
    // Graceful shutdown: the guest's own sync, not the host, makes it durable.
    let now = guest.now();
    guest.step(vec![Request::Shutdown], now);
    assert_eq!(guest.until_exit(), ExitReason::Shutdown);
    guest.reboot();
    guest.until("DISK dirty 3\n");
    assert_eq!(guest.report(), "DISK ext4 boot=2 synced=2 dirty=2");
}

pub fn snapshots_keep_both_layers_independently_without_promotion<M: Machine<Context = ()>>(
    backend: &Backend<M>,
) where
    M::Error: Debug,
{
    let mut factory = (backend.factory)();
    let mut source = Guest::new(
        backend,
        factory(&definition(backend, "snapshot", &["raw", "5"], raw_image())).unwrap(),
    );
    source.until("DISK volatile 1\n");
    let live = source.snapshot();

    // The source loses its unflushed write at its own crash.
    source.crash();
    assert_eq!((first(source.store(), A), first(source.store(), B)), (1, 0));
    let terminal = source.snapshot();
    // Restored copies own their base: nothing depends on the destroyed source.
    drop(source);

    // A restored copy continues the same boot and FLUSHes the write itself.
    let mut flushed = live.restore(backend);
    flushed.until("DISK flushed 1\n");
    flushed.crash();
    assert_eq!(
        (first(flushed.store(), A), first(flushed.store(), B)),
        (1, 1)
    );

    // Capturing promoted nothing: another copy crashed at once loses it, and
    // its next boot reports only the FLUSHed generation.
    let mut lost = live.restore(backend);
    lost.crash();
    assert_eq!((first(lost.store(), A), first(lost.store(), B)), (1, 0));
    lost.reboot();
    lost.until("DISK volatile 2\n");
    assert_eq!(
        lost.report(),
        "DISK raw boot=1 a=01 b=00 base=77 bytes=65536"
    );

    // A terminal snapshot holds the boot-end result already applied.
    let mut ended = terminal.restore(backend);
    assert_eq!(
        ended.store().encode_overlays(),
        {
            let mut expected = live.restore(backend);
            expected.crash();
            expected.store().encode_overlays()
        },
        "terminal capture equals the crashed source"
    );
    ended.reboot();
    ended.until("DISK volatile 2\n");
    assert_eq!(
        ended.report(),
        "DISK raw boot=1 a=01 b=00 base=77 bytes=65536"
    );

    // The flushed copy's next boot sees its own durable write.
    flushed.reboot();
    flushed.until("DISK volatile 2\n");
    assert_eq!(
        flushed.report(),
        "DISK raw boot=1 a=01 b=01 base=77 bytes=65536"
    );
}
