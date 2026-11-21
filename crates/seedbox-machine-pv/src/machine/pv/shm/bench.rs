//! An opt-in two-process benchmark. The control words below are exclusive to
//! this benchmark and live outside both rings' protocol headers.
use super::*;
use crate::machine::pv::{protocol::Stop, wire};
use seedbox_machine::host::idle::SpinThenSleep;
use seedbox_machine::pv::transport::{GuestWrite, LifecycleWrite};
use std::io::BufReader;
use std::os::fd::BorrowedFd;
use std::process::Stdio;
use std::time::Duration;

const READY: usize = 80;
const RECEIVED: usize = 81;
const SENT_AT: usize = 82;
const THREAD: usize = 83;
const SPIN_SEQUENCE: usize = 84;
const TIMEOUT: Duration = Duration::from_secs(120);
type Connection<R, W> = BufReader<Stream<R, W>>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Spin,
    Park,
    Bulk,
}
impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Spin => "ping-spin",
            Self::Park => "ping-park",
            Self::Bulk => "one-way",
        }
    }
}

// In a strategy trial, start the clock only after the worker has entered its
// selected spin tier, and keep it there instead of asleep. The additional
// mapping keeps the notification word live. A peer left waiting for TIMEOUT
// has lost its parent: it then sleeps, whatever its budget, and the transport
// sees the end of its pipe.
struct SpinningPeer {
    region: Region,
    policy: Idle,
    idle: Box<dyn IdleStrategy>,
}
impl IdleStrategy for SpinningPeer {
    fn idle(&mut self, attempt: u64, elapsed: Duration) -> Action {
        if elapsed >= TIMEOUT {
            // Whatever budget the trial asked for.
            return Action::Sleep;
        }
        let action = self.idle.idle(attempt, elapsed);
        let reached = match self.policy {
            Idle::Monitor => matches!(action, Action::Monitor(_)),
            Idle::SpinThenSleep => true,
        };
        if reached || action == Action::Sleep {
            let seq = self.region.word(SPIN_SEQUENCE).load(SeqCst);
            if self.region.word(READY).load(SeqCst) != seq {
                self.region.word(READY).store(seq, SeqCst);
            }
        }
        if action == Action::Sleep {
            Action::Spin
        } else {
            action
        }
    }
    fn reset(&mut self, outcome: Outcome) {
        self.idle.reset(outcome);
    }
}

fn selected() -> Result<Option<Idle>> {
    match std::env::var("PV_BENCH_IDLE").ok().as_deref() {
        None => Ok(None),
        Some("bounded") => Ok(Some(Idle::SpinThenSleep)),
        Some("monitor") => Ok(Some(Idle::Monitor)),
        Some(_) => Err("unknown PV_BENCH_IDLE".into()),
    }
}
fn connection<R: Read, W: Write>(
    region: Region,
    side: Side,
    input: R,
    output: W,
    mode: Mode,
    spinning: Option<Region>,
) -> Result<Connection<R, W>> {
    let idle: Box<dyn IdleStrategy> = match (selected()?, mode) {
        (_, Mode::Park) => Box::new(SpinThenSleep::new(Duration::ZERO)),
        (Some(policy), _) => {
            let budget = match side {
                Side::Worker => {
                    std::env::var("PV_BENCH_VM_US").map_or(Ok(worker_spin(policy)), |s| {
                        s.parse()
                            .map(Duration::from_micros)
                            .map_err(|_| "invalid PV_BENCH_VM_US")
                    })?
                }
                Side::Client => SpinThenSleep::SPIN,
            };
            let idle = policy.with_budget(budget);
            if let Some(region) = spinning {
                Box::new(SpinningPeer {
                    region,
                    policy,
                    idle,
                })
            } else {
                idle
            }
        }
        (None, _) => Box::new(SpinThenSleep::new(TIMEOUT)),
    };
    Ok(BufReader::new(Stream::new(
        region, side, input, output, idle,
    )))
}

