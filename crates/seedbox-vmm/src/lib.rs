//! Seedbox world: several Linux machines, a network fabric and a scenario,
//! all executed by one host thread. A seed and the config fully determine the run.

#![recursion_limit = "256"]

mod boundary;
mod management_uart;
mod output_layout;
#[cfg(test)]
use seedbox_engine_core::faults;
#[cfg(test)]
use seedbox_engine_core::world::{replay_readiness, stop_progress};
use seedbox_engine_core::{checkpoint, control, disk, links, network_counters, rng};
mod counter;
mod devices;
mod host_control;
mod machine;
mod profile;
mod stepper;
mod timebase;
mod undo;
mod virtio_blk;
mod virtio_console;
mod virtio_net;
mod world_adapter;

use counter::Counter;
use machine::pv::PvClock;
use machine::{
    BootSpec, Machine, State, BLK_IRQ, BLK_MMIO, CHANNEL_IRQ, CHANNEL_MMIO, NET_IRQ, NET_MMIO,
};
use rng::Rng;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

thread_local! {
    static START: Instant = Instant::now();
    static CONSOLE_TRACE: Cell<Option<ConsoleTraceCallback>> = const { Cell::new(None) };
}

/// Performance choices and optional diagnostics; they do not change guest results.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerformanceOptions {
    pub undo_margin: u64,
    /// None selects the adaptive stepping margin.
    pub margin: Option<u64>,
    pub pv_watchdog_ms: u64,
    /// Windowed scheduler testing only: delay machine i's runs by
    /// (machines - i) times this many microseconds, so later machines finish first.
    pub windowed_skew_us: u64,
}

impl Default for PerformanceOptions {
    fn default() -> Self {
        Self {
            undo_margin: 256,
            margin: None,
            pv_watchdog_ms: 20,
            windowed_skew_us: 0,
        }
    }
}

impl PerformanceOptions {
    /// Interpret values supplied by the process boundary, without reading its environment.
    pub fn parse(undo: Option<&str>, margin: Option<&str>) -> Result<Self, String> {
        let undo_margin = match undo {
            None => 256,
            Some(m) => m.parse().map_err(|e| format!("SEEDBOX_UNDO: {e}"))?,
        };
        let margin = match margin {
            Some("adaptive") | None => None,
            Some(m) => Some(m.parse().map_err(|e| format!("margin: {e}"))?),
        };
        Ok(Self {
            undo_margin,
            margin,
            ..Self::default()
        })
    }
}

/// One console line observed at the host diagnostic boundary.
pub struct ConsoleTrace<'a> {
    pub host_seconds: f64,
    pub guest_seconds: f64,
    pub branches: u64,
    pub line: &'a [u8],
}

pub type ConsoleTraceCallback = for<'a> fn(ConsoleTrace<'a>);

/// Explicit inputs to a world worker. Argument and environment acquisition belong to its caller.
#[derive(Clone, Debug)]
pub struct WorkerOptions {
    pub config: PathBuf,
    pub seed: u64,
    /// Further seeds released from the same booted world after `seed`.
    pub fork_seeds: Vec<u64>,
    pub out: PathBuf,
    pub digest: bool,
    pub host_control: Option<PathBuf>,
    pub console_limit: Option<usize>,
    pub log_chunk_limit: Option<usize>,
    pub pv_clock: bool,
    pub page_timer: bool,
    pub deferred_tx: bool,
    pub checkpoint_at_ns: Option<String>,
    pub stream_console: bool,
    pub trace: bool,
    pub performance: PerformanceOptions,
    pub console_trace: Option<ConsoleTraceCallback>,
    /// Machine threads for the windowed scheduler (performance only;
    /// docs/windowed-scheduler.md).
    pub machine_workers: usize,
    /// One CPU per machine thread, in thread order (performance only).
    pub machine_cpus: Vec<usize>,
}

impl WorkerOptions {
    /// Parse the standalone worker's existing flag protocol from caller-supplied arguments.
    pub fn from_args(args: &[String], performance: PerformanceOptions) -> Result<Self, String> {
        let config = arg(args, "--config").ok_or("--config required")?.into();
        let seed = arg(args, "--seed")
            .ok_or("--seed required")?
            .parse()
            .map_err(|e| format!("seed: {e}"))?;
        let mut fork_seeds = Vec::new();
        if let Some(list) = arg(args, "--fork-seeds") {
            for s in list.split(',') {
                fork_seeds.push(s.parse().map_err(|e| format!("--fork-seeds: {e}"))?);
            }
        }
        let out = arg(args, "--out").ok_or("--out required")?.into();
        let console_limit = arg(args, "--console-limit")
            .map(|limit| {
                limit
                    .parse::<usize>()
                    .map_err(|error| format!("console limit: {error}"))
            })
            .transpose()?;
        let pv_clock = match arg(args, "--timebase").as_deref() {
            None | Some("counter") => false,
            Some("pv") => true,
            Some(other) => return Err(format!("--timebase {other}: expected counter or pv")),
        };
        let log_chunk_limit = arg(args, "--log-chunk-limit")
            .map(|v| v.parse::<usize>())
            .transpose()
            .map_err(|e| e.to_string())?;
        let checkpoint_at_ns = arg(args, "--checkpoint-at-ns");
        if args.iter().any(|a| a == "--checkpoint-at-ns") && checkpoint_at_ns.is_none() {
            return Err("--checkpoint-at-ns requires comma-separated unsigned nanoseconds".into());
        }
        let machine_workers = match arg(args, "--machine-workers") {
            None => 1,
            Some(n) => n
                .parse()
                .ok()
                .filter(|n| (1..=64).contains(n))
                .ok_or("--machine-workers: use 1..64")?,
        };
        let machine_cpus: Vec<usize> = match arg(args, "--machine-cpus") {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .map(|c| c.parse().map_err(|e| format!("--machine-cpus: {e}")))
                .collect::<Result<_, _>>()?,
        };
        if !machine_cpus.is_empty() && machine_cpus.len() != machine_workers {
            return Err("--machine-cpus: give one CPU per machine worker".into());
        }
        Ok(Self {
            config,
            seed,
            fork_seeds,
            out,
            pv_clock,
            log_chunk_limit,
            checkpoint_at_ns,
            machine_workers,
            machine_cpus,
            page_timer: args.iter().any(|a| a == "--page-timer"),
            deferred_tx: args.iter().any(|a| a == "--deferred-tx"),
            digest: args.iter().any(|a| a == "--digest"),
            host_control: arg(args, "--host-control").map(PathBuf::from),
            console_limit,
            stream_console: args.iter().any(|a| a == "--stream-console"),
            trace: args.iter().any(|a| a == "--trace"),
            performance,
            console_trace: None,
        })
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

pub(crate) fn emit_console_trace(time: u64, branches: u64, line: &[u8]) {
    CONSOLE_TRACE.with(|trace| {
        if let Some(callback) = trace.get() {
            callback(ConsoleTrace {
                host_seconds: START.with(|s| s.elapsed().as_secs_f64()),
                guest_seconds: time as f64 / 1e9,
                branches,
                line,
            });
        }
    });
}

/// The world's timebase (see `timebase.rs`), one for all its machines.
pub enum Clock {
    Counter(Counter),
    Pv(PvClock),
}

impl Clock {
    fn name(&self) -> &'static str {
        match self {
            Clock::Counter(_) => "counter",
            Clock::Pv(_) => "pv",
        }
    }
}

/// A read-only private mapping of a guest image. Boot copies it into guest
/// RAM straight from the page cache instead of reading it into the heap first.
pub(crate) struct MappedFile {
    ptr: *const u8,
    len: usize,
}

impl MappedFile {
    fn open(path: &str) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(path)?;
        let len = usize::try_from(file.metadata()?.len())
            .map_err(|_| std::io::Error::other("file too large"))?;
        if len == 0 {
            return Ok(Self {
                ptr: std::ptr::NonNull::dangling().as_ptr(),
                len,
            });
        }
        // SAFETY: a fresh private read-only mapping of an open file; the
        // retained inputs are read-only and never written while a run executes.
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
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }
}

