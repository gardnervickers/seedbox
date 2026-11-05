//! TEMPORARY measurement counters for the simulation-speed investigation.
//! Not for merge. Each process prints its cumulative counters to stderr at
//! most every two seconds; PV worker stderr is the simulation's `vmm.stderr`.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Instant;

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        pub(crate) struct Counters { $(pub(crate) $name: AtomicU64,)* }
        pub(crate) static C: Counters = Counters { $($name: AtomicU64::new(0),)* };
        fn dump(kind: &str) {
            let mut line = format!("SEEDBOX-PERF {kind} pid={}", std::process::id());
            $(line.push_str(&format!(" {}={}", stringify!($name), C.$name.load(Relaxed)));)*
            eprintln!("{line}");
        }
    };
}

counters!(
    // Client side (simulation worker process), per PV machine call.
    client_runs,
    client_ipc_ns,
    // Worker side (one PV worker process).
    requests,
    wait_ns,
    runs,
    zero_runs,
    run_ns,
    sim_ns,
    enters,
    enter_ns,
    handle_ns,
    exit_interrupted,
    exit_maintenance,
    exit_clock_arm,
    exit_clock_ack,
    exit_idle,
    exit_yield,
    exit_clock_other,
    exit_lifecycle,
    exit_disk,
    exit_net_read,
    exit_net_write,
    exit_platform,
    stop_horizon,
    stop_blocked,
    stop_other,
    frames_in,
    writes_out,
);

pub(crate) fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Relaxed);
}

pub(crate) fn since(start: Instant) -> u64 {
    start.elapsed().as_nanos() as u64
}

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

pub(crate) fn maybe_dump(kind: &str) {
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match *last {
        Some(at) if now.duration_since(at).as_secs() < 2 => {}
        _ => {
            *last = Some(now);
            dump(kind);
        }
    }
}
