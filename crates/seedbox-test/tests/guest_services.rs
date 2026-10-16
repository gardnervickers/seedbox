#![cfg(unix)]
//! Actual Linux guest service acceptance. Opt-in separately for each backend;
//! inputs are statically linked helper + fixture initramfs from guest.nix.
#[path = "support/epoch.rs"]
mod epoch;
#[path = "support/prepared_application.rs"]
mod prepared;
use seedbox_machine::{
    clock::Clock,
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv,
    rng::RngStream,
};
use std::{
    fmt::Debug,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

struct SimulationClock(AtomicU64);
impl SimulationClock {
    fn new(n: u64) -> Self {
        Self(AtomicU64::new(n))
    }
    fn set(&self, n: u64) {
        self.0.store(n, Ordering::Relaxed);
    }
}
impl Clock for SimulationClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}
struct FixtureRng(u8);
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}

struct Session<M> {
    machine: M,
    clock: SimulationClock,
    rng: FixtureRng,
    inbox: std::collections::VecDeque<pv::Input>,
    events: Vec<pv::Output>,
    exited: bool,
    started: Instant,
}
impl<M: Machine<Context = ()>> Session<M>
where
    M::Error: Debug,
{
    fn new(machine: M) -> Self {
        Self {
            machine,
            clock: SimulationClock::new(0),
            rng: FixtureRng(7),
            inbox: Default::default(),
            events: Vec::new(),
            exited: false,
            started: Instant::now(),
        }
    }
    fn pump(&mut self) {
        assert!(
            self.started.elapsed() < Duration::from_secs(120),
            "native service deadline: {:?}",
            self.events
        );
        assert!(
            !self.exited,
            "guest exited before expected event: {:?}",
            self.events
        );
        let start = Timestamp::from_nanos(self.clock.now());
        let upto = Timestamp::from_nanos(self.clock.now() + 5_000_000);
        let mut staged = Vec::new();
        let result = self.machine.run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &self.clock,
                rng: &mut self.rng,
                inbox: pv::Inbox::new(&mut self.inbox, start),
                outbox: pv::Outbox::new(&mut staged, start, upto),
            },
            upto,
        );
        let reason = result.unwrap_or_else(|error| {
            let console = self
                .events
                .iter()
                .chain(&staged)
                .filter_map(|event| match &event.payload {
                    pv::Response::BootConsole { bytes } => Some(bytes.as_slice()),
                    _ => None,
                })
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            panic!(
                "native guest failed: {error:?}; boot console:\n{}",
                String::from_utf8_lossy(&console)
            );
        });
        let reached = match reason {
            StopReason::BoundaryReached => upto,
            StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
        };
        assert!(
            staged.iter().all(|event| event.at <= reached),
            "output crossed committed stop"
        );
        self.events.extend(staged);
        if let StopReason::Exited { reason, .. } = reason {
            assert_eq!(reason, seedbox_machine::machine::ExitReason::Shutdown);
        }
        self.exited = matches!(reason, StopReason::Exited { .. });
        self.clock.set(upto.as_nanos());
    }
    fn until(&mut self, predicate: impl Fn(&pv::Response) -> bool) {
        while !self.events.iter().any(|event| predicate(&event.payload)) {
            self.pump();
        }
    }
    fn until_output(&mut self, stream: pv::Stream, needle: &[u8]) {
        loop {
            let bytes = self
                .events
                .iter()
                .filter_map(|event| match &event.payload {
                    pv::Response::ApplicationOutput {
                        stream: actual,
                        bytes,
                    } if *actual == stream => Some(bytes.as_slice()),
                    _ => None,
                })
                .flatten()
                .copied()
                .collect::<Vec<_>>();
            if bytes.windows(needle.len()).any(|window| window == needle) {
                return;
            }
            self.pump();
        }
    }
    fn accept_zero(&mut self) {
        let now = Timestamp::from_nanos(self.clock.now());
        let rng_before = self.rng.0;
        let events_before = self.events.len();
        self.machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock: &self.clock,
                    rng: &mut self.rng,
                    inbox: pv::Inbox::new(&mut self.inbox, now),
                    outbox: pv::Outbox::new(&mut self.events, now, now),
                },
                now,
            )
            .unwrap();
        assert!(self.inbox.is_empty());
        assert_eq!(self.rng.0, rng_before);
        assert_eq!(
            self.events.len(),
            events_before,
            "zero-budget acceptance executes no guest code"
        );
    }
    fn command(&mut self, id: u32, command: pv::ApplicationCommand, expected: pv::CommandResult) {
        let id = std::num::NonZeroU32::new(id).unwrap();
        self.inbox.push_back(pv::Input {
            at: Timestamp::from_nanos(self.clock.now()),
            payload: pv::Request::Application { id, command },
        });
        self.until(|payload| matches!(payload, pv::Response::CommandAcknowledged { id: actual, result } if *actual == id && *result == expected));
    }
}
fn applications<M: Machine<Context = ()>>(machine: M)
where
    M::Error: Debug,
{
    use pv::{ApplicationCommand::*, CommandResult::*, Response, Stream};
    let mut session = Session::new(machine);
    session.until(|payload| matches!(payload, Response::Ready));
    session.until_output(Stream::Stdout, b"app\0out\n");
    session.until_output(Stream::Stderr, b"\xff\0z");
    session.command(17, Pause, Completed);
    session.command(18, Resume, Completed);
    session.command(19, Stop, Completed);
    assert!(!session
        .events
        .iter()
        .any(|event| matches!(event.payload, Response::ApplicationOutcome { .. })));
    session.command(20, Start, Completed);
    session.command(21, Start, Rejected);
    session.until(|payload| matches!(payload, Response::ApplicationOutcome { code: 23 }));
    assert!(!session.exited, "natural exit must leave the guest alive");
    session.accept_zero();
    session.command(22, Start, Completed);
    while session
        .events
        .iter()
        .filter(|event| matches!(event.payload, Response::ApplicationOutcome { code: 23 }))
        .count()
        < 2
    {
        session.pump();
    }
    assert!(!session.exited);
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Shutdown,
    });
    session.accept_zero();
    while !session.exited {
        session.pump();
    }
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| matches!(event.payload, Response::Ready))
            .count(),
        1
    );
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| matches!(event.payload, Response::ApplicationOutcome { .. }))
            .count(),
        2
    );
}
fn shutdown<M: Machine<Context = ()>>(machine: M)
where
    M::Error: Debug,
{
    use pv::{Response, Stream};
    let mut session = Session::new(machine);
    session.until_output(Stream::Stdout, b"app\0out\n");
    // A host-paused guest accepts both at zero budget; its command must reach
    // the application supervisor before the following shutdown bytes.
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Application {
            id: std::num::NonZeroU32::new(99).unwrap(),
            command: pv::ApplicationCommand::Resume,
        },
    });
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Shutdown,
    });
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Shutdown,
    });
    session.accept_zero();

    session.until_output(Stream::Stdout, b"app\0out\ncleaned\n");
    let resume = session.events.iter().position(|output| matches!(output.payload, Response::CommandAcknowledged { id, result: pv::CommandResult::Completed } if id.get() == 99)).expect("Resume preceding Shutdown must be acknowledged");
    let cleanup = session.events.iter().position(|output| matches!(&output.payload, Response::ApplicationOutput { bytes, .. } if bytes.windows(7).any(|b| b == b"cleaned"))).unwrap();
    assert!(
        resume < cleanup,
        "same-time app command precedes shutdown cleanup"
    );

    while !session.exited {
        session.pump();
    }
    assert!(!session
        .events
        .iter()
        .any(|event| matches!(event.payload, Response::ApplicationOutcome { .. })));
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Shutdown,
    });
    session.accept_zero();
}

fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
fn tcg() -> seedbox_machine_tcg::machine::tcg::TcgMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[test]
#[ignore = "requires patched QEMU and SEEDBOX_TCG_SERVICE_INITRD static Linux helper+fixture"]
fn tcg_native_application_controls_logs_and_outcome() {
    applications(tcg());
}
#[test]
#[ignore = "requires patched QEMU and SEEDBOX_TCG_SERVICE_INITRD static Linux helper+fixture"]
fn tcg_native_shutdown_preserves_graceful_cleanup() {
    shutdown(tcg());
}

#[cfg(seedbox_pv)]
fn pv() -> seedbox_machine_pv::machine::pv::PVMachine {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = seedbox_machine_pv::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    backend.create(&definition).unwrap()
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_application_controls_logs_and_outcome() {
    applications(pv());
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor, worker/kernel and SEEDBOX_PV_SERVICE_INITRD static helper+fixture"]
fn pv_native_shutdown_preserves_graceful_cleanup() {
    shutdown(pv());
}
