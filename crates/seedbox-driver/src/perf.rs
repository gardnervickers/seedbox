//! TEMPORARY measurement counters for the simulation-speed investigation.
//! Not for merge. Prints cumulative counters to stderr (the simulation
//! worker's `vmm.stderr`) at most every two seconds and when the Driver drops.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        pub struct Counters { $(pub $name: AtomicU64,)* }
        pub static C: Counters = Counters { $($name: AtomicU64::new(0),)* };
        fn dump(label: &str) {
            let mut line = format!("SEEDBOX-PERF driver {label} pid={}", std::process::id());
            $(line.push_str(&format!(" {}={}", stringify!($name), C.$name.load(Relaxed)));)*
            eprintln!("{line}");
        }
    };
}

counters!(
    steps,
    step_ns,
    commits,
    batches,
    batch_machines,
    zero_batches,
    zero_batch_machines,
    timer_wake_batches,
    frame_batches,
    execute_ns,
    zero_execute_ns,
    run_ns_sum,
    run_ns_max_sum,
    commit_ns,
    flush_ns,
    sim_ns,
    frames_routed,
    outputs,
);

pub fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Relaxed);
}

pub fn since(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

/// TEMPORARY trace: who asked for the next batch (1 step, 2 frames,
/// 3 timers, 4 admission, 5 resume), batch and step indices, host clock.
pub static KIND: AtomicU64 = AtomicU64::new(0);
pub static BATCH: AtomicU64 = AtomicU64::new(0);
pub static STEP: AtomicU64 = AtomicU64::new(0);
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

pub fn host_ns() -> u64 {
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

pub fn set_kind(kind: u64) {
    KIND.store(kind, Relaxed);
}

pub fn take_kind() -> u64 {
    KIND.swap(0, Relaxed)
}

pub(crate) fn maybe_dump() {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match *last {
        Some(at) if now.duration_since(at).as_secs() < 2 => {}
        _ => {
            *last = Some(now);
            dump("periodic");
        }
    }
}

pub(crate) fn final_dump() {
    dump("final");
}

/// TEMPORARY experiment (not for merge): round frame arrivals and blocked
/// timer wakeups up to this grid in nanoseconds, set at build time with
/// SEEDBOX_PERF_GRID_NS. Unset or 0 leaves timing unchanged.
pub fn grid_ns() -> u64 {
    option_env!("SEEDBOX_PERF_GRID_NS")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0)
}

pub fn grid(at: u64) -> u64 {
    match grid_ns() {
        g if g > 1 => at.div_ceil(g).saturating_mul(g),
        _ => at,
    }
}