impl std::ops::Deref for MappedFile {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: `ptr` maps `len` readable bytes until drop (or is dangling with len 0).
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        if self.len != 0 {
            unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
        }
    }
}

/// Everything a machine boots from. A reboot starts a new incarnation from
/// exactly these inputs, with its own entropy and timer seeds.
#[derive(Clone)]
pub struct BootInputs {
    kernel: Rc<[u8]>,
    initrd: Rc<MappedFile>,
    /// The command line before and after the launch barrier flag.
    cmdline: (String, String),
    mem: usize,
    mac: Option<[u8; 6]>,
    control_channel: bool,
    /// The disk as configured, with nothing written. Only the first
    /// incarnation boots from it; a reboot inherits its predecessor's.
    disk: Option<disk::Disk>,
    undo_margin: u64,
    tolerance: u64,
    timer_jitter: u64,
    exact_timer_ppm: u64,
    /// A fixed stepping margin; None adapts it.
    margin: Option<u64>,
    console_limit: Option<usize>,
    /// The machine runs under the PV clock timebase.
    pv_clock: bool,
    page_timer: bool,
    deferred_tx: bool,
    log_chunk_limit: Option<usize>,
}

/// A machine's `disk`: `{"size_mib": N}` for a zeroed disk, or
/// `{"image": PATH}` for a raw image (with `size_mib`, it must be that size).
fn machine_disk(
    cfg: &Value,
    path: &dyn Fn(&Value) -> std::path::PathBuf,
) -> Result<Option<disk::Disk>, String> {
    if cfg.is_null() {
        return Ok(None);
    }
    let fields = cfg.as_object().ok_or("disk must be an object")?;
    if let Some(key) = fields
        .keys()
        .find(|k| !matches!(k.as_str(), "size_mib" | "image"))
    {
        return Err(format!("disk: unknown field {key}"));
    }
    let sectors = match &cfg["size_mib"] {
        Value::Null => None,
        v => Some(
            v.as_u64()
                .filter(|&m| m > 0 && m <= 1 << 20)
                .ok_or("disk.size_mib must be 1..=1048576")?
                * 2048,
        ),
    };
    let image = match &cfg["image"] {
        Value::Null => None,
        v if v.is_string() => {
            let p = path(v);
            Some(std::fs::read(&p).map_err(|e| format!("disk image {}: {e}", p.display()))?)
        }
        _ => return Err("disk.image must be a path".into()),
    };
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
        undo_margin: b.undo_margin,
        barrier,
        pv_clock: b.pv_clock,
        page_timer: b.page_timer,
    };
    let mut mach = Machine::new(kvm, name, &spec)?;
    if let Some(net) = mach.net.as_mut() {
        net.set_deferred_tx(b.deferred_tx);
    }
    mach.com1.output_limit = b.console_limit;
    mach.com2.output_limit = b.console_limit;
    mach.com1.chunks.limit = b.log_chunk_limit;
    mach.com2.chunks.limit = b.log_chunk_limit;
    mach.tolerance = b.tolerance;
    mach.timer_jitter = b.timer_jitter;
    mach.exact_timer_ppm = b.exact_timer_ppm;
    match b.margin {
        None => mach.adaptive_margin = true,
        Some(m) => mach.margin = m,
    }
    Ok(mach)
}

use seedbox_engine_core::world::{
    self as world_core, arm_seed, boot_faults, check_plan_after, entropy_seed, Payload,
};
use seedbox_engine_core::world_windowed;
type World = world_core::World<Machine>;
type WorldSnapshot = world_core::WorldSnapshot<Machine>;

/// One segment of the run under the windowed scheduler, on one thread or
/// the machine worker pool. Returns the outcome word and the scheduler's
/// report section.
fn simulate_windowed(
    world: &mut World,
    run: &world_windowed::Run,
    options: &WorkerOptions,
    host_control: &mut Option<host_control::HostControl>,
) -> Result<(&'static str, Value), String> {
    let mut boundary = || {
        if let Some(control) = host_control.as_mut() {
            control
                .boundary()
                .map_err(|e| format!("host control: {e}"))?;
        }
        Ok(())
    };
    let skew = std::time::Duration::from_micros(options.performance.windowed_skew_us);
    if options.machine_workers <= 1 {
        return world_windowed::simulate(world, run, skew, &mut boundary);
    }
    // Each pool thread opens its own clock: the counter counts that thread's
    // guest execution, and the PV watchdog that thread's CPU time.
    let pv = matches!(world.clock, Clock::Pv(_));
    let watchdog = std::time::Duration::from_millis(options.performance.pv_watchdog_ms.max(1));
    let cpus = &options.machine_cpus;
    let workers = world_windowed::Workers {
        count: options.machine_workers,
        skew,
    };
    world_windowed::simulate_parallel(
        world,
        run,
        workers,
        |w| {
            if let Some(&cpu) = cpus.get(w) {
                pin(cpu)?;
            }
            Ok(if pv {
                Clock::Pv(PvClock::new(watchdog)?)
            } else {
                Clock::Counter(Counter::new()?)
            })
        },
        &mut boundary,
    )
}
/// Run the world to its end, or with `fork` to its fork point, on one thread.
#[cfg(test)]
fn simulate(
    world: &mut World,
    cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    host_control: &mut Option<host_control::HostControl>,
) -> Result<&'static str, String> {
    simulate_to(world, cfg, duration, end_on, fork, None, host_control)
}
/// As [`simulate`], stopping at `checkpoint`.
#[cfg(test)]
fn simulate_to(
    world: &mut World,
    cfg: &Value,
    duration: u64,
    end_on: &Option<String>,
    fork: Option<Option<u64>>,
    checkpoint: Option<u64>,
    host_control: &mut Option<host_control::HostControl>,
) -> Result<&'static str, String> {
    let run = world_windowed::Run {
        cfg,
        duration,
        end_on,
        fork,
        checkpoint,
        require_barrier: true,
    };
    let options = WorkerOptions {
        config: PathBuf::new(),
        seed: 0,
        fork_seeds: vec![],
        out: PathBuf::new(),
        digest: false,
        host_control: None,
        console_limit: None,
        log_chunk_limit: None,
        pv_clock: false,
        page_timer: false,
        deferred_tx: false,
        checkpoint_at_ns: None,
        stream_console: false,
        trace: false,
        performance: PerformanceOptions::default(),
        console_trace: None,
        machine_workers: 1,
        machine_cpus: vec![],
    };
    simulate_windowed(world, &run, &options, host_control).map(|(word, _)| word)
}

