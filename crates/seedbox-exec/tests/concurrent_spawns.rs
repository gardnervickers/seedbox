//! Concurrent spawns never hand one another's pipes to their children.
//!
//! Threads released together at a barrier each spawn this binary through
//! `seedbox_exec::spawn` the way the PV client starts a worker: with a
//! `pre_exec` hook, so std forks instead of using posix_spawn, and with piped
//! stdin and stdout. Each child reports the pipe ends it holds, by fstat
//! device and inode, and then stays alive until its stdin closes, as a worker
//! does. A child may hold only its own stdin and stdout and the pipes this
//! process held before any spawn (an inherited stderr or jobserver). Any other
//! pipe came from a spawn in flight on another thread: on macOS std creates a
//! pipe and marks it close-on-exec in a second call, and a fork in between
//! copies it. If the copy is that spawn's exec-status write end, the spawn
//! never returns; the containment bound below turns that into a failure.
//!
//! No libtest harness: the binary is also the spawned child ([`CHILD`]).
use std::{
    collections::BTreeSet,
    fs::File,
    io::{BufRead, BufReader, Read, Write},
    mem::ManuallyDrop,
    os::{
        fd::{FromRawFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt},
            process::CommandExt,
        },
    },
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Barrier, Mutex},
    thread,
    time::{Duration, Instant},
};

const NAME: &str = "concurrent_spawns_keep_their_pipes";
/// First argument of the re-executed child.
const CHILD: &str = "--seedbox-exec-report-pipes";
const THREADS: usize = 6;
const ROUNDS: usize = 50;
/// Containment only: a whole round normally takes milliseconds.
const BOUND: Duration = Duration::from_secs(30);

/// A pipe end, identified by fstat (device, inode); never by descriptor
/// number, which another process reuses freely.
type Pipe = (u64, u64);

/// Every pipe end this process holds, with its descriptor for diagnostics.
fn pipes() -> Vec<(RawFd, Pipe)> {
    let mut pipes = Vec::new();
    for entry in std::fs::read_dir("/dev/fd").expect("list /dev/fd") {
        let name = entry.expect("/dev/fd entry").file_name();
        let fd: RawFd = name.to_str().and_then(|n| n.parse().ok()).expect("fd");
        // SAFETY: borrowed for one fstat and never closed (ManuallyDrop). The
        // listing comes from /dev/fd while its own directory is still open,
        // and no other thread opens or closes descriptors meanwhile.
        let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
        if let Ok(meta) = file.metadata() {
            if meta.file_type().is_fifo() {
                pipes.push((fd, (meta.dev(), meta.ino())));
            }
        }
    }
    pipes
}

/// The child: print "fd:dev:ino" for each pipe end, then hold them all until
/// stdin closes.
fn child() {
    let line: Vec<String> = pipes()
        .into_iter()
        .map(|(fd, (dev, ino))| format!("{fd}:{dev}:{ino}"))
        .collect();
    let mut stdout = std::io::stdout();
    writeln!(stdout, "{}", line.join(" ")).expect("report");
    stdout.flush().expect("report");
    let _ = std::io::stdin().read_to_end(&mut Vec::new());
}

struct Spawned {
    thread: usize,
    stdin: Option<ChildStdin>,
    /// The child's pipe ends, as it reported them.
    held: Vec<(RawFd, Pipe)>,
}

/// One thread's spawn: wait at the barrier, spawn, register the child for
/// cleanup, and read its report.
fn spawn_one(thread: usize, barrier: &Barrier, children: &Mutex<Vec<Child>>) -> Spawned {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .arg(CHILD)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    // Any hook makes std fork and wait on its exec-status pipe, as the PV
    // client's descriptor hooks do. SAFETY: the hook does nothing.
    unsafe { command.pre_exec(|| Ok(())) };
    barrier.wait();
    let mut child = seedbox_exec::spawn(&mut command).expect("spawn");
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take().expect("stdout"));
    children.lock().unwrap().push(child);
    let mut report = String::new();
    // Empty if the child was killed before reporting (cleanup after a hang).
    let _ = BufReader::new(stdout).read_line(&mut report);
    let held = report
        .split_whitespace()
        .map(|entry| {
            let mut fields = entry.split(':').map(|f| f.parse::<u64>().expect("field"));
            let mut next = || fields.next().expect("fd:dev:ino");
            (next() as RawFd, (next(), next()))
        })
        .collect();
    Spawned {
        thread,
        stdin,
        held,
    }
}

