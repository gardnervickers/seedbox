//! Seedbox world: several Linux machines, a network fabric and a scenario,
//! all executed by one host thread. A seed and the config fully determine the run.

#![recursion_limit = "256"]

mod boundary;
mod live_checkpoint;
mod management_uart;
use seedbox_engine_core::{checkpoint, control, disk, links, output_layout, rng};
#[cfg(test)]
mod device_tests;
mod devices;
pub use seedbox_engine_core::host_control;
mod machine;
mod shadow;
mod virtio_blk;
mod virtio_console;
#[cfg(test)]
mod virtio_console_tests;
mod virtio_net;
mod witness_log;
mod world_adapter;
mod xsave;

use machine::pv::PvClock;
use machine::{
    BootSpec, Machine, State, BLK_IRQ, BLK_MMIO, CHANNEL_IRQ, CHANNEL_MMIO, NET_IRQ, NET_MMIO,
};
use rng::Rng;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

thread_local! {
    static START: Instant = Instant::now();
    static CONSOLE_TRACE: Cell<Option<ConsoleTraceCallback>> = const { Cell::new(None) };
}

pub use seedbox_engine_core::launch::{MachineFeatures, Performance, WorkerRequest};

/// One console line observed at the host diagnostic boundary.
pub struct ConsoleTrace<'a> {
    pub host_seconds: f64,
    pub guest_seconds: f64,
    pub line: &'a [u8],
}

impl std::fmt::Display for ConsoleTrace<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[host {:.3}s guest {:.6}s] {}",
            self.host_seconds,
            self.guest_seconds,
            String::from_utf8_lossy(self.line)
        )
    }
}

pub type ConsoleTraceCallback = for<'a> fn(ConsoleTrace<'a>);

/// The console trace a worker main installs for `SEEDBOX_TRACE_CONSOLE`.
pub fn print_console_trace(trace: ConsoleTrace<'_>) {
    eprintln!("{trace}");
}

/// Debug-only worker settings from the environment. They never change results.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DebugEnv {
    /// `SEEDBOX_WATCHDOG_MS`: the spin watchdog period.
    pub watchdog_ms: Option<u64>,
    /// `SEEDBOX_WINDOWED_SKEW_US`: reversed-completion testing delay.
    pub windowed_skew_us: Option<u64>,
    /// `SEEDBOX_TRACE_CONSOLE`: print each guest console line with its times.
    pub trace_console: bool,
}

impl DebugEnv {
    /// Read the settings through `get`, the worker main's environment lookup.
    pub fn from_env(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Result<Self, String> {
        let number = |name: &str| -> Result<Option<u64>, String> {
            get(name)
                .map(|value| {
                    value
                        .to_str()
                        .and_then(|v| v.parse().ok())
                        .ok_or_else(|| format!("{name}: expected an unsigned integer"))
                })
                .transpose()
        };
        Ok(Self {
            watchdog_ms: number("SEEDBOX_WATCHDOG_MS")?,
            windowed_skew_us: number("SEEDBOX_WINDOWED_SKEW_US")?,
            trace_console: get("SEEDBOX_TRACE_CONSOLE").is_some(),
        })
    }

    /// Apply the settings to `request`; returns the console trace to install.
    pub fn apply(self, request: &mut WorkerRequest) -> Option<ConsoleTraceCallback> {
        if let Some(ms) = self.watchdog_ms {
            request.performance.watchdog_ms = ms;
        }
        if let Some(us) = self.windowed_skew_us {
            request.windowed_skew_us = us;
        }
        self.trace_console
            .then_some(print_console_trace as ConsoleTraceCallback)
    }
}

struct ConsoleTraceGuard(Option<ConsoleTraceCallback>);
impl Drop for ConsoleTraceGuard {
    fn drop(&mut self) {
        CONSOLE_TRACE.with(|trace| trace.set(self.0));
    }
}

pub(crate) fn console_trace_enabled() -> bool {
    CONSOLE_TRACE.with(|trace| trace.get().is_some())
}

pub(crate) fn emit_console_trace(time: u64, line: &[u8]) {
    CONSOLE_TRACE.with(|trace| {
        if let Some(callback) = trace.get() {
            callback(ConsoleTrace {
                host_seconds: START.with(|s| s.elapsed().as_secs_f64()),
                guest_seconds: time as f64 / 1e9,
                line,
            });
        }
    });
}

/// The world's clock (see `machine/pv.rs`), one per executing thread.
pub type Clock = PvClock;

/// Open the clock on the calling thread: its watchdog measures that thread's
/// CPU time after `watchdog_ms` (performance only).
fn open_clock(watchdog_ms: u64) -> Result<Clock, String> {
    PvClock::new(std::time::Duration::from_millis(watchdog_ms.max(1)))
}

/// Guest image bytes. Boot copies a mapped image into guest RAM straight
/// from the page cache instead of reading it into the heap first.
pub(crate) enum GuestImage {
    Mapped { ptr: *const u8, len: usize },
    Owned(Vec<u8>),
}

impl GuestImage {
    fn open(path: &str) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("file too large"))?;
        if len == 0 {
            return Ok(Self::Owned(Vec::new()));
        }
        // SAFETY: a fresh private read-only mapping of an open file; retained
        // inputs are read-only and nothing writes them while a run executes.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self::Mapped {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl From<Vec<u8>> for GuestImage {
    fn from(bytes: Vec<u8>) -> Self {
        Self::Owned(bytes)
    }
}

impl std::ops::Deref for GuestImage {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match self {
            // SAFETY: `ptr` maps `len` readable bytes until drop.
            Self::Mapped { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
            Self::Owned(bytes) => bytes,
        }
    }
}

impl Drop for GuestImage {
    fn drop(&mut self) {
        if let Self::Mapped { ptr, len } = self {
            unsafe { libc::munmap(*ptr as *mut libc::c_void, *len) };
        }
    }
}