fn send<R: Read, W: Write>(io: &mut Connection<R, W>, seq: u64, data: &[GuestWrite]) -> Result<()> {
    let header = Response::Stopped {
        time: seq,
        stop: Stop::Horizon,
        writes: 1,
    };
    io.get_mut().reply(&header, Some(data))
}

fn receive<R: Read, W: Write>(io: &mut Connection<R, W>, seq: u64) -> Result<Vec<GuestWrite>> {
    super::receive(io, &[], |header, body| match header {
        Response::Stopped {
            time,
            stop: Stop::Horizon,
            writes: 1,
        } if time == seq => wire::receive_writes(body, 1),
        _ => Err("transport benchmark received the wrong header".into()),
    })?
    .ok_or_else(|| "transport benchmark peer closed".into())
}

// A real header-only Snapshot request and response, with no hypervisor work.
fn control_send<R: Read, W: Write>(
    io: &mut Connection<R, W>,
    header: &impl serde::Serialize,
) -> Result<()> {
    let ring = io.get_mut();
    ring.message(header, |_| Ok(()))?;
    ring.finish_send().map_err(|e| e.to_string())
}
fn control_receive<R: Read, W: Write, H: serde::de::DeserializeOwned>(
    io: &mut Connection<R, W>,
) -> Result<H> {
    super::receive(io, &[], |header, _| Ok(header))?
        .ok_or_else(|| "control benchmark peer closed".into())
}

