//! On `RUN`, LAZYINIT_THREADS threads (4) race through LAZYINIT_ROUNDS
//! rounds (4): each round a barrier releases them together and each gets or
//! builds the shared value, which takes LAZYINIT_WORK rounds of hashing
//! (30000). The reply counts the rounds that built it more than once. With
//! LAZYINIT_BUG=0 the check and build happen under a lock.
use seedbox_eval_lazyinit::PORT;
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};

struct Shared {
    value: AtomicU64,
    builds: AtomicU64,
    lock: Mutex<()>,
    buggy: bool,
    work: u64,
}

fn build(work: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for i in 0..work {
        hash = (hash ^ i).wrapping_mul(0x0100_0000_01b3);
    }
    std::hint::black_box(hash) | 1
}

fn get_or_init(shared: &Shared) -> u64 {
    let _guard = (!shared.buggy).then(|| shared.lock.lock().unwrap());
    let current = shared.value.load(Ordering::Acquire);
    if current != 0 {
        return current;
    }
    let value = build(shared.work);
    shared.value.store(value, Ordering::Release);
    shared.builds.fetch_add(1, Ordering::AcqRel);
    value
}

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

/// Run every round; return the build count of each.
fn race(threads: usize, rounds: usize, shared: Arc<Shared>) -> Vec<u64> {
    let barrier = Arc::new(Barrier::new(threads));
    let counts = Arc::new(Mutex::new(Vec::new()));
    let workers: Vec<_> = (0..threads)
        .map(|_| {
            let (barrier, shared, counts) = (barrier.clone(), shared.clone(), counts.clone());
            std::thread::spawn(move || {
                for _ in 0..rounds {
                    barrier.wait();
                    get_or_init(&shared);
                    if barrier.wait().is_leader() {
                        counts
                            .lock()
                            .unwrap()
                            .push(shared.builds.swap(0, Ordering::AcqRel));
                        shared.value.store(0, Ordering::Release);
                    }
                    barrier.wait();
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }
    let counts = counts.lock().unwrap().clone();
    counts
}

fn main() -> std::io::Result<()> {
    let buggy = std::env::var("LAZYINIT_BUG").map_or(true, |value| value != "0");
    let threads = env("LAZYINIT_THREADS", 4) as usize;
    let rounds = env("LAZYINIT_ROUNDS", 4) as usize;
    let shared = Arc::new(Shared {
        value: AtomicU64::new(0),
        builds: AtomicU64::new(0),
        lock: Mutex::new(()),
        buggy,
        work: env("LAZYINIT_WORK", 30_000),
    });
    let listener = TcpListener::bind(("0.0.0.0", PORT))?;
    seedbox_sdk::setup_complete(json!({"buggy": buggy}));
    for stream in listener.incoming() {
        let stream = stream?;
        let mut writer = stream.try_clone()?;
        for line in BufReader::new(stream).lines() {
            let reply = match line?.trim() {
                "RUN" => {
                    let counts = race(threads, rounds, shared.clone());
                    let doubled = counts.iter().filter(|&&c| c > 1).count();
                    if doubled > 0 {
                        seedbox_sdk::event("built twice", json!({"rounds": doubled}));
                    }
                    format!("rounds {} doubled {doubled}", counts.len())
                }
                _ => "ERR".to_string(),
            };
            writer.write_all(format!("{reply}\n").as_bytes())?;
        }
    }
    Ok(())
}