/// Everything a machine boots from. A reboot starts a new incarnation from
/// exactly these inputs, with its own entropy and timer seeds.
#[derive(Clone)]
pub struct BootInputs {
    kernel: Rc<[u8]>,
    initrd: Rc<GuestImage>,
    /// The command line before and after the launch barrier flag.
    cmdline: (String, String),
    mem: usize,
    mac: Option<[u8; 6]>,
    control_channel: bool,
    /// The disk as configured, with nothing written. Only the first
    /// incarnation boots from it; a reboot inherits its predecessor's.
    disk: Option<disk::Disk>,
    tolerance: u64,
    timer_jitter: u64,
    cpu_speed_hz: u64,
    progress_ns: u64,
    progress_jitter: u64,
    exact_timer_ppm: u64,
    console_limit: Option<usize>,
    features: MachineFeatures,
    log_chunk_limit: Option<usize>,
    observations: Option<std::sync::Arc<seedbox_engine_core::observation::Publisher>>,
}

/// A machine's disk: zeroed, or a raw image relative to `base` (with
/// `size_mib`, it must be that size).
fn machine_disk(cfg: Option<&DiskConfig>, base: &Path) -> Result<Option<disk::Disk>, String> {
    let Some(cfg) = cfg else {
        return Ok(None);
    };
    let sectors = cfg
        .size_mib
        .map(|m| {
            (m > 0 && m <= 1 << 20)
                .then_some(m * 2048)
                .ok_or("disk.size_mib must be 1..=1048576")
        })
        .transpose()?;
    let image = cfg
        .image
        .as_ref()
        .map(|p| {
            let p = base.join(p);
            std::fs::read(&p).map_err(|e| format!("disk image {}: {e}", p.display()))
        })
        .transpose()?;
    disk::Disk::new(image, sectors).map(Some)
}

fn boot_machine(
    kvm: &kvm_ioctls::Kvm,
    name: &str,
    b: &BootInputs,
    barrier: bool,
    rng_seed: [u8; 32],
) -> Result<Machine, String> {
    let mut cmdline = b.cmdline.0.clone();
    if barrier {
        cmdline += " seedbox.barrier=1";
    }
    cmdline += &b.cmdline.1;
    let spec = BootSpec {
        kernel: &b.kernel,
        initrd: &b.initrd,
        cmdline,
        mem: b.mem,
        rng_seed,
        mac: b.mac,
        control_channel: b.control_channel,
        disk: b.disk.clone(),
        barrier,
        page_timer: b.features.page_timer,
    };
    let mut mach = Machine::new(kvm, name, &spec)?;
    if let Some(net) = mach.net.as_mut() {
        net.set_deferred_tx(b.features.deferred_tx);
    }
    mach.com1.output_limit = b.console_limit;
    mach.com2.output_limit = b.console_limit;
    mach.com1.chunks.limit = b.log_chunk_limit;
    mach.com2.chunks.limit = b.log_chunk_limit;
    mach.observations = b.observations.clone();
    mach.com1.discard_output = b.observations.is_some();
    mach.com2.discard_output = b.observations.is_some();
    mach.tolerance = b.tolerance;
    mach.timer_jitter = b.timer_jitter;
    mach.exact_timer_ppm = b.exact_timer_ppm;
    // The charge and its bound; the jitter stream's seed follows from the
    // boot seed (first boot) or the incarnation (`succeed`).
    mach.set_progress(b.progress_ns, b.progress_jitter, 0);
    mach.cpu_speed_hz = b.cpu_speed_hz;
    Ok(mach)
}

pub use seedbox_engine_core::report::RunReport;
use seedbox_engine_core::report::{
    BootReport, BootSummary, ControlChannelReport, ControlRecord, DiskReport, EngineReport,
    IoDirection, Kill, MachineOutcome, MachineReport, PendingIo, ShadowReport, SkewReport,
    TimerReport, WorldEvidence, REPORT_VERSION,
};
use seedbox_engine_core::world::{
    self as world_core, arm_seed, boot_faults, entropy_seed, Payload,
};
use seedbox_engine_core::world_config::{CheckpointConfig, DiskConfig, SeedPlan, WorldConfig};
use seedbox_engine_core::world_windowed;
use seedbox_engine_core::world_windowed::RunOutcome;
type World = world_core::World<Machine>;
type WorldSnapshot = world_core::WorldSnapshot<Machine>;

/// Host playback is inspected before effects without creating a new retained
/// boundary; ordinary replay/admission remains at its original hook.
struct NativeBoundary<'a> {
    host: &'a mut Option<host_control::HostControl>,
}
impl world_windowed::BoundaryCallbacks for NativeBoundary<'_> {
    fn before_fault(&mut self, position: world_windowed::Boundary) -> Result<(), String> {
        if let Some(host) = self.host.as_mut() {
            host.before_fault(position)
                .map_err(|e| format!("host control: {e}"))?;
        }
        Ok(())
    }
    fn boundary(
        &mut self,
        position: world_windowed::Boundary,
        admission: &mut seedbox_engine_core::intervention::Admission<'_>,
    ) -> Result<(), String> {
        // All external actions are armed in the shared timed program.
        if let Some(host) = self.host.as_mut() {
            host.boundary_with_admission(position, admission)
                .map_err(|e| format!("host control: {e}"))?;
        }
        Ok(())
    }
}

/// One segment of the run under the windowed scheduler, on one thread or
/// the machine worker pool. Returns the outcome word and the scheduler's
/// report section.
fn simulate_segment(
    world: &mut World,
    run: &world_windowed::Run,
    request: &WorkerRequest,
    host_control: &mut Option<host_control::HostControl>,
    _replay: &mut seedbox_engine_core::intervention::Replay,
) -> Result<(RunOutcome, Value), String> {
    let mut boundary = NativeBoundary { host: host_control };
    let skew = std::time::Duration::from_micros(request.windowed_skew_us);
    if request.machine_workers <= 1 {
        return world_windowed::simulate_with_callbacks(world, run, skew, &mut boundary);
    }
    // Each pool thread opens its own clock: the watchdog measures that
    // thread's CPU time.
    let watchdog_ms = request.performance.watchdog_ms;
    let workers = world_windowed::Workers {
        count: request.machine_workers,
        skew,
    };
    world_windowed::simulate_parallel_with_callbacks(
        world,
        run,
        workers,
        |_| open_clock(watchdog_ms),
        &mut boundary,
    )
}
/// Hold interactive snapshots across segments; a successful capture is an
/// observation of the current timeline, not the simulation's final outcome.
fn simulate_windowed(
    world: &mut World,
    run: &world_windowed::Run,
    request: &WorkerRequest,
    host: &mut Option<host_control::HostControl>,
    replay: &mut seedbox_engine_core::intervention::Replay,
    checkpoints: &mut live_checkpoint::Checkpoints,
) -> Result<(RunOutcome, Value), String> {
    let mut segments = Vec::new();
    loop {
        if let Some(host) = host.as_mut() {
            checkpoints.held.configure(host);
        }
        let (outcome, mut scheduler) = simulate_segment(world, run, request, host, replay)?;
        if outcome == RunOutcome::Checkpoint {
            if let Some(host) = host.as_mut() {
                if checkpoints.capture(world, replay, host, &request.out)? {
                    segments.push(scheduler);
                    continue;
                }
            }
        }
        if !segments.is_empty() {
            segments.push(scheduler.clone());
            scheduler["segments"] = serde_json::json!(segments);
        }
        return Ok((outcome, scheduler));
    }
}

