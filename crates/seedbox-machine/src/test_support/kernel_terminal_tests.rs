//! Guest-initiated kernel terminal paths on real Linux kernels, through the
//! common Machine contract on TCG and PV.
//!
//! A privileged application asks the kernel to reboot or power off, or panics
//! it through sysrq. Each path must end in `Exited` with the documented
//! reason at a bounded simulated time, never as an error, a hang, an in-place
//! backend reset or a fabricated program exit. The exited state must snapshot
//! and restore, keep its reason through zero-budget runs, and accept a Reboot
//! request that starts a fresh boot. Simulated bounds define the result; the
//! host-time check only diagnoses a hang. PV additionally recognizes direct OS
//! halt as shutdown; that behavior is not required by the common contract.
use super::{ExitReason, Machine, RunContext, StopReason, Timestamp};
use crate::{
    clock::{Clock, SimulationClock},
    definition::{FilesystemArchive, Launch, MachineDefinition, PreparedApplication, Resources},
    pv::{Inbox, Input, Outbox, Output, Request, Response},
    rng::{RngStream, SeededRng},
};
use seedbox_test_time::HangGuard;
use std::{fmt::Debug, time::Duration};

pub const QUANTUM: u64 = 10_000_000;
/// Simulated bound for one boot to reach its terminal request and stop.
pub const BOOT_LIMIT: u64 = 60_000_000_000;
/// Host diagnostic only, checked between synchronous calls: it reports a run
/// that never ends its boot, but cannot interrupt a call that never returns
/// (the test process's own execution bound diagnoses that).
pub const HOST_LIMIT: Duration = Duration::from_secs(600);

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

/// Expected outcome and kernel console evidence of one terminal path.
pub struct Path {
    pub mode: &'static str,
    pub reason: ExitReason,
    pub console: &'static str,
}
pub const PATHS: [Path; 3] = [
    Path {
        mode: "reboot",
        reason: ExitReason::Crashed,
        console: "reboot: Restarting system",
    },
    Path {
        mode: "poweroff",
        reason: ExitReason::Shutdown,
        console: "reboot: Power down",
    },
    Path {
        mode: "panic",
        reason: ExitReason::Crashed,
        console: "Kernel panic - not syncing: sysrq triggered crash",
    },
];

// PV maps halt to Shutdown: x86 recognizes masked HLT after the handshake;
// ARM uses its existing kernel's SYSTEM_OFF routing. Keep this regression
// without requiring the same classification from TCG, which may stay Blocked.
pub const PV_HALT: Path = Path {
    mode: "halt",
    reason: ExitReason::Shutdown,
    console: "reboot: System halted",
};

/// The fixture as a root application with one mode argument.
pub fn definition(binary: &[u8], mode: &str, name: &str, memory_bytes: u64) -> MachineDefinition {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(binary.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", binary).unwrap();
    MachineDefinition {
        name: name.into(),
        resources: Resources { memory_bytes },
        application: PreparedApplication {
            filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
            launch: Launch {
                argv: vec!["/application".into(), mode.into()],
                environment: Default::default(),
                working_directory: "/".into(),
                user: "0:0".into(),
            },
        },
        disk: None,
        network: None,
    }
}

/// Caller-owned RNG that counts the bytes a machine draws.
#[derive(Clone)]
struct CountingRng(SeededRng, usize);
impl RngStream for CountingRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        self.1 += bytes.len();
        self.0.fill_bytes(bytes);
    }
}

/// One direct `Machine::run` with explicit caller-owned inputs.
fn step<M: Machine<Context = ()>>(
    machine: &mut M,
    clock: &SimulationClock,
    rng: &mut CountingRng,
    requests: Vec<Request>,
    upto: Timestamp,
) -> Result<(StopReason, Vec<Output>), M::Error> {
    let now = at(clock.now());
    let mut inbox = requests
        .into_iter()
        .map(|payload| Input { at: now, payload })
        .collect();
    let mut staged = Vec::new();
    let stop = machine.run(
        &mut RunContext {
            worker: &mut (),
            clock,
            rng,
            execution: Default::default(),
            inbox: Inbox::new(&mut inbox, now),
            outbox: Outbox::new(&mut staged, now, upto),
        },
        upto,
    )?;
    assert!(inbox.is_empty(), "every due request is accepted");
    let reached = match stop {
        StopReason::BoundaryReached => upto,
        StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
    };
    assert!(
        now <= reached && reached <= upto,
        "{stop:?} within [{now:?}, {upto:?}]"
    );
    assert!(
        staged.iter().all(|output| output.at <= reached),
        "no output beyond the accepted boundary"
    );
    Ok((stop, staged))
}