/// Pin the calling thread to one CPU.
fn pin(cpu: usize) -> Result<(), String> {
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(cpu, &mut set);
        if libc::sched_setaffinity(0, std::mem::size_of_val(&set), &set) != 0 {
            return Err(format!(
                "pin machine worker to CPU {cpu}: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}
#[cfg(test)]
use world_core::drain_machine_messages;
use world_core::reseed_checkpoint;

fn release(
    world: &mut World,
    cfg: &Value,
    names: &[String],
    seed: u64,
    skew: u64,
) -> Result<(Vec<Value>, Value), String> {
    world_core::release(world, cfg, names, seed, skew, &mut |name, abi| {
        eprintln!("{}: guest ABI {} predates time-page reseeding; its CRNG is not reseeded at this time fork",name,abi);
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// Execute a world and return its engine report; the caller owns stdout, stderr and exit status.
pub fn run(options: &WorkerOptions) -> Result<Value, String> {
    START.with(|_| ());
    let _console_trace =
        ConsoleTraceGuard(CONSOLE_TRACE.with(|trace| trace.replace(options.console_trace)));
    let config_path = &options.config;
    let seed = options.seed;
    let mut seeds = vec![seed];
    seeds.extend_from_slice(&options.fork_seeds);
    let out = options.out.to_string_lossy().into_owned();
    let digest = options.digest;
    let pv_clock = options.pv_clock;
    let page_timer = options.page_timer;
    let deferred_tx = options.deferred_tx;
    // Undo-able precise stops (performance only): initial final margin, 0 = off.
    let undo_margin = options.performance.undo_margin;
    std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
    let mut host_control = options
        .host_control
        .as_deref()
        .map(host_control::HostControl::bind)
        .transpose()
        .map_err(|e| format!("host control: {e}"))?;
    let cfg: Value = serde_json::from_slice(
        &std::fs::read(&config_path).map_err(|e| format!("{}: {e}", config_path.display()))?,
    )
    .map_err(|e| format!("config: {e}"))?;
    let disk_fault_model = disk::parse_disk_fault_model(&cfg["disk_loss"])?;
    let base = config_path.parent().unwrap().to_path_buf();
    let path = |v: &Value| base.join(v.as_str().unwrap());
    let kernel = std::fs::read(path(&cfg["kernel"])).map_err(|e| format!("kernel: {e}"))?;
    let mem = cfg["mem_mb"].as_u64().unwrap_or(512) as usize * 1024 * 1024;
    let duration = cfg["duration_ns"].as_u64().unwrap_or(60_000_000_000);
    let checkpoint_list = &options.checkpoint_at_ns;
    let checkpoint_times = if let Some(list) = checkpoint_list.as_deref() {
        if !cfg["checkpoint"].is_null() {
            return Err("--checkpoint-at-ns cannot override config.checkpoint".into());
        }
        checkpoint::times(list, duration)?
    } else if !cfg["checkpoint"].is_null() {
        let t = cfg["checkpoint"]["at_ns"]
            .as_u64()
            .ok_or("checkpoint.at_ns required")?;
        seeds = vec![cfg["checkpoint"]["branch_seed"]
            .as_u64()
            .ok_or("checkpoint.branch_seed required")?];
        if !options.fork_seeds.is_empty() {
            return Err("--fork-seeds cannot override config.checkpoint".into());
        }
        checkpoint::times(&t.to_string(), duration)?
    } else {
        Vec::new()
    };
    if !checkpoint_times.is_empty() {
        if cfg["boot"].is_object() {
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
    let end_on: Option<String> = cfg["end_on_exit"].as_str().map(String::from);
    let names: Vec<String> = cfg["machines"]
        .as_array()
        .ok_or("machines")?
        .iter()
        .map(|m| m["name"].as_str().unwrap().to_string())
        .collect();
    let networked = cfg["network"].is_object();
    let control_channel = cfg["control_channel"] == "virtio-console-v1";
    if !cfg["control"].is_null() && !control_channel {
        return Err("runtime control requires virtio-console-v1 channel".into());
    }
    if !cfg["control_channel"].is_null() && !control_channel {
        return Err("unknown runtime control channel".into());
    }

    // Boot once under a fixed boot seed and park every machine at its launch
    // barrier; the run seed applies from the release onwards. The world at
    // the barrier is the same for every run seed, so it can be forked.
    let boot = &cfg["boot"];
    let forked = boot.is_object();
    let boot_seed = if forked {
        boot["seed"].as_u64().ok_or("boot.seed required")?
    } else {
        seed
    };
    let release_skew = boot["release_skew_ns"].as_u64().unwrap_or(0);
    // Fork at a simulated time instead of the guests' launch barrier: every
    // machine parks at its first deterministic point at or after it.
    let fork_at = boot["fork_at_ns"].as_u64();
    if fork_at.is_some_and(|t| t >= duration) {
        return Err("boot.fork_at_ns must be before duration_ns".into());
    }
    if let Some(t) = fork_at {
        // A barrier fork's release time is known only after boot; checked
        // again at the release.
        check_plan_after(&cfg, t.saturating_add(release_skew))?;
    }
    if seeds.len() > 1 && !forked && checkpoint_times.is_empty() {
        return Err("--fork-seeds requires boot.seed in the config".into());
    }
    if seeds.len() > 1 && options.trace {
        return Err("--trace cannot be combined with --fork-seeds".into());
    }
    let kvm = kvm_ioctls::Kvm::new().map_err(|e| format!("kvm: {e}"))?;
    let kernel: Rc<[u8]> = kernel.into();
    let margin = options.performance.margin;
    let console_limit = options.console_limit;
    let barrier = forked && fork_at.is_none();
    let mut machines = Vec::new();
    let mut macs = Vec::new();
    let mut boots = Vec::new();
    let mut initrds: BTreeMap<String, Rc<MappedFile>> = BTreeMap::new();
    for (i, mc) in cfg["machines"].as_array().unwrap().iter().enumerate() {
        let ipath = path(&mc["initrd"]).to_string_lossy().to_string();
        if !initrds.contains_key(&ipath) {
            initrds.insert(
                ipath.clone(),
                Rc::new(MappedFile::open(&ipath).map_err(|e| format!("{ipath}: {e}"))?),
            );
        }
        let mac = [0x52, 0x54, 0x00, 0x00, 0x00, i as u8 + 1];
        macs.push(mac);
        let mut cmdline = format!(
            "console=ttyS0 nokaslr noapic nolapic acpi=off io_delay=none tsc=reliable tsc_early_khz=1000000 lpj=1000000 \
             clocksource=tsc random.trust_cpu=off random.trust_bootloader=on panic=-1 loglevel=4 rdinit=/init \
             seedbox.name={} seedbox.index={}",
            names[i], i
        );
        if networked {
            cmdline += &format!(" virtio_mmio.device=4K@{NET_MMIO:#x}:{NET_IRQ}");
        }
        if control_channel {
            cmdline += &format!(" virtio_mmio.device=4K@{CHANNEL_MMIO:#x}:{CHANNEL_IRQ}");
        }
        let disk = machine_disk(&mc["disk"], &path)?.map(|d| d.with_fault_model(disk_fault_model));
        if disk.is_some() {
            cmdline += &format!(" virtio_mmio.device=4K@{BLK_MMIO:#x}:{BLK_IRQ}");
        }
        let mut after = String::new();
        if names
            .get(i)
            .is_some_and(|name| cfg["control"]["machine"].as_str() == Some(name.as_str()))
        {
            after += " seedbox.control=1";
        }
        if let Some(extra) = mc["cmdline"].as_str() {
            after += " ";
            after += extra;
        }
        let boot = BootInputs {
            kernel: kernel.clone(),
            initrd: initrds[&ipath].clone(),
            cmdline: (cmdline, after),
            mem,
            mac: networked.then_some(mac),
            control_channel,
            disk,
            undo_margin,
            tolerance: cfg["stop_tolerance_ns"].as_u64().unwrap_or(0),
            timer_jitter: cfg["timer_jitter_ns"].as_u64().unwrap_or(0),
            exact_timer_ppm: cfg["exact_timer_ppm"].as_u64().unwrap_or(0),
            margin,
            console_limit,
            pv_clock,
            page_timer,
            deferred_tx,
            log_chunk_limit: options.log_chunk_limit,
        };
        let seed = entropy_seed(boot_seed, &format!("boot-entropy/{}", names[i]));
        let mut mach = boot_machine(&kvm, &names[i], &boot, barrier, seed)?;
        boots.push(boot);
        if options.stream_console {
            mach.com1.output_file = Some(
                std::fs::File::create(format!("{out}/{}.console", names[i]))
                    .map_err(|error| error.to_string())?,
            );
            mach.com2.output_file = Some(
                std::fs::File::create(format!("{out}/{}.mgmt", names[i]))
                    .map_err(|error| error.to_string())?,
            );
        }
        mach.com1
            .chunks
            .open(&devices::log_index::index_name(&format!(
                "{out}/{}.console",
                names[i]
            )));
        mach.com2
            .chunks
            .open(&devices::log_index::index_name(&format!(
                "{out}/{}.mgmt",
                names[i]
            )));
        mach.timer_rng = Some(Rng::new(boot_seed, &format!("timer-jitter/{}", names[i])));
        mach.syscall_ns =
            machine::pv::syscall_ns(&mut Rng::new(boot_seed, &format!("pv-speed/{}", names[i])));
        if options.trace {
            let f = std::fs::File::create(format!("{out}/{}.trace", names[i]))
                .map_err(|e| e.to_string())?;
            mach.trace = Some(std::io::BufWriter::new(f));
        }
        machines.push(mach);
    }
    if !cfg["control"].is_null()
        && (!cfg["faults"].as_array().is_none_or(Vec::is_empty)
            || !cfg["scenario"].as_array().is_none_or(Vec::is_empty)
            || cfg["kill_restart"].is_object())
    {
        return Err("runtime control cannot mix with static/legacy scenarios".into());
    }
    if !cfg["control"].is_null() && cfg["control"]["machine"] != cfg["end_on_exit"] {
        return Err("controller must also be the end_on_exit machine".into());
    }
    let n = machines.len();
    let net = &cfg["network"];
    let latency = net["latency_ns"].as_u64().unwrap_or(1_000_000);
    let early = (latency / 2).min(cfg["stop_tolerance_ns"].as_u64().unwrap_or(0));
    let clock = if pv_clock {
        // Host CPU time a run may go without an exit before it is checked
        // for a spin (performance only).
        let ms = options.performance.pv_watchdog_ms;
        Clock::Pv(PvClock::new(std::time::Duration::from_millis(ms.max(1)))?)
    } else {
        Clock::Counter(Counter::new()?)
    };
    let mut world = World {
        failure: None,
        factory: kvm,
        boot: boots,
        barrier,
        boot_seed,
        run_seed: seed,
        branch_identity: None,
        incarnations: vec![Vec::new(); n],
        paused_ingress: vec![0; n],
        clock,
        events: (0..n).map(|_| BTreeMap::new()).collect(),
        seq: 0,
        lookahead: latency,
        early,
        latency,
        partitioned: BTreeMap::new(),
        // Faults, control and scenarios belong to the run seed: armed below,
        // or at the release of a booted world.
        faults: boot_faults(
            &names,
            &[],
            latency,
            links::Links::from_config(boot_seed, &names, net)?,
        )?,
        control: control::Control::new(&Value::Null, names.clone(), latency, early)?,
        strict_events: (0..n).map(|_| BTreeMap::new()).collect(),
        macs,
        dropped: 0,
        delivered: 0,
        link_counts: BTreeMap::new(),
        semantic_counts: network_counters::NetworkCounters::default(),
        slices: 0,
        machines,
    };
    if !forked {
        let kills = arm_seed(&mut world, &cfg, &names, seed, 0)?;
        if !checkpoint_times.is_empty() {
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            let engine = format!(
                "{:x}",
                Sha256::digest(std::fs::read(exe).map_err(|e| e.to_string())?)
            );
            let identity_timebase = checkpoint::identity_timebase(world.clock.name(), deferred_tx);
            let parent = checkpoint::parent_identity(
                &cfg,
                seed,
                &engine,
                &identity_timebase,
                page_timer,
                |p| {
                    std::fs::read(base.join(p))
                        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
                        .map_err(|e| format!("{p}: {e}"))
                },
            )?;
            if let Some(expected) = cfg["checkpoint"]["parent_identity"].as_str() {
                if expected != parent {
                    return Err(
                        "checkpoint parent identity does not match retained world inputs".into(),
                    );
                }
            }
            // Reconstruct each prefix from the same origin: an extra hard
            // stop for an earlier checkpoint must not change a later one's
            // identity or execution. The held snapshots remain independent.
            let origin = if checkpoint_times.len() > 1 {
                Some(WorldSnapshot::take_checkpoint(&mut world)?)
            } else {
                None
            };
            let mut held = Vec::new();
            for (i, &at) in checkpoint_times.iter().enumerate() {
                if i > 0 {
                    origin.as_ref().unwrap().restore_checkpoint(&mut world)?;
                }
                let run = world_windowed::Run {
                    cfg: &cfg,
                    duration,
                    end_on: &end_on,
                    fork: None,
                    checkpoint: Some(at),
                    require_barrier: true,
                };
                let (outcome, _) = simulate_windowed(&mut world, &run, options, &mut host_control)?;
                if outcome != "checkpoint" {
                    return Err(format!("checkpoint at {at} ns not reached: {outcome}"));
                }
                held.push((at, WorldSnapshot::take_checkpoint(&mut world)?));
            }
            let mut reports = Vec::new();
            for (at, snapshot) in &held {
                for &branch_seed in &seeds {
                    snapshot.restore_checkpoint(&mut world)?;
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
                    checkpoint_output(&mut world, &dir, options.stream_console)?;
                    let started = Instant::now();
                    let run = world_windowed::Run {
                        cfg: &cfg,
                        duration,
                        end_on: &end_on,
                        fork: None,
                        checkpoint: None,
                        require_barrier: true,
                    };
                    let (outcome, evidence) =
                        simulate_windowed(&mut world, &run, options, &mut host_control)?;
                    // The public report's seed remains the parent seed; the
                    // semantic branch seed and identity have explicit fields.
                    let mut r = report(
                        &mut world,
                        outcome,
                        seed,
                        &kills,
                        started.elapsed().as_secs_f64(),
                        digest,
                        &dir,
                        duration,
                        &end_on,
                    )?;
                    r["scheduler"] = evidence;
                    r["checkpoint"] = json!({"at_ns":at,"branch_seed":branch_seed,"parent_identity":parent,
                        "branch_identity":identity});
                    r["checkpoint_reseed"] = json!(reseed);
                    reports.push(r);
                }
            }
            if reports.len() == 1 {
                return Ok(reports.pop().unwrap());
            }
            return Ok(json!({"report_version":1,"parent_identity":parent,"checkpoints":reports}));
        }
        let started = Instant::now();
        let run = world_windowed::Run {
            cfg: &cfg,
            duration,
            end_on: &end_on,
            fork: None,
            checkpoint: None,
            require_barrier: true,
        };
        let (outcome, evidence) = simulate_windowed(&mut world, &run, options, &mut host_control)?;
        let host_seconds = started.elapsed().as_secs_f64();
        let mut r = report(
            &mut world,
            outcome,
            seed,
            &kills,
            host_seconds,
            digest,
            &out,
            duration,
            &end_on,
        )?;
        r["scheduler"] = evidence;
        return Ok(r);
    }

    let started = Instant::now();
    let boot_run = world_windowed::Run {
        cfg: &cfg,
        duration,
        end_on: &end_on,
        fork: Some(fork_at),
        checkpoint: None,
        require_barrier: true,
    };
    simulate_windowed(&mut world, &boot_run, options, &mut host_control)?;
    let barrier_ns = world
        .machines
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
    let stream = options.stream_console;
    let mut reports = Vec::new();
    for (i, &seed) in seeds.iter().enumerate() {
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
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            if !stream {
                for mach in &mut world.machines {
                    mach.com1
                        .chunks
                        .open(&devices::log_index::index_name(&format!(
                            "{dir}/{}.console",
                            mach.world.name
                        )));
                    mach.com2
                        .chunks
                        .open(&devices::log_index::index_name(&format!(
                            "{dir}/{}.mgmt",
                            mach.world.name
                        )));
                }
            }
            if stream {
                for mach in world.machines.iter_mut() {
                    for (uart, ext) in [(&mut mach.com1, "console"), (&mut mach.com2, "mgmt")] {
                        use std::io::Write;
                        let mut file =
                            std::fs::File::create(format!("{dir}/{}.{ext}", mach.world.name))
                                .map_err(|e| e.to_string())?;
                        file.write_all(&uart.out).map_err(|e| e.to_string())?;
                        uart.output_file = Some(file);
                        uart.chunks.open(&devices::log_index::index_name(&format!(
                            "{dir}/{}.{ext}",
                            mach.world.name
                        )));
                    }
                }
            }
        }
        let started = Instant::now();
        let (kills, reseed) = release(&mut world, &cfg, &names, seed, release_skew)?;
        let run = world_windowed::Run {
            cfg: &cfg,
            duration,
            end_on: &end_on,
            fork: None,
            checkpoint: None,
            require_barrier: true,
        };
        let (outcome, evidence) = simulate_windowed(&mut world, &run, options, &mut host_control)?;
        let host_seconds = started.elapsed().as_secs_f64();
        let mut r = report(
            &mut world,
            outcome,
            seed,
            &kills,
            host_seconds,
            digest,
            &dir,
            duration,
            &end_on,
        )?;
        r["scheduler"] = evidence;
        r["boot"] = json!({
            "seed": boot_seed, "barrier_ns": barrier_ns, "fork_at_ns": fork_at, "release_skew_ns": release_skew,
            "boot_host_seconds": boot_host_seconds, "forked": i > 0,
            "restored_pages": restored_pages, "restore_host_seconds": restore_host_seconds,
            "reseed": reseed,
        });
        reports.push(r);
    }
    if reports.len() == 1 {
        return Ok(reports.pop().unwrap());
    }
    Ok(json!({
        "report_version": 1,
        "boot": {"seed": boot_seed, "barrier_ns": barrier_ns, "boot_host_seconds": boot_host_seconds,
                 "snapshot_host_seconds": snapshot_host_seconds},
        "seeds": reports,
    }))
}

fn checkpoint_output(world: &mut World, out: &str, stream: bool) -> Result<(), String> {
    std::fs::create_dir_all(out).map_err(|e| e.to_string())?;
    for mach in &mut world.machines {
        for (uart, ext) in [(&mut mach.com1, "console"), (&mut mach.com2, "mgmt")] {
            uart.output_file = uart.chunks.reopen_stream(
                &format!("{out}/{}.{ext}", mach.world.name),
                &uart.out,
                stream,
            )?;
        }
    }
    Ok(())
}

/// Finish a run: write consoles and build the engine report.
#[allow(clippy::too_many_arguments)]
fn report(
    world: &mut World,
    outcome: &str,
    seed: u64,
    kills: &[Value],
    host_seconds: f64,
    digest: bool,
    out: &str,
    duration: u64,
    end_on: &Option<String>,
) -> Result<Value, String> {
    let slices = world.slices;
    world.finish(outcome, duration, end_on)?;
    let trigger_evidence = world.control.triggers.evidence()?;
    let fault_error = world.faults.error.clone().or(world.control.error.clone());
    let mut network_hash = Sha256::new();
    network_hash.update(b"seedbox-network-v1");
    let mut frames = Vec::new();
    for (to, events) in world.events.iter().enumerate() {
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
    world.faults.network_witness(&mut network_hash);
    let legacy_cuts: Vec<_> = world.partitioned.iter().filter(|(_, on)| **on).collect();
    network_hash.update((legacy_cuts.len() as u64).to_le_bytes());
    for (&pair, _) in legacy_cuts {
        links::pair_hash(&mut network_hash, pair);
    }
    let network_witness = network_hash.finalize();
    let mut world_witness = Sha256::new();
    world.control.triggers.hash_into(&mut world_witness)?;
    // Only a run that uses link policy carries the network witness, so a run
    // without it keeps the world witness it had before link policy existed.
    if world.faults.links.in_use() || world.faults.connections.used {
        world_witness.update(network_witness);
    }
    if world.control.controller.is_some() {
        world_witness.update(serde_json::to_vec(&world.control.log).map_err(|e| e.to_string())?);
    }
    if !world.faults.log.is_empty() {
        world_witness.update(serde_json::to_vec(&world.faults.log).map_err(|e| e.to_string())?);
        world_witness.update(world.delivered.to_le_bytes());
        world_witness.update(world.dropped.to_le_bytes());
        world_witness
            .update(serde_json::to_vec(&world.network_snapshot()).map_err(|e| e.to_string())?);
    }
    let mut report = Vec::new();
    for (index, mach) in world.machines.iter_mut().enumerate() {
        if let Some(t) = mach.trace.as_mut() {
            use std::io::Write;
            let _ = t.flush();
        }
        let w = mach.exit_witness();
        world_witness.update(w.as_bytes());
        let (state_digest, state_parts) = if digest {
            let (d, parts) = mach.state_digest();
            // The host supplies output/MACHINE_OUTPUT_DIR as --out; retention
            // preserves that directory prefix (see seedbox/src/vmm.rs).
            std::fs::write(
                format!("{out}/{}.pages", mach.world.name),
                mach.page_hashes(),
            )
            .map_err(|e| e.to_string())?;
            let parts: serde_json::Map<String, Value> = parts
                .into_iter()
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            (Some(d), Some(parts))
        } else {
            (None, None)
        };
        if let Some(d) = &state_digest {
            world_witness.update(d.as_bytes());
        }
        // A disk outlives its machine's crashes: what it holds is part of
        // the outcome even without --digest.
        if let Some(b) = &mach.blk {
            world_witness.update(b.disk().digest().as_bytes());
        }
        std::fs::write(format!("{out}/{}.console", mach.world.name), &mach.com1.out)
            .map_err(|e| e.to_string())?;
        std::fs::write(format!("{out}/{}.mgmt", mach.world.name), &mach.com2.out)
            .map_err(|e| e.to_string())?;
        let s = &mach.stats;
        let mut sk = s.skids.clone();
        sk.sort();
        let skid_p50 = sk.get(sk.len() / 2).copied();
        let skid_p99 = sk.get(sk.len() * 99 / 100).copied();
        let mut row = json!({
            "name": mach.world.name,
            "state": if mach.world.down {"Down".into()} else if mach.world.paused {"Paused".into()} else {format!("{:?}", mach.world.state)},
            "guest_exit_code": mach.guest_exit_code,
            "guest_init_error": mach.guest_init_error,
            "infrastructure_error": matches!(mach.world.state, State::Exited(_)) && mach.guest_exit_code.is_none(),
            "time_ns": mach.world.time,
            "branches": mach.branches,
            "pending_io": mach.pending_io.as_ref().map(|p| json!({"remaining_ns":p.remaining_ns,"direction":if p.write {"out"} else {"in"},"port":p.port,"width":p.width,"count":p.count,"bytes":p.bytes})),
            "witness": w,
            "state_digest": state_digest,
            "state_digest_parts": state_parts,
            "exits": s.exits, "pio": s.pio, "mmio": s.mmio, "halts": s.halts, "irqs": s.irqs,
            "window_exits": s.window_exits, "eintr": s.eintr,
            "precise_stops": s.precise_stops, "steps": s.steps,
            "step_ms": s.step_ns as f64 / 1e6, "run_ms": s.run_ns as f64 / 1e6,
            "skid_max": s.skid_max, "skid_count": s.skids.len(), "final_margin": mach.margin,
            "skid_p50": skid_p50, "skid_p99": skid_p99, "yields": s.yields, "yield_stops": s.yield_stops,
            "soft_misses": s.soft_misses, "soft_misses_clean": s.soft_misses_clean, "soft_undos": s.soft_undos,
            "undo": mach.undo.as_ref().map(|u| json!({
                "checkpoints": u.stats.checkpoints, "rollbacks": u.stats.rollbacks,
                "synced_pages": u.stats.synced_pages, "restored_pages": u.stats.restored_pages,
                "checkpoint_ms": u.stats.checkpoint_ns as f64 / 1e6,
                "rollback_ms": u.stats.rollback_ns as f64 / 1e6,
            })),
            "kvm_ms_by_exit": {"eintr": s.kvm_ns[0] as f64/1e6, "debug": s.kvm_ns[1] as f64/1e6, "pio": s.kvm_ns[2] as f64/1e6, "mmio": s.kvm_ns[3] as f64/1e6, "hlt": s.kvm_ns[4] as f64/1e6, "window": s.kvm_ns[5] as f64/1e6, "other": s.kvm_ns[6] as f64/1e6},
            "kvm_n_by_exit": s.kvm_n,
            "control_channel":mach.channel.as_ref().map(|_|json!({"transport":"virtio-console-v1","tx_bytes":mach.control_tx_bytes,"rx_dma_bytes":mach.control_dma_bytes})),
            "net": mach.net.as_ref().map(|n| n.report()),
        });
        row["pio_total"] = json!(
            s.kvm_n[2]
                + world.incarnations[index]
                    .iter()
                    .filter_map(|old| old["pio_total"].as_u64())
                    .sum::<u64>()
        );
        row["disk"] = json!(mach.blk.as_ref().map(|b| json!({
                "fault_model": disk::disk_fault_model_report(b.disk().fault_model()), "capacity_sectors": b.disk().capacity(), "base_sha256": b.disk().base_sha256(),
                "digest": b.disk().digest(), "stable_sectors": b.disk().stable_sectors(), "volatile_sectors": b.disk().volatile_sectors(),
                "reads": b.stats().reads, "writes": b.stats().writes, "flushes": b.stats().flushes,
                "sectors_read": b.stats().sectors_read, "sectors_written": b.stats().sectors_written,
                "sectors_flushed": b.stats().sectors_flushed, "errors": b.stats().errors,
            })));
        row["incarnation"] = json!(mach.world.incarnation);
        row["booted_at_ns"] = json!(mach.world.booted_at);
        row["crashed_incarnations"] = json!(world.incarnations[index]);
        let skew: serde_json::Map<String, Value> = machine::Site::NAMES
            .iter()
            .zip(&s.skew)
            .map(|(k, v)| (k.to_string(), json!({"n": v.n, "ahead": v.ahead, "ahead_max_ns": v.ahead_max_ns, "behind": v.behind, "behind_max_ns": v.behind_max_ns})))
            .collect();
        row["timer"] = json!({"writes": s.timer_writes, "overdue": s.timer_overdue, "overdue_max_ns": s.timer_overdue_max_ns, "overdue_syscalls_max": s.timer_write_syscalls_max, "guest_clock_skew": skew});
        if let Clock::Pv(_) = world.clock {
            row["spin"] = json!({"probes": s.spin_probes, "idles": s.spin_idles, "false_alarms": s.spin_false, "clock_progress": s.spin_clock_progress, "mismatch": s.spin_mismatch});
            row["syscalls"] = json!(mach.syscalls());
            row["syscall_ns"] = json!(mach.syscall_ns);
            let by: serde_json::Map<String, Value> = machine::Cause::NAMES
                .iter()
                .zip(s.time_by)
                .map(|(k, v)| (k.to_string(), json!(v)))
                .collect();
            row["time_ns_by_cause"] = Value::Object(by);
        }
        report.push(row);
    }
    let sim_ns = world
        .machines
        .iter()
        .map(|m| m.world.time)
        .max()
        .unwrap_or(0);
    let mut result = json!({
        "report_version": 1,
        "seed": seed,
        "timebase": world.clock.name(),
        "deferred_tx": world.boot.iter().any(|b| b.deferred_tx),
        "counter_profile": match world.clock { Clock::Counter(_) => Some(profile::host()?.name), Clock::Pv(_) => None },
        "outcome": outcome,
        "sim_seconds": sim_ns as f64 / 1e9,
        "host_seconds": host_seconds,
        "speed": (sim_ns as f64 / 1e9) / host_seconds,
        "slices": slices,
        "frames_delivered": world.delivered,
        "frames_dropped": world.dropped, "network_counters": world.network_snapshot(),
        "network_witness": format!("{network_witness:x}"),
        "world_witness": format!("{:x}", world_witness.finalize()),
        "kills": kills,
        "faults": world.faults.log, "fault_error":fault_error,
        "control_history":world.control.log,"control_latency_ns":world.control.latency,
        "rejected_fault_effects":world.faults.rejected_effects,"management_readiness":world.faults.readiness,
        "in_flight_policy":"retain_send_time_decisions; cuts_dominate_shapes",
        "machines": report,
    });
    if let Some(evidence) = trigger_evidence {
        result["triggers"] = evidence;
    }
    Ok(result)
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[test]
    fn a_forked_world_rejects_plan_times_before_the_release() {
        let cfg = json!({
            "faults":[{"id":1,"at_ns":1_000_000_000}],
            "scenario":[{"at_ns":1_500_000_000,"serial":"x\n","machine":"a"}],
            "kill_restart":{"start_ns":2_000_000_000},
        });
        assert_eq!(check_plan_after(&cfg,1_400_000_000).unwrap_err(),
            "faults[0].at_ns 1000000000 is before the fork point 1400000000; in a forked world it must be at or after the release (boot.fork_at_ns or the launch barrier, plus boot.release_skew_ns)");
        assert!(check_plan_after(&cfg, 1_000_000_000).is_ok());
        let later = |key: &str| {
            let mut c = cfg.clone();
            c.as_object_mut().unwrap().remove(key);
            c
        };
        assert!(check_plan_after(&later("faults"), 1_600_000_000)
            .unwrap_err()
            .starts_with("scenario[0].at_ns 1500000000 "));
        let kills = json!({"kill_restart":cfg["kill_restart"]});
        assert!(check_plan_after(&kills, 2_000_000_001)
            .unwrap_err()
            .starts_with("kill_restart.start_ns 2000000000 "));
        assert!(check_plan_after(&json!({}), u64::MAX).is_ok());
    }

    /// A Compose guest reports management readiness while it boots; with a
    /// time fork that is before the release. The boot world accepts it, and
    /// the seed's plan starts with that machine already ready.
    #[test]
    fn readiness_signalled_before_the_fork_carries_over_to_the_seed() {
        let names = vec!["a".to_string(), "b".to_string()];
        let links = links::Links::from_config(0, &names, &json!({"latency_ns":1000})).unwrap();
        let mut boot = boot_faults(&names, &[], 1000, links).unwrap();
        boot.management_ready(1, 700, 1000).unwrap();
        let restored = boot_faults(&names, &boot.readiness, 1000, boot.links.clone()).unwrap();
        assert_eq!(restored.readiness, boot.readiness);
        let mut seed = faults::Faults::new(Vec::new(), names.clone()).unwrap();
        seed.require_readiness();
        replay_readiness(&mut seed, &names, &restored.readiness, 1000).unwrap();
        assert_eq!(seed.readiness, vec![json!({"machine":"b","at_ns":700})]);
        assert!(
            seed.management_ready(1, 2000, 1000).is_err(),
            "b is already ready"
        );
        assert!(seed.management_ready(0, 2000, 1000).is_ok());
    }
    #[test]
    fn equal_time_delivery_can_replan_but_an_unchanged_plan_cannot_loop() {
        let mut previous = None;
        let plan = (0, 1000, 1000, 1000, 1000, 1000, 1000, 2000, 500, 1);
        assert!(stop_progress(&mut previous, plan, false).is_ok());
        let delivered = (0, 1000, 1000, 1000, 2000, 2000, 2000, 2000, 500, 2);
        assert!(stop_progress(&mut previous, delivered, false).is_ok());
        assert!(stop_progress(&mut previous, delivered, false).is_err());
        assert!(stop_progress(&mut previous, delivered, true).is_ok());
        assert_eq!(previous, None);
        assert!(stop_progress(&mut previous, delivered, false).is_ok());
    }
}

#[cfg(test)]
mod trigger_world_tests;

#[cfg(test)]
mod worker_options_tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn worker_flags_preserve_the_standalone_protocol() {
        let performance = PerformanceOptions::parse(Some("0"), Some("64")).unwrap();
        let options = WorkerOptions::from_args(
            &args(&[
                "seedbox-vmm",
                "--config",
                "world.json",
                "--seed",
                "7",
                "--out",
                "output",
                "--fork-seeds",
                "8,9",
                "--digest",
                "--host-control",
                "control.sock",
                "--console-limit",
                "123",
                "--log-chunk-limit",
                "456",
                "--timebase",
                "pv",
                "--stream-console",
                "--trace",
                "--page-timer",
                "--deferred-tx",
                "--checkpoint-at-ns",
                "100,200",
            ]),
            performance,
        )
        .unwrap();
        assert_eq!(options.config, PathBuf::from("world.json"));
        assert_eq!(options.seed, 7);
        assert_eq!(options.fork_seeds, vec![8, 9]);
        assert_eq!(options.out, PathBuf::from("output"));
        assert_eq!(options.host_control, Some(PathBuf::from("control.sock")));
        assert_eq!(options.console_limit, Some(123));
        assert_eq!(options.log_chunk_limit, Some(456));
        assert!(options.pv_clock && options.page_timer && options.deferred_tx);
        assert_eq!(options.checkpoint_at_ns.as_deref(), Some("100,200"));
        assert!(options.digest && options.stream_console && options.trace);
        assert_eq!(options.performance, performance);
        assert!(options.console_trace.is_none());
    }

    #[test]
    fn performance_values_are_explicit_and_keep_their_defaults() {
        assert_eq!(
            PerformanceOptions::parse(None, None).unwrap(),
            PerformanceOptions::default()
        );
        assert_eq!(
            PerformanceOptions::parse(Some("256"), Some("adaptive")).unwrap(),
            PerformanceOptions::default()
        );
        assert!(PerformanceOptions::parse(Some("invalid"), None)
            .unwrap_err()
            .starts_with("SEEDBOX_UNDO: "));
        assert!(PerformanceOptions::parse(None, Some("invalid"))
            .unwrap_err()
            .starts_with("margin: "));
    }

    #[test]
    fn worker_flag_errors_keep_their_existing_wording() {
        let performance = PerformanceOptions::default();
        assert_eq!(
            WorkerOptions::from_args(&[], performance).unwrap_err(),
            "--config required"
        );
        assert_eq!(
            WorkerOptions::from_args(
                &args(&[
                    "--config",
                    "world.json",
                    "--seed",
                    "7",
                    "--out",
                    "output",
                    "--checkpoint-at-ns"
                ]),
                performance
            )
            .unwrap_err(),
            "--checkpoint-at-ns requires comma-separated unsigned nanoseconds"
        );
        assert_eq!(
            WorkerOptions::from_args(&args(&["--config", "world.json"]), performance).unwrap_err(),
            "--seed required"
        );
        assert!(WorkerOptions::from_args(
            &args(&["--config", "world.json", "--seed", "invalid"]),
            performance
        )
        .unwrap_err()
        .starts_with("seed: "));
        assert_eq!(
            WorkerOptions::from_args(
                &args(&["--config", "world.json", "--seed", "7"]),
                performance
            )
            .unwrap_err(),
            "--out required"
        );
        assert!(WorkerOptions::from_args(
            &args(&[
                "--config",
                "world.json",
                "--seed",
                "7",
                "--out",
                "output",
                "--fork-seeds",
                "8,invalid"
            ]),
            performance
        )
        .unwrap_err()
        .starts_with("--fork-seeds: "));
        assert!(WorkerOptions::from_args(
            &args(&[
                "--config",
                "world.json",
                "--seed",
                "7",
                "--out",
                "output",
                "--console-limit",
                "invalid"
            ]),
            performance
        )
        .unwrap_err()
        .starts_with("console limit: "));
    }

    #[test]
    fn machine_workers_are_performance_settings() {
        let performance = PerformanceOptions::default();
        let base = ["--config", "world.json", "--seed", "7", "--out", "output"];
        let with = |extra: &[&str]| {
            WorkerOptions::from_args(&args(&[&base[..], extra].concat()), performance)
        };
        let o = with(&[]).unwrap();
        assert_eq!((o.machine_workers, o.machine_cpus.as_slice()), (1, &[][..]));
        let o = with(&["--machine-workers", "4", "--machine-cpus", "4,5,6,7"]).unwrap();
        assert_eq!(
            (o.machine_workers, o.machine_cpus.as_slice()),
            (4, &[4, 5, 6, 7][..])
        );
        assert_eq!(
            with(&["--machine-workers", "0"]).unwrap_err(),
            "--machine-workers: use 1..64"
        );
        assert_eq!(
            with(&["--machine-workers", "2", "--machine-cpus", "4,5,6"]).unwrap_err(),
            "--machine-cpus: give one CPU per machine worker"
        );
    }

    #[test]
    fn worker_timebase_defaults_to_counter_and_accepts_pv() {
        let base = args(&["--config", "world.json", "--seed", "7", "--out", "output"]);
        assert!(
            !WorkerOptions::from_args(&base, PerformanceOptions::default())
                .unwrap()
                .pv_clock
        );
        for (value, pv) in [("counter", false), ("pv", true)] {
            let mut argv = base.clone();
            argv.extend(args(&["--timebase", value]));
            assert_eq!(
                WorkerOptions::from_args(&argv, PerformanceOptions::default())
                    .unwrap()
                    .pv_clock,
                pv
            );
        }
        let mut argv = base;
        argv.extend(args(&["--timebase", "bad"]));
        assert_eq!(
            WorkerOptions::from_args(&argv, PerformanceOptions::default()).unwrap_err(),
            "--timebase bad: expected counter or pv"
        );
    }

    #[test]
    fn console_trace_callback_is_restored_when_worker_scope_ends() {
        fn callback(_: ConsoleTrace<'_>) {}
        assert!(!console_trace_enabled());
        {
            let _guard =
                ConsoleTraceGuard(CONSOLE_TRACE.with(|trace| trace.replace(Some(callback))));
            assert!(console_trace_enabled());
            emit_console_trace(1000, 2, b"line");
        }
        assert!(!console_trace_enabled());
    }
}

#[cfg(test)]
mod checkpoint_tests;