use world_core::reseed_checkpoint;

fn release(
    world: &mut World,
    plan: SeedPlan<'_>,
    names: &[String],
    seed: u64,
    skew: u64,
) -> Result<(Vec<Kill>, Value), String> {
    world_core::release_plan(world, plan, names, seed, skew, &mut |name, abi| {
        eprintln!("{}: guest ABI {} predates time-page reseeding; its CRNG is not reseeded at this time fork",name,abi);
    })
}

/// Read the world document at `path`.
fn read_config(path: &Path) -> Result<WorldConfig, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("config: {e}"))
}

/// Check the parts of a world document that no type expresses, before any
/// machine boots.
fn validate_config(cfg: &WorldConfig) -> Result<(), String> {
    seedbox_engine_core::input_program::validate_program(&cfg.input_program, cfg.duration_ns)?;
    for machine in &cfg.machines {
        if !(64..=65536).contains(&machine.memory_mb(cfg)) {
            return Err(format!(
                "machine {} memory must be 64..65536 MiB",
                machine.name
            ));
        }
    }
    if cfg.control.is_some() && cfg.control_channel.is_none() {
        return Err("runtime control requires virtio-console-v1 channel".into());
    }
    if let Some(control) = &cfg.control {
        if !cfg.scenario.is_empty() || cfg.kill_restart.is_some() {
            return Err("runtime control cannot mix with legacy scenarios".into());
        }
        if cfg.end_on_exit.as_ref() != Some(&control.machine) {
            return Err("controller must also be the end_on_exit machine".into());
        }
    }
    Ok(())
}

/// The kernel command line every machine starts with.
const BASE_CMDLINE: &str = "console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 \
     clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init";

/// What a machine's kernel command line announces.
struct Cmdline<'a> {
    name: &'a str,
    index: usize,
    net: bool,
    channel: bool,
    disk: bool,
    /// The machine runs the runtime controller.
    control: bool,
    /// The world document's extra arguments for this machine.
    extra: Option<&'a str>,
}

impl Cmdline<'_> {
    /// The command line before and after the launch barrier flag. Guests see
    /// these bytes: their order and spacing are part of every run.
    fn build(&self) -> (String, String) {
        let mut before = format!(
            "{BASE_CMDLINE} seedbox.name={} seedbox.index={}",
            self.name, self.index
        );
        for (on, mmio, irq) in [
            (self.net, NET_MMIO, NET_IRQ),
            (self.channel, CHANNEL_MMIO, CHANNEL_IRQ),
            (self.disk, BLK_MMIO, BLK_IRQ),
        ] {
            if on {
                before += &format!(" virtio_mmio.device=4K@{mmio:#x}:{irq}");
            }
        }
        let mut after = String::new();
        if self.control {
            after += " seedbox.control=1";
        }
        if let Some(extra) = self.extra {
            after += " ";
            after += extra;
        }
        (before, after)
    }
}

/// The checkpoint times of this run, from `--checkpoint-at-ns` or the
/// document's `checkpoint`; the latter also selects the branch seed.
fn checkpoint_plan(
    cfg: &WorldConfig,
    options: &WorkerRequest,
    seeds: &mut Vec<u64>,
) -> Result<Vec<u64>, String> {
    let duration = cfg.duration_ns;
    let times = match (options.checkpoint_at_ns.is_empty(), &cfg.checkpoint) {
        (false, Some(_)) => {
            return Err("--checkpoint-at-ns cannot override config.checkpoint".into())
        }
        (false, None) => {
            checkpoint::check_times(&options.checkpoint_at_ns, duration)?;
            options.checkpoint_at_ns.clone()
        }
        (true, Some(c)) => {
            *seeds = vec![c.branch_seed];
            if !options.fork_seeds.is_empty() {
                return Err("--fork-seeds cannot override config.checkpoint".into());
            }
            checkpoint::check_times(&[c.at_ns], duration)?;
            vec![c.at_ns]
        }
        (true, None) => Vec::new(),
    };
    if !times.is_empty() {
        if cfg.boot.is_some() {
            return Err("runtime checkpoints cannot mix with boot forks".into());
        }
        if options.trace {
            return Err("--trace cannot be combined with runtime checkpoints".into());
        }
        let unique: std::collections::BTreeSet<_> = seeds.iter().collect();
        if unique.len() != seeds.len() {
            return Err("runtime checkpoint branch seeds must be distinct".into());
        }
    }
    Ok(times)
}