// The same monotonic clock in both processes. One-way latency ends at the
// receiver, before its acknowledgment or any payload checking.
fn now_ns() -> u64 {
    let mut ts = std::mem::MaybeUninit::uninit();
    // SAFETY: clock_gettime initializes the output on success.
    assert_eq!(
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, ts.as_mut_ptr()) },
        0
    );
    // SAFETY: the call succeeded, so it wrote the whole timespec.
    let ts = unsafe { ts.assume_init() };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn wait_for(mut ready: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + TIMEOUT;
    while !ready() {
        if Instant::now() >= deadline {
            return Err("transport benchmark handshake timed out".into());
        }
        std::hint::spin_loop();
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn thread_id() -> u64 {
    let mut tid = 0;
    // SAFETY: a null thread denotes this thread; tid is writable.
    assert_eq!(unsafe { libc::pthread_threadid_np(0, &mut tid) }, 0);
    tid
}
#[cfg(target_os = "linux")]
fn thread_id() -> u64 {
    u64::from(std::process::id())
}

#[cfg(target_os = "macos")]
fn parked(pid: u32, tid: u64) -> Result<bool> {
    // <sys/proc_info.h>: this flavor accepts the numeric pthread thread ID.
    const PROC_PIDTHREADID64INFO: i32 = 15;
    let mut info = std::mem::MaybeUninit::<libc::proc_threadinfo>::uninit();
    let len = std::mem::size_of_val(&info) as i32;
    // SAFETY: the output is sized and aligned for this flavor.
    if unsafe {
        libc::proc_pidinfo(
            pid as i32,
            PROC_PIDTHREADID64INFO,
            tid,
            info.as_mut_ptr().cast(),
            len,
        )
    } != len
    {
        return Err(format!(
            "querying parked peer: {}",
            io::Error::last_os_error()
        ));
    }
    // SAFETY: the call filled the whole record, as its length says.
    Ok(unsafe { info.assume_init() }.pth_run_state == libc::TH_STATE_WAITING)
}
#[cfg(target_os = "linux")]
fn parked(pid: u32, _: u64) -> Result<bool> {
    // This single-threaded peer has armed the ring and its only blocking
    // operation until publication is a read on its wake pipe.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
    Ok(stat
        .rsplit_once(") ")
        .is_some_and(|(_, rest)| rest.starts_with("S ")))
}

fn payload(size: usize) -> Vec<GuestWrite> {
    let bytes = (0..size)
        .map(|i| (i.wrapping_mul(73) ^ (i >> 8)) as u8)
        .collect();
    vec![GuestWrite::Console(LifecycleWrite { time: 0, bytes })]
}
fn validate(got: &[GuestWrite], expected: &[GuestWrite], all: bool) -> Result<()> {
    match (got, expected) {
        ([GuestWrite::Console(a)], [GuestWrite::Console(b)])
            if a.bytes.len() == b.bytes.len()
                && a.bytes.first() == b.bytes.first()
                && a.bytes.last() == b.bytes.last()
                && (!all || a.bytes == b.bytes) =>
        {
            Ok(())
        }
        _ => Err("transport benchmark payload mismatch".into()),
    }
}

fn peer(mode: Mode, size: usize, count: u64, capacity: u64) -> Result<()> {
    let signal = mode == Mode::Spin && selected()?.is_some();
    let spinning = if signal {
        // SAFETY: the benchmark parent passes this descriptor to the child.
        // The second mapping it makes is read and written only as atomics.
        let fd = unsafe { BorrowedFd::borrow_raw(DESCRIPTOR) }
            .try_clone_to_owned()
            .map_err(|e| e.to_string())?;
        Some(Region::map(&fd, capacity)?)
    } else {
        None
    };
    let region = Region::inherited(capacity)?;
    let mut io = connection(
        region,
        Side::Worker,
        std::io::stdin().lock(),
        std::io::stdout().lock(),
        mode,
        spinning,
    )?;
    // SAFETY: the parent passed this process its unique inherited descriptors.
    io.get_mut().bell = Some(unsafe { Ringer::inherited() }.map_err(|e| e.to_string())?);
    io.get_ref().region.word(THREAD).store(thread_id(), SeqCst);
    let expected = payload(size);
    for seq in 1..=count {
        if mode == Mode::Bulk {
            wait_for(|| io.get_ref().region.word(RECEIVED).load(SeqCst) == seq - 1)?;
            io.get_ref().region.word(SENT_AT).store(now_ns(), SeqCst);
            send(&mut io, seq, &expected)?;
        } else {
            io.get_ref()
                .region
                .word(if signal { SPIN_SEQUENCE } else { READY })
                .store(seq, SeqCst);
            if size == 0 {
                match control_receive(&mut io)? {
                    protocol::Request::Snapshot {
                        limit,
                        verify: false,
                    } if limit == seq => {}
                    _ => return Err("wrong snapshot request".into()),
                }
                control_send(
                    &mut io,
                    &Response::Snapshotted {
                        id: seq,
                        pages: 0,
                        held: 0,
                    },
                )?;
            } else {
                let got = receive(&mut io, seq)?;
                send(&mut io, seq, &got)?;
            }
        }
    }
    Ok(())
}

fn samples<R: Read, W: Write>(
    io: &mut Connection<R, W>,
    pid: u32,
    mode: Mode,
    size: usize,
    count: u64,
) -> Result<()> {
    let expected = payload(size);
    let warm = count.min(64) / 4;
    let mut durations = Vec::new();
    let mut started = 0;
    let mut ended = 0;
    for seq in 1..=count + warm {
        let mut begin = 0;
        if mode != Mode::Bulk {
            let region = &io.get_ref().region;
            wait_for(|| region.word(READY).load(SeqCst) == seq)?;
            if mode == Mode::Park {
                wait_for(|| region.word(Ring::Requests.word(READER_ASLEEP)).load(SeqCst) != 0)?;
                let tid = region.word(THREAD).load(SeqCst);
                let deadline = Instant::now() + TIMEOUT;
                while !parked(pid, tid)? {
                    if Instant::now() >= deadline {
                        return Err("peer failed to park".into());
                    }
                }
            }
            begin = now_ns();
            if size == 0 {
                control_send(
                    io,
                    &protocol::Request::Snapshot {
                        limit: seq,
                        verify: false,
                    },
                )?;
            } else {
                send(io, seq, &expected)?;
            }
        }
        let got = if size == 0 {
            match control_receive(io)? {
                Response::Snapshotted {
                    id,
                    pages: 0,
                    held: 0,
                } if id == seq => None,
                _ => return Err("wrong snapshot reply".into()),
            }
        } else {
            Some(receive(io, seq)?)
        };
        ended = now_ns();
        if mode == Mode::Bulk {
            begin = io.get_ref().region.word(SENT_AT).load(SeqCst);
        }
        if let Some(got) = got {
            validate(&got, &expected, seq == 1 || seq == count + warm)?;
        }
        if seq == warm + 1 {
            started = begin;
        }
        if seq > warm {
            durations.push(ended - begin);
        }
        io.get_ref().region.word(RECEIVED).store(seq, SeqCst);
    }
    durations.sort_unstable();
    let pct = |p: usize| durations[(durations.len() - 1) * p / 100];
    let mib_s = (size as f64 * count as f64 / (1 << 20) as f64) / ((ended - started) as f64 / 1e9);
    println!(
        "{}",
        serde_json::json!({"mode":mode.name(),"payload":size,"capacity":io.get_ref().region.capacity,
        "samples":count,"p50_ns":pct(50),"p95_ns":pct(95),"p99_ns":pct(99),"mib_s":mib_s})
    );
    Ok(())
}

fn case(mode: Mode, size: usize, count: u64, capacity: u64) -> Result<()> {
    let (region, fd) = Region::create(capacity)?;
    let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
    inherit(fd, &mut command)?;
    seedbox_machine::host::doorbell::Doorbell::process()
        .and_then(|bell| bell.inherit(&mut command))
        .map_err(|e| e.to_string())?;
    let warm = count.min(64) / 4;
    command
        .args([
            "peer",
            mode.name(),
            &size.to_string(),
            &(count + warm).to_string(),
            &capacity.to_string(),
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = seedbox_exec::spawn(&mut command).map_err(|e| e.to_string())?;
    let mut io = connection(
        region,
        Side::Client,
        child.stdout.take().unwrap(),
        child.stdin.take().unwrap(),
        Mode::Spin,
        None,
    )?;
    let result = samples(&mut io, child.id(), mode, size, count);
    drop(io);
    if result.is_err() {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    result?;
    if !status.success() {
        return Err(format!("peer exited with {status}"));
    }
    Ok(())
}

pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("peer") {
        let [mode, size, count, capacity] = &args[2..] else {
            return Err("benchmark peer takes a mode, size, count and capacity".into());
        };
        let mode = match mode.as_str() {
            "ping-spin" => Mode::Spin,
            "ping-park" => Mode::Park,
            "one-way" => Mode::Bulk,
            _ => return Err("unknown benchmark mode".into()),
        };
        let number = |what: &str| format!("benchmark peer {what}");
        return peer(
            mode,
            size.parse().map_err(|_| number("size"))?,
            count.parse().map_err(|_| number("count"))?,
            capacity.parse().map_err(|_| number("capacity"))?,
        );
    }
    let capacity = std::env::var("PV_BENCH_CAPACITY").map_or(Ok(CAPACITY), |s| {
        s.parse().map_err(|e| format!("benchmark capacity: {e}"))
    })?;
    if args.iter().skip(1).any(|arg| arg == "control") {
        for mode in [Mode::Spin, Mode::Park] {
            case(mode, 0, 4096, capacity)?;
        }
        return Ok(());
    }
    for size in [64, 1024] {
        for mode in [Mode::Spin, Mode::Park] {
            case(mode, size, 4096, capacity)?;
        }
    }
    if args.iter().skip(1).any(|arg| arg == "ping") {
        return Ok(());
    }
    for size in [4 << 10, 64 << 10, 1 << 20, 8 << 20] {
        case(
            Mode::Bulk,
            size,
            (64_u64 << 20).div_ceil(size as u64).clamp(24, 4096),
            capacity,
        )?;
    }
    Ok(())
}