/// Execute one boot like a single-machine driver until the guest ends it.
fn until_exit<M: Machine<Context = ()>>(
    machine: &mut M,
    clock: &SimulationClock,
    rng: &mut CountingRng,
) -> (Timestamp, ExitReason, Vec<Output>)
where
    M::Error: Debug,
{
    let guard = HangGuard::new("native kernel terminal transport must return", HOST_LIMIT);
    let limit = clock.now() + BOOT_LIMIT;
    let mut outputs = Vec::new();
    loop {
        guard
            .remaining()
            .expect("kernel terminal infrastructure hang");
        let upto = clock.now() + QUANTUM;
        assert!(
            upto <= limit,
            "guest did not end within the simulated bound"
        );
        let (stop, staged) = step(machine, clock, rng, Vec::new(), at(upto)).unwrap();
        outputs.extend(staged);
        match stop {
            StopReason::BoundaryReached => clock.set(upto),
            StopReason::Blocked { reached, deadline } => {
                let deadline = deadline.expect("terminal fixture blocked without a timer");
                assert!(deadline > reached);
                clock.set(deadline.as_nanos());
            }
            StopReason::Exited { reached, reason } => {
                assert!(reached.as_nanos() <= upto && reached.as_nanos() >= clock.now());
                assert!(
                    outputs.iter().all(|output| output.at <= reached),
                    "no output beyond the exit"
                );
                clock.set(reached.as_nanos());
                return (reached, reason, outputs);
            }
        }
    }
}

fn text(outputs: &[Output], console: bool) -> String {
    let bytes: Vec<u8> = outputs
        .iter()
        .filter_map(|output| match &output.payload {
            Response::BootConsole { bytes } if console => Some(bytes.as_slice()),
            Response::ApplicationOutput { bytes, .. } if !console => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// One boot ended by `path`: the guest's request, not process exit, ended it.
fn check_boot(path: &Path, reason: ExitReason, outputs: &[Output]) {
    assert_eq!(reason, path.reason, "{}", path.mode);
    assert!(
        outputs
            .iter()
            .any(|output| output.payload == Response::Ready),
        "{}: application launched",
        path.mode
    );
    assert_eq!(
        text(outputs, false),
        format!("KERNEL_TERMINAL {}\n", path.mode),
        "{}: application marker",
        path.mode
    );
    assert!(
        text(outputs, true).contains(path.console),
        "{}: kernel console {:?}",
        path.mode,
        text(outputs, true)
    );
    assert!(
        !outputs
            .iter()
            .any(|output| matches!(output.payload, Response::ApplicationOutcome { .. })),
        "{}: the application never returned from its request",
        path.mode
    );
}

/// Direct Machine contract for one terminal path on one backend. Only valid
/// calls reach the original and its continuation copy; the refused call goes
/// to a disposable copy, because `Err` promises no resumable state. RNG use is
/// compared, not sized: how much a boot draws, and when, is backend-specific.
pub fn terminal_contract<M, R>(path: &Path, mut machine: M, restore: R)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
    R: Fn(&M::Snapshot) -> M,
{
    let clock = SimulationClock::new(0);
    let mut rng = CountingRng(SeededRng::new(41), 0);
    let (exit, reason, outputs) = until_exit(&mut machine, &clock, &mut rng);
    check_boot(path, reason, &outputs);
    let drawn = rng.1;
    let exited = |reached| StopReason::Exited { reached, reason };

    // Zero-budget observations keep the reason, execute nothing and draw nothing.
    let (stop, staged) = step(&mut machine, &clock, &mut rng, Vec::new(), exit).unwrap();
    assert_eq!((stop, staged.len()), (exited(exit), 0));
    let later = at(exit.as_nanos() + 5 * QUANTUM);
    clock.set(later.as_nanos());
    let (stop, staged) = step(&mut machine, &clock, &mut rng, Vec::new(), later).unwrap();
    assert_eq!((stop, staged.len()), (exited(later), 0));

    // The terminal state is self-contained: a restored copy reports it too.
    let saved = machine.snapshot().unwrap();
    assert_eq!(rng.1, drawn, "exited observations and capture draw nothing");
    let mut restored = restore(&saved);
    let mut restored_rng = rng.clone();
    let (stop, staged) = step(&mut restored, &clock, &mut restored_rng, Vec::new(), later).unwrap();
    assert_eq!((stop, staged.len()), (exited(later), 0));
    assert_eq!(restored_rng.1, drawn);

    // Positive budget needs a live machine. Check the refusal on a disposable
    // restored copy with its own RNG, then discard both.
    {
        let mut disposable = restore(&saved);
        let mut disposable_rng = rng.clone();
        let upto = at(later.as_nanos() + 1);
        let refused = step(
            &mut disposable,
            &clock,
            &mut disposable_rng,
            Vec::new(),
            upto,
        );
        assert!(refused.is_err(), "positive budget after exit is refused");
    }

    // Reboot starts a fresh boot, on the original and on the restored machine
    // alike, with identical outputs, stops and RNG consumption.
    let mut boots = Vec::new();
    for (machine, rng) in [(&mut machine, &mut rng), (&mut restored, &mut restored_rng)] {
        clock.set(later.as_nanos());
        let (stop, staged) = step(machine, &clock, rng, vec![Request::Reboot], later).unwrap();
        assert_eq!((stop, staged.len()), (StopReason::BoundaryReached, 0));
        let (second, reason, outputs) = until_exit(machine, &clock, rng);
        assert!(second > later);
        check_boot(path, reason, &outputs);
        assert!(rng.1 > drawn, "the new boot draws fresh entropy");
        boots.push((second, outputs, rng.1));
    }
    assert!(
        boots[0] == boots[1],
        "{}: restored terminal state reboots identically",
        path.mode
    );
}