/// Execute a world and return its engine report; the caller owns stdout, stderr and exit status.
/// `console_trace` receives each guest console line while the world runs.
pub fn run(
    options: &WorkerRequest,
    console_trace: Option<ConsoleTraceCallback>,
) -> Result<RunReport, String> {
    options.validate()?;
    let mut replay = seedbox_engine_core::intervention::Replay::new(Default::default())?;
    let observations = options
        .observations
        .as_deref()
        .map(seedbox_engine_core::observation::Publisher::connect)
        .transpose()
        .map_err(|e| format!("observation socket: {e}"))?;
    START.with(|_| ());
    let _console_trace =
        ConsoleTraceGuard(CONSOLE_TRACE.with(|trace| trace.replace(console_trace)));
    let config_path = &options.config;
    let seed = options.seed;
    let mut seeds = vec![seed];
    seeds.extend_from_slice(&options.fork_seeds);
    let out = options.out.to_string_lossy().into_owned();
    let digest = options.digest;
    let features = options.features;
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut live_checkpoints = live_checkpoint::Checkpoints::default();
    let mut host_control = options
        .host_control
        .as_deref()
        .map(|path| {
            if options.start_paused {
                host_control::HostControl::bind_paused(path)
            } else {
                host_control::HostControl::bind(path)
            }
        })
        .transpose()
        .map_err(|e| format!("host control: {e}"))?;
    let mut cfg = read_config(config_path)?;
    seedbox_engine_core::input_program::merge_interventions(
        &mut cfg.input_program,
        &options.interventions.clone().unwrap_or_default(),
    )?;
    validate_config(&cfg)?;
    let disk_fault_model = disk::fault_model(cfg.disk_loss)?;
    let base = config_path.parent().unwrap_or(Path::new(""));
    let kernel = std::fs::read(base.join(&cfg.kernel)).map_err(|e| format!("kernel: {e}"))?;
    let duration = cfg.duration_ns;
    let checkpoint_times = checkpoint_plan(&cfg, options, &mut seeds)?;
    let end_on = cfg.end_on_exit.as_deref();
    let names: Vec<String> = cfg.machines.iter().map(|m| m.name.clone()).collect();
    let networked = cfg.network.is_some();
    let control_channel = cfg.control_channel.is_some();

    // Boot once under a fixed boot seed and park every machine at its launch
    // barrier; the run seed applies from the release onwards. The world at
    // the barrier is the same for every run seed, so it can be forked.
    let forked = cfg.boot.is_some();
    let boot_seed = cfg.boot.map_or(seed, |b| b.seed);
    let release_skew = cfg.boot.map_or(0, |b| b.release_skew_ns);
    // Fork at a simulated time instead of the guests' launch barrier: every
    // machine parks at its first deterministic point at or after it.
    let fork_at = cfg.boot.and_then(|b| b.fork_at_ns);
    if fork_at.is_some_and(|t| t >= duration) {
        return Err("boot.fork_at_ns must be before duration_ns".into());
    }
    if let Some(t) = fork_at {
        // A barrier fork's release time is known only after boot; checked
        // again at the release.
        cfg.seed_plan()
            .check_after(t.saturating_add(release_skew))?;
    }
    if seeds.len() > 1 && !forked && checkpoint_times.is_empty() {
        return Err("--fork-seeds requires boot.seed in the config".into());
    }
    if seeds.len() > 1 && options.trace {
        return Err("--trace cannot be combined with --fork-seeds".into());
    }
    // One scheduler segment of this world: to its end, its fork point or a checkpoint.
    let segment = move |fork, checkpoint| world_windowed::Run {
        duration,
        end_on,
        fork,
        checkpoint,
        require_barrier: true,
    };
    let kvm = kvm_ioctls::Kvm::new().map_err(|e| format!("kvm: {e}"))?;
    let kernel: Rc<[u8]> = kernel.into();
    let console_limit = options.console_limit;
    let barrier = forked && fork_at.is_none();
    let mut machines = Vec::new();
    let mut macs = Vec::new();
    let mut boots = Vec::new();
    let mut initrds: BTreeMap<String, Rc<GuestImage>> = BTreeMap::new();
    for (i, mc) in cfg.machines.iter().enumerate() {
        let ipath = base.join(&mc.initrd).to_string_lossy().to_string();
        if !initrds.contains_key(&ipath) {
            initrds.insert(
                ipath.clone(),
                Rc::new(GuestImage::open(&ipath).map_err(|e| format!("{ipath}: {e}"))?),
            );
        }
        let mac = [0x52, 0x54, 0x00, 0x00, 0x00, i as u8 + 1];
        macs.push(mac);
        let disk =
            machine_disk(mc.disk.as_ref(), base)?.map(|d| d.with_fault_model(disk_fault_model));
        let cmdline = Cmdline {
            name: &mc.name,
            index: i,
            net: networked,
            channel: control_channel,
            disk: disk.is_some(),
            control: cfg.control.as_ref().is_some_and(|c| c.machine == mc.name),
            extra: mc.cmdline.as_deref(),
        };
        let boot = BootInputs {
            kernel: kernel.clone(),
            initrd: initrds[&ipath].clone(),
            cmdline: cmdline.build(),
            mem: mc.memory_mb(&cfg) as usize * 1024 * 1024,
            mac: networked.then_some(mac),
            control_channel,
            disk,
            tolerance: cfg.stop_tolerance_ns,
            timer_jitter: cfg.timer_jitter_ns,
            cpu_speed_hz: mc.cpu_speed_hz,
            progress_ns: mc.progress_ns,
            progress_jitter: mc.progress_jitter_ns,
            exact_timer_ppm: cfg.exact_timer_ppm.unwrap_or(0),
            console_limit,
            features,
            log_chunk_limit: options.log_chunk_limit,
            observations: observations.clone(),
        };
        let seed = entropy_seed(boot_seed, &format!("boot-entropy/{}", names[i]));
        let mut mach = boot_machine(&kvm, &names[i], &boot, barrier, seed)?;
        boots.push(boot);
        open_outputs(&mut mach, &out, options.stream_console)?;
        mach.timer_rng = Some(Rng::new(boot_seed, &format!("timer-jitter/{}", names[i])));
        mach.seed_progress_jitter(
            Rng::new(boot_seed, &format!("progress-jitter/{}", names[i])).next(),
        );
        if options.trace {
            let f = std::fs::File::create(format!("{out}/{}.trace", names[i]))
                .map_err(|e| e.to_string())?;
            mach.trace = Some(std::io::BufWriter::new(f));
        }
        machines.push(mach);
    }
    let net = cfg.network.clone().unwrap_or_default();
    let latency = net.latency_ns;
    let early = (latency / 2).min(cfg.stop_tolerance_ns);
    // Host CPU time a run may go without an exit before it is checked for a
    // spin (performance only).
    let clock = open_clock(options.performance.watchdog_ms)?;
    let mut world = World::new(world_core::WorldInputs {
        factory: kvm,
        boot: boots,
        barrier,
        boot_seed,
        run_seed: seed,
        clock,
        lookahead: latency,
        early,
        latency,
        // Faults, control and scenarios belong to the run seed: armed below,
        // or at the release of a booted world.
        faults: boot_faults(
            &names,
            &[],
            latency,
            links::Links::from_network(boot_seed, &names, &net)?,
        )?,
        control: control::Control::from_config(None, names.clone(), latency, early)?,
        macs,
        machines,
    })?
    .with_observations(observations);
    if let Some(path) = &options.resume_checkpoint {
        let saved = live_checkpoint::load(path)?;
        let state = saved.state;
        replay = seedbox_engine_core::intervention::Replay::new(Default::default())?;
        state.world.restore_checkpoint(&mut world)?;
        world.merge_input_program(cfg.input_program.clone())?;
        live_checkpoints.boot = saved.boot;
        live_checkpoints.kills = saved.kills;
        let started = Instant::now();
        let run = segment(None, None);
        let (outcome, scheduler) = simulate_windowed(
            &mut world,
            &run,
            options,
            &mut host_control,
            &mut replay,
            &mut live_checkpoints,
        )?;
        replay.finish()?;
        let finished = Finished {
            outcome,
            scheduler,
            seed,
            kills: &live_checkpoints.kills,
            host_seconds: started.elapsed().as_secs_f64(),
            out: &out,
        };
        let mut result = report(&mut world, &run, digest, finished)?;
        result.boot = live_checkpoints.boot;
        return Ok(RunReport::Single(Box::new(result)));
    }
    if !forked {
        let kills = arm_seed(&mut world, cfg.seed_plan(), &names, seed, 0)?;
        live_checkpoints.kills = kills.clone();
        if !checkpoint_times.is_empty() {
            let parent = checkpoint::parent_identity(
                &cfg,
                seed,
                features.deferred_tx,
                features.page_timer,
                |p| {
                    std::fs::read(base.join(p))
                        .map(seedbox_engine_core::sha256_hex)
                        .map_err(|e| format!("{}: {e}", p.display()))
                },
            )?;
            let expected = cfg.checkpoint.as_ref();
            if expected
                .and_then(|c| c.parent_identity.as_deref())
                .is_some_and(|expected| expected != parent)
            {
                return Err(
                    "checkpoint parent identity does not match retained world inputs".into(),
                );
            }
            // Reconstruct each prefix from the same origin: an extra hard
            // stop for an earlier checkpoint must not change a later one's
            // identity or execution. The held snapshots remain independent.
            let origin = if checkpoint_times.len() > 1 {
                Some(WorldSnapshot::take_checkpoint(&mut world)?)
            } else {
                None
            };
            let origin_replay = replay.clone();
            let mut held = Vec::new();
            for &at in &checkpoint_times {
                if let Some(origin) = origin.as_ref().filter(|_| !held.is_empty()) {
                    origin.restore_checkpoint(&mut world)?;
                    replay = origin_replay.clone();
                }
                let run = segment(None, Some(at));
                let (outcome, _) = simulate_windowed(
                    &mut world,
                    &run,
                    options,
                    &mut host_control,
                    &mut replay,
                    &mut live_checkpoints,
                )?;
                if outcome != RunOutcome::Checkpoint {
                    return Err(format!("checkpoint at {at} ns not reached: {outcome}"));
                }
                held.push((
                    at,
                    WorldSnapshot::take_checkpoint(&mut world)?,
                    replay.clone(),
                ));
            }
            let mut reports = Vec::new();
            for (at, snapshot, saved_replay) in &held {
                for &branch_seed in &seeds {
                    snapshot.restore_checkpoint(&mut world)?;
                    replay = saved_replay.clone();
                    let identity = checkpoint::branch_identity(&parent, *at, branch_seed);
                    let reseed =
                        reseed_checkpoint(&mut world, &names, seed, branch_seed, &identity)?;
                    let dir = output_layout::checkpoint_directory(
                        &out,
                        held.len(),
                        seeds.len(),
                        *at,
                        branch_seed,
                    );
                    reopen_outputs(&mut world, &dir, options.stream_console)?;
                    let started = Instant::now();
                    let run = segment(None, None);
                    let (outcome, scheduler) = simulate_windowed(
                        &mut world,
                        &run,
                        options,
                        &mut host_control,
                        &mut replay,
                        &mut live_checkpoints,
                    )?;
                    replay.finish()?;
                    // The public report's seed remains the parent seed; the
                    // semantic branch seed and identity have explicit fields.
                    let finished = Finished {
                        outcome,
                        scheduler,
                        seed,
                        kills: &kills,
                        host_seconds: started.elapsed().as_secs_f64(),
                        out: &dir,
                    };
                    let mut r = report(&mut world, &run, digest, finished)?;
                    r.checkpoint = Some(CheckpointConfig {
                        at_ns: *at,
                        branch_seed,
                        parent_identity: Some(parent.clone()),
                        branch_identity: Some(identity),
                    });
                    r.checkpoint_reseed = Some(reseed);
                    reports.push(r);
                }
            }
            return Ok(match <[_; 1]>::try_from(reports) {
                Ok([single]) => RunReport::Single(Box::new(single)),
                Err(checkpoints) => RunReport::Checkpoints {
                    report_version: REPORT_VERSION,
                    parent_identity: parent,
                    checkpoints,
                },
            });
        }
        let started = Instant::now();
        let run = segment(None, None);
        let (outcome, scheduler) = simulate_windowed(
            &mut world,
            &run,
            options,
            &mut host_control,
            &mut replay,
            &mut live_checkpoints,
        )?;
        let finished = Finished {
            outcome,
            scheduler,
            seed,
            kills: &kills,
            host_seconds: started.elapsed().as_secs_f64(),
            out: &out,
        };
        replay.finish()?;
        return report(&mut world, &run, digest, finished).map(|r| RunReport::Single(Box::new(r)));
    }

    let started = Instant::now();
    simulate_windowed(
        &mut world,
        &segment(Some(fork_at.into()), None),
        options,
        &mut host_control,
        &mut replay,
        &mut live_checkpoints,
    )?;
    let barrier_ns = world
        .machines()
        .iter()
        .map(|m| m.world.time)
        .max()
        .unwrap_or(0);
    let boot_host_seconds = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let snapshot = if seeds.len() > 1 {
        Some(WorldSnapshot::take(&mut world)?)
    } else {
        None
    };
    let snapshot_host_seconds = started.elapsed().as_secs_f64();
    let snapshot_replay = replay.clone();
    let stream = options.stream_console;
    let mut reports = Vec::new();
    for (i, &seed) in seeds.iter().enumerate() {
        replay = snapshot_replay.clone();
        let started = Instant::now();
        let restored_pages = match (i, snapshot.as_ref()) {
            (0, _) | (_, None) => 0,
            (_, Some(snapshot)) => snapshot.restore(&mut world)?,
        };
        let restore_host_seconds = started.elapsed().as_secs_f64();
        let dir = if seeds.len() > 1 {
            format!("{out}/seed-{seed}")
        } else {
            out.clone()
        };
        if seeds.len() > 1 {
            reopen_outputs(&mut world, &dir, stream)?;
        }
        let started = Instant::now();
        let (kills, reseed) = release(&mut world, cfg.seed_plan(), &names, seed, release_skew)?;
        live_checkpoints.kills = kills.clone();
        live_checkpoints.boot = Some(BootReport {
            seed: boot_seed,
            barrier_ns,
            fork_at_ns: fork_at,
            release_skew_ns: release_skew,
            boot_host_seconds,
            forked: i > 0,
            restored_pages,
            restore_host_seconds,
            reseed: reseed.clone(),
        });
        let run = segment(None, None);
        let (outcome, scheduler) = simulate_windowed(
            &mut world,
            &run,
            options,
            &mut host_control,
            &mut replay,
            &mut live_checkpoints,
        )?;
        let finished = Finished {
            outcome,
            scheduler,
            seed,
            kills: &kills,
            host_seconds: started.elapsed().as_secs_f64(),
            out: &dir,
        };
        replay.finish()?;
        let mut r = report(&mut world, &run, digest, finished)?;
        r.boot = Some(BootReport {
            seed: boot_seed,
            barrier_ns,
            fork_at_ns: fork_at,
            release_skew_ns: release_skew,
            boot_host_seconds,
            forked: i > 0,
            restored_pages,
            restore_host_seconds,
            reseed,
        });
        reports.push(r);
    }
    Ok(match <[_; 1]>::try_from(reports) {
        Ok([single]) => RunReport::Single(Box::new(single)),
        Err(seeds) => RunReport::Seeds {
            report_version: REPORT_VERSION,
            boot: BootSummary {
                seed: boot_seed,
                barrier_ns,
                boot_host_seconds,
                snapshot_host_seconds,
            },
            seeds,
        },
    })
}