/// Pipe ends a child holds that are neither its own stdin (0) and stdout (1)
/// nor inherited from this process's state before any spawn.
fn foreign(spawned: &Spawned, inherited: &BTreeSet<Pipe>) -> Vec<(RawFd, Pipe)> {
    spawned
        .held
        .iter()
        .filter(|(fd, pipe)| *fd > 1 && !inherited.contains(pipe))
        .copied()
        .collect()
}

/// One round. Fails on the first foreign pipe or on a spawn outlasting
/// [`BOUND`]; either way every child is killed first, which also releases
/// any spawn stuck on a leaked exec-status pipe.
fn round(round: usize, inherited: &BTreeSet<Pipe>) -> Result<(), String> {
    let barrier = Barrier::new(THREADS);
    let children = Mutex::new(Vec::new());
    let (done, results) = mpsc::channel();
    thread::scope(|scope| {
        for thread in 0..THREADS {
            let (barrier, children, done) = (&barrier, &children, done.clone());
            scope.spawn(move || done.send(spawn_one(thread, barrier, children)));
        }
        drop(done);
        let deadline = Instant::now() + BOUND;
        let mut spawned = Vec::new();
        let mut failure = None;
        while spawned.len() < THREADS {
            let wait = deadline.saturating_duration_since(Instant::now());
            match results.recv_timeout(wait) {
                Ok(next) => {
                    let leaked = foreign(&next, inherited);
                    if !leaked.is_empty() {
                        failure = Some(format!(
                            "round {round}: the child of thread {} holds pipe ends of another spawn (fd:(dev, ino)) {leaked:?}; {} of {THREADS} spawns had returned",
                            next.thread,
                            spawned.len() + 1,
                        ));
                    }
                    spawned.push(next);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    failure = Some(format!(
                        "round {round}: {} of {THREADS} spawns returned within {BOUND:?}",
                        spawned.len()
                    ))
                }
                // A spawn thread panicked; the scope reports it.
                Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            }
            if failure.is_some() {
                break;
            }
        }
        if let Some(failure) = failure {
            // Kill until every thread has reported: a stuck spawn returns once
            // the child holding its status pipe dies, then registers its own.
            let mut outstanding = THREADS - spawned.len();
            loop {
                for child in children.lock().unwrap().iter_mut() {
                    let _ = child.kill();
                }
                if outstanding == 0 {
                    break;
                }
                results.recv().expect("spawn thread");
                outstanding -= 1;
            }
            for mut child in std::mem::take(&mut *children.lock().unwrap()) {
                let _ = child.wait();
            }
            return Err(failure);
        }
        // Close every stdin: the children exit.
        spawned.into_iter().for_each(|spawned| drop(spawned.stdin));
        for mut child in std::mem::take(&mut *children.lock().unwrap()) {
            let status = child.wait().expect("wait");
            assert!(status.success(), "round {round}: child {status}");
        }
        Ok(())
    })
}

fn concurrent_spawns_keep_their_pipes() {
    let inherited: BTreeSet<Pipe> = pipes().into_iter().map(|(_, pipe)| pipe).collect();
    for n in 0..ROUNDS {
        if let Err(failure) = round(n, &inherited) {
            panic!("{failure}");
        }
    }
}

fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some(CHILD) {
        return child();
    }
    // Of libtest's options, only listing and ignored-only runs change what
    // runs; filters are ignored, as the one test takes about a second.
    if arguments.iter().any(|a| a == "--list") {
        println!("{NAME}: test");
        return;
    }
    if arguments.iter().any(|a| a == "--ignored") {
        return;
    }
    concurrent_spawns_keep_their_pipes();
    println!("test {NAME} ... ok");
}
