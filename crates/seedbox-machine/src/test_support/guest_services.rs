//! Shared guest-service protocol scenarios; each backend owns its native entrypoints.
use crate::{
    clock::{Clock, SimulationClock},
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv,
    rng::RngStream,
};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{fmt::Debug, time::Duration};

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
    guard: HangGuard,
    progress: GuestProgress,
    last_stop: Option<StopReason>,
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

            // guest_service_fixture.c: one kernel/init boot (10s allowance),
            // six app commands plus Shutdown (1s protocol allowance each), and
            // two completed Starts sleeping one guest second each. Shutdown's
            // shorter path shares this finite whole-fixture horizon. No renewed
            // per-poll budgets or same-instant rounds: pump commits requested upto.
            guard: HangGuard::new(
                "native guest-service infrastructure",
                Duration::from_secs(120),
            ),
            progress: GuestProgress::new(
                "guest-service boot/commands/sleeps/shutdown",
                Duration::ZERO,
                Duration::from_secs(19),
                0,
            )
            .unwrap(),
            last_stop: None,
        }
    }
    fn pump(&mut self) {
        self.guard.remaining().unwrap();
        assert!(
            !self.exited,
            "guest exited before expected event: {:?}",
            self.events
        );
        let start = Timestamp::from_nanos(self.clock.now());
        let upto = Timestamp::from_nanos(
            self.progress
                .next(
                    Duration::from_nanos(self.clock.now()),
                    Duration::from_millis(5),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "{error}: phase={}; last_stop={:?}; inbox={:?}; events={:?}",
                        self.progress.phase(),
                        self.last_stop,
                        self.inbox,
                        self.events
                    )
                })
                .as_nanos()
                .try_into()
                .unwrap(),
        );
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
        self.last_stop = Some(reason);
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
            assert_eq!(reason, crate::machine::ExitReason::Shutdown);
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
pub fn applications<M: Machine<Context = ()>>(machine: M)
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
pub fn shutdown<M: Machine<Context = ()>>(machine: M)
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

/// Kernel OOM and SIGKILL subjects share this native oracle. Completion is the
/// explicit application outcome, never timed absence of a kernel event.
/// Call with full-state capture configured to compare independent fresh runs.
fn oom_run<M>(
    machine: M,
    expect_oom: bool,
) -> (
    Vec<pv::Output>,
    crate::machine::ExecutionEvidence,
    crate::machine::StateEvidence,
)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
{
    let mut session = Session::new(machine);
    session.until(|r| matches!(r, pv::Response::ApplicationOutcome { .. }));
    let outcome = session
        .events
        .iter()
        .position(|o| matches!(o.payload, pv::Response::ApplicationOutcome { code: 137 }))
        .expect("SIGKILL outcome 137");
    let kills: Vec<_> = session
        .events
        .iter()
        .enumerate()
        .filter_map(|(i, o)| match &o.payload {
            pv::Response::OutOfMemoryKill { kill } => Some((i, kill)),
            _ => None,
        })
        .collect();
    if expect_oom {
        assert_eq!(kills.len(), 1, "one real kernel victim");
        let (index, kill) = kills[0];
        assert!(index < outcome, "kernel report precedes outcome");
        assert!(kill.application);
        assert_eq!(kill.command, "oom-fixture");
        assert!(kill.anon_rss_kib > 0);
        let stdout: Vec<_> = session
            .events
            .iter()
            .filter_map(|o| match &o.payload {
                pv::Response::ApplicationOutput {
                    stream: pv::Stream::Stdout,
                    bytes,
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(stdout, format!("victim pid={}\n", kill.pid).as_bytes());
    } else {
        assert!(kills.is_empty(), "user SIGKILL is not OOM");
    }
    let execution = session
        .machine
        .execution_evidence()
        .unwrap()
        .expect("native witness");
    let state = session
        .machine
        .state_evidence()
        .unwrap()
        .expect("configured full state");
    // Verify normal cleanup, rather than ending by dropping a live native VM.
    session.inbox.push_back(pv::Input {
        at: Timestamp::from_nanos(session.clock.now()),
        payload: pv::Request::Shutdown,
    });
    while !session.exited {
        session.pump();
    }
    (session.events, execution, state)
}

/// Replay from a retained cold snapshot also compares the complete witness and
/// full state; fresh execution is checked separately by the backend entrypoint.
pub fn oom_outcome<M, F>(
    mut machine: M,
    expect_oom: bool,
    restore: F,
) -> (
    Vec<pv::Output>,
    crate::machine::ExecutionEvidence,
    crate::machine::StateEvidence,
)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
    F: Fn(&M::Snapshot) -> M,
{
    let snapshot = machine.snapshot().unwrap();
    let original = oom_run(machine, expect_oom);
    let replay = oom_run(restore(&snapshot), expect_oom);
    assert_eq!(
        original, replay,
        "retained-input replay MATCH: outputs, native witness and full state"
    );
    original
}