/// Open a machine's console and management logs under `out`: their chunk
/// indexes and, when streaming, the raw logs, which start with the bytes the
/// machine has already written.
fn open_outputs(mach: &mut Machine, out: &str, stream: bool) -> Result<(), String> {
    if mach.observations.is_some() {
        return Ok(());
    }
    for (uart, ext) in [(&mut mach.com1, "console"), (&mut mach.com2, "mgmt")] {
        uart.output_file = uart.chunks.reopen_stream(
            &format!("{out}/{}.{ext}", mach.world.name),
            &uart.out,
            stream,
        )?;
    }
    Ok(())
}

/// Move every machine's logs to a new output directory `out`, for a branch.
fn reopen_outputs(world: &mut World, out: &str, stream: bool) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    for mach in world.machines_mut() {
        open_outputs(mach, out, stream)?;
    }
    Ok(())
}

/// How one run segment ended, for its report.
struct Finished<'a> {
    outcome: RunOutcome,
    /// The scheduler's report section.
    scheduler: Value,
    seed: u64,
    kills: &'a [Kill],
    host_seconds: f64,
    /// Where the consoles (and with `--digest` page hashes) are written.
    out: &'a str,
}

/// The network witness: every frame still in flight, the fault network
/// state and the legacy partitions.
fn network_witness(world: &World) -> sha2::digest::Output<Sha256> {
    let mut network_hash = Sha256::new();
    network_hash.update(b"seedbox-network-v1");
    let mut frames = Vec::new();
    for (to, events) in world.events().iter().enumerate() {
        for (&(at, seq), payload) in events {
            if let Payload::Frame { from, bytes } = payload {
                frames.push((at, seq, *from, to, Sha256::digest(bytes)));
            }
        }
    }
    frames.sort_by_key(|f| (f.0, f.1, f.2, f.3));
    network_hash.update((frames.len() as u64).to_le_bytes());
    for (at, seq, from, to, bytes_hash) in frames {
        network_hash.update(at.to_le_bytes());
        network_hash.update(seq.to_le_bytes());
        links::pair_hash(&mut network_hash, (from, to));
        network_hash.update(bytes_hash);
    }
    world.faults().network_witness(&mut network_hash);
    let legacy_cuts: Vec<_> = world.partitions().iter().filter(|(_, on)| **on).collect();
    network_hash.update((legacy_cuts.len() as u64).to_le_bytes());
    for (&pair, _) in legacy_cuts {
        links::pair_hash(&mut network_hash, pair);
    }
    network_hash.finalize()
}

/// The world witness's world-level evidence, before each machine's.
fn world_witness_prefix(world: &World, network_witness: &[u8]) -> Result<Sha256, String> {
    let mut world_witness = Sha256::new();
    world.control().triggers.hash_into(&mut world_witness)?;
    // Only a run that uses link policy carries the network witness, so a run
    // without it keeps the world witness it had before link policy existed.
    if world.faults().links.in_use() || world.faults().connections.used {
        world_witness.update(network_witness);
    }
    if world.control().controller.is_some() {
        world_witness.update(serde_json::to_vec(&world.control().log).map_err(|e| e.to_string())?);
    }
    if !world.faults().log.is_empty() {
        world_witness.update(serde_json::to_vec(&world.faults().log).map_err(|e| e.to_string())?);
        world_witness.update(world.delivered().to_le_bytes());
        world_witness.update(world.dropped().to_le_bytes());
        world_witness
            .update(serde_json::to_vec(&world.network_snapshot()).map_err(|e| e.to_string())?);
    }
    Ok(world_witness)
}

/// Milliseconds from host nanoseconds.
fn ms(ns: u128) -> f64 {
    ns as f64 / 1e6
}

/// The `kvm_ns` and `kvm_n` slot of port I/O exits.
const EXIT_PIO: usize = 2;
/// Reported exit kinds and their `kvm_ns` slots.
const EXIT_NAMES: [(&str, usize); 6] = [
    ("eintr", 0),
    ("pio", EXIT_PIO),
    ("mmio", 3),
    ("hlt", 4),
    ("window", 5),
    ("other", 6),
];

/// One machine's report row: its outcome evidence and diagnostics. With
/// `digest`, its page hashes are written to `out` too.
fn machine_report(
    world: &mut World,
    index: usize,
    witness: String,
    digest: bool,
    out: &str,
) -> Result<MachineReport, String> {
    let (mach, incarnations) = world.machine_with_history(index);
    let (state_digest, state_digest_parts) = if digest {
        let (d, parts) = mach.state_digest();
        // The host supplies output/MACHINE_OUTPUT_DIR as --out; retention
        // preserves that directory prefix (see seedbox/src/vmm/mod.rs).
        std::fs::write(
            format!("{out}/{}.pages", mach.world.name),
            mach.page_hashes(),
        )
        .map_err(|e| e.to_string())?;
        let parts = parts.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        (Some(d), Some(parts))
    } else {
        (None, None)
    };
    let s = &mach.stats;
    let crashed_incarnations = incarnations
        .iter()
        .map(|old| serde_json::from_value(old.clone()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("crashed incarnation summary: {e}"))?;
    let pio_total = s.kvm_n[EXIT_PIO]
        + incarnations
            .iter()
            .filter_map(|old| old["pio_total"].as_u64())
            .sum::<u64>();
    let outcome = MachineOutcome {
        name: mach.world.name.clone(),
        state: seedbox_engine_core::report::machine_state(
            mach.world.state,
            mach.world.is_paused(),
            mach.world.is_down(),
        ),
        time_ns: mach.world.time,
        guest_exit_code: mach.guest_exit_code,
        guest_init_error: mach.guest_init_error,
        infrastructure_error: matches!(mach.world.state, State::Exited(_))
            && mach.guest_exit_code.is_none(),
        witness,
        state_digest,
        state_digest_parts,
        pending_io: mach.pending_io.as_ref().map(|p| PendingIo {
            remaining_ns: p.remaining_ns,
            direction: if p.write {
                IoDirection::Out
            } else {
                IoDirection::In
            },
            port: p.port,
            width: p.width,
            count: p.count,
            bytes: p.bytes.clone(),
        }),
        crashed_incarnations,
    };
    let timer = TimerReport {
        writes: s.timer_writes,
        overdue: s.timer_overdue,
        overdue_max_ns: s.timer_overdue_max_ns,
        overdue_syscalls_max: s.timer_write_syscalls_max,
        guest_clock_skew: machine::Site::NAMES
            .iter()
            .zip(&s.skew)
            .map(|(k, v)| {
                let skew = SkewReport {
                    n: v.n,
                    ahead: v.ahead,
                    ahead_max_ns: v.ahead_max_ns,
                    behind: v.behind,
                    behind_max_ns: v.behind_max_ns,
                };
                (k.to_string(), skew)
            })
            .collect(),
    };
    Ok(MachineReport {
        outcome,
        incarnation: mach.world.incarnation(),
        booted_at_ns: mach.world.booted_at(),
        exits: s.exits,
        pio: s.pio,
        pio_total,
        mmio: s.mmio,
        halts: s.halts,
        irqs: s.irqs,
        window_exits: s.window_exits,
        eintr: s.eintr,
        run_ms: ms(s.run_ns),
        stalls: s.stalls,
        yields: s.yields,
        yield_stops: s.yield_stops,
        shadow: ShadowReport {
            synced_pages: mach.shadow.stats.synced_pages,
            restored_pages: mach.shadow.stats.restored_pages,
        },
        kvm_ms_by_exit: EXIT_NAMES
            .iter()
            .map(|&(k, i)| (k.to_string(), ms(s.kvm_ns[i])))
            .collect(),
        kvm_n_by_exit: s.kvm_n.to_vec(),
        control_channel: mach.channel.as_ref().map(|_| ControlChannelReport {
            transport: "virtio-console-v1".into(),
            tx_bytes: mach.control_tx_bytes,
            rx_dma_bytes: mach.control_dma_bytes,
        }),
        net: mach.net.as_ref().map(|n| n.report()),
        disk: mach.blk.as_ref().map(|b| {
            let (disk, stats) = (b.disk(), b.stats());
            DiskReport {
                fault_model: disk.fault_model(),
                capacity_sectors: disk.capacity(),
                base_sha256: disk.base_sha256().map(str::to_owned),
                digest: disk.digest(),
                stable_sectors: disk.stable_sectors(),
                volatile_sectors: disk.volatile_sectors(),
                reads: stats.reads,
                writes: stats.writes,
                flushes: stats.flushes,
                sectors_read: stats.sectors_read,
                sectors_written: stats.sectors_written,
                sectors_flushed: stats.sectors_flushed,
                errors: stats.errors,
            }
        }),
        timer,
        syscalls: mach.syscalls(),
        syscall_ns: mach.syscall_ns,
        time_ns_by_cause: machine::Cause::NAMES
            .iter()
            .zip(s.time_by)
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    })
}

/// Finish a run: write consoles and build the engine report.
fn report(
    world: &mut World,
    run: &world_windowed::Run,
    digest: bool,
    finished: Finished,
) -> Result<EngineReport, String> {
    let Finished {
        outcome,
        scheduler,
        seed,
        kills,
        host_seconds,
        out,
    } = finished;
    let slices = world.slices();
    world.finish(
        outcome.as_str(),
        run.duration,
        &run.end_on.map(str::to_owned),
    )?;
    let triggers = Some(world.control().triggers.records.clone()).filter(|r| !r.is_empty());
    let fault_error = world
        .faults()
        .error
        .clone()
        .or(world.control().error.clone());
    let network_witness = network_witness(world);
    let mut world_witness = world_witness_prefix(world, &network_witness)?;
    let mut machines = Vec::new();
    for index in 0..world.machines().len() {
        let mach = &mut world.machines_mut()[index];
        if let Some(t) = mach.trace.as_mut() {
            use std::io::Write;
            let _ = t.flush();
        }
        let witness = mach.exit_witness();
        world_witness.update(witness.as_bytes());
        let row = machine_report(world, index, witness, digest, out)?;
        if let Some(d) = &row.outcome.state_digest {
            world_witness.update(d.as_bytes());
        }
        let mach = &world.machines()[index];
        // A disk outlives its machine's crashes: what it holds is part of
        // the outcome even without --digest.
        if let Some(b) = &mach.blk {
            world_witness.update(b.disk().digest().as_bytes());
        }
        std::fs::write(
            format!("{out}/{}.witness-log", mach.world.name),
            mach.witness_log.encode(mach.world.time, &mach.witness),
        )
        .map_err(|e| e.to_string())?;
        if mach.observations.is_none() {
            std::fs::write(format!("{out}/{}.console", mach.world.name), &mach.com1.out)
                .map_err(|e| e.to_string())?;
            std::fs::write(format!("{out}/{}.mgmt", mach.world.name), &mach.com2.out)
                .map_err(|e| e.to_string())?;
        }
        machines.push(row);
    }
    let sim_ns = world
        .machines()
        .iter()
        .map(|m| m.world.time)
        .max()
        .unwrap_or(0);
    let sim_seconds = sim_ns as f64 / 1e9;
    let faults = world
        .faults()
        .log
        .iter()
        .map(seedbox_engine_core::report::fault_record)
        .collect::<Result<_, _>>()?;
    let control_history = world
        .control()
        .log
        .iter()
        .map(ControlRecord::of)
        .collect::<Result<_, _>>()?;
    let to_values = |rows: &[_]| -> Result<Vec<Value>, String> {
        rows.iter()
            .map(|r| serde_json::to_value(r).map_err(|e| e.to_string()))
            .collect()
    };
    if !world.input_rejections().is_empty() {
        world_witness
            .update(serde_json::to_vec(world.input_rejections()).map_err(|e| e.to_string())?);
    }
    Ok(EngineReport {
        report_version: REPORT_VERSION,
        seed,
        deferred_tx: world.boot_inputs().iter().any(|b| b.features.deferred_tx),
        evidence: WorldEvidence {
            outcome,
            world_witness: hex::encode(world_witness.finalize()),
            faults,
            fault_error,
            input_admissions: world.input_admissions().to_vec(),
            input_rejections: world.input_rejections().to_vec(),
            control_history,
            control_latency_ns: world.control().latency,
            triggers,
        },
        sim_seconds,
        host_seconds,
        speed: Some(sim_seconds / host_seconds).filter(|s| s.is_finite()),
        slices,
        frames_delivered: world.delivered(),
        frames_dropped: world.dropped(),
        network_counters: world.network_snapshot(),
        network_witness: hex::encode(network_witness),
        kills: kills.to_vec(),
        rejected_fault_effects: to_values(&world.faults().rejected_effects)?,
        management_readiness: world.faults().readiness.clone(),
        in_flight_policy: "retain_send_time_decisions; cuts_dominate_shapes".into(),
        machines,
        scheduler,
        checkpoint: None,
        checkpoint_reseed: None,
        boot: None,
    })
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    /// Guests see the command line: these are the exact bytes `run` gave
    /// them before it had a builder.
    #[test]
    fn kernel_cmdline_bytes_are_unchanged() {
        let base = "console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init";
        let plain = Cmdline {
            name: "a",
            index: 0,
            net: false,
            channel: false,
            disk: false,
            control: false,
            extra: None,
        };
        assert_eq!(
            plain.build(),
            (
                format!("{base} seedbox.name=a seedbox.index=0"),
                String::new()
            )
        );
        let full = Cmdline {
            name: "db",
            index: 3,
            net: true,
            channel: true,
            disk: true,
            control: true,
            extra: Some("seedbox.blk=ext4"),
        };
        assert_eq!(
            full.build(),
            (
                format!(
                    "{base} seedbox.name=db seedbox.index=3 virtio_mmio.device=4K@0xd0001000:5 \
                     virtio_mmio.device=4K@0xd1000000:6 virtio_mmio.device=4K@0xd0002000:7"
                ),
                " seedbox.control=1 seedbox.blk=ext4".to_owned()
            )
        );
    }
}

#[cfg(test)]
mod worker_options_tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn debug_env_is_read_once_and_applied_to_the_request() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let none = DebugEnv::from_env(env(&[])).unwrap();
        assert_eq!(none, DebugEnv::default());
        let mut request = WorkerRequest::new("w".into(), 0, "o".into());
        assert!(none.apply(&mut request).is_none());
        assert_eq!(request.performance, Performance::default());
        let debug = DebugEnv::from_env(env(&[
            ("SEEDBOX_WATCHDOG_MS", "5"),
            ("SEEDBOX_WINDOWED_SKEW_US", "300"),
            ("SEEDBOX_TRACE_CONSOLE", ""),
        ]))
        .unwrap();
        assert!(debug.apply(&mut request).is_some());
        assert_eq!(
            (request.performance.watchdog_ms, request.windowed_skew_us),
            (5, 300)
        );
        assert_eq!(
            DebugEnv::from_env(env(&[("SEEDBOX_WINDOWED_SKEW_US", "x")])).unwrap_err(),
            "SEEDBOX_WINDOWED_SKEW_US: expected an unsigned integer"
        );
    }

    #[test]
    fn console_trace_lines_keep_their_format() {
        let trace = ConsoleTrace {
            host_seconds: 1.5,
            guest_seconds: 0.25,
            line: b"hello",
        };
        assert_eq!(trace.to_string(), "[host 1.500s guest 0.250000s] hello");
    }

    #[test]
    fn console_trace_callback_is_restored_when_worker_scope_ends() {
        fn callback(_: ConsoleTrace<'_>) {}
        assert!(!console_trace_enabled());
        {
            let _guard =
                ConsoleTraceGuard(CONSOLE_TRACE.with(|trace| trace.replace(Some(callback))));
            assert!(console_trace_enabled());
            emit_console_trace(1000, b"line");
        }
        assert!(!console_trace_enabled());
    }
}
