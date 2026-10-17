//! Native accepted-command snapshot continuation, separate from host codec tests.
use super::{Machine, RunContext, StopReason, Timestamp};
use crate::{
    clock::{Clock, SimulationClock},
    pv::{self, ApplicationCommand as Command, Response},
    rng::SeededRng,
};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{collections::VecDeque, fmt::Debug, num::NonZeroU32, time::Duration};

struct Session<M> {
    machine: M,
    clock: SimulationClock,
    rng: SeededRng,
    inbox: VecDeque<pv::Input>,
    events: Vec<pv::Output>,
    exited: bool,
    guard: HangGuard,
    progress: GuestProgress,
    deadline: u64,
    last_stop: Option<StopReason>,
}
impl<M: Machine<Context = ()>> Session<M>
where
    M::Error: Debug,
{
    fn new(machine: M, now: u64, rng: SeededRng, deadline: u64) -> Self {
        Self {
            machine,
            clock: SimulationClock::new(now),
            rng,
            inbox: VecDeque::new(),
            events: Vec::new(),
            exited: false,

            // Infrastructure only: snapshots and transport may be slow on loaded hosts.
            guard: HangGuard::new(
                "native snapshot continuation infrastructure",
                Duration::from_secs(120),
            ),
            progress: GuestProgress::new(
                "snapshot fixture boot/commands/outcome/shutdown",
                Duration::from_nanos(now),
                Duration::from_nanos(deadline),
                0,
            )
            .unwrap(),
            deadline,
            last_stop: None,
        }
    }
    fn run(&mut self, budget: u64) {
        assert!(!self.exited);

        self.guard.remaining().unwrap();
        let now = Timestamp::from_nanos(self.clock.now());
        // Zero-budget ownership checks are not unfinished guest advancement.
        let upto = if budget == 0 {
            now
        } else {
            Timestamp::from_nanos(
                self.progress
                    .next(
                        Duration::from_nanos(now.as_nanos()),
                        Duration::from_nanos(budget),
                    )
                    .unwrap_or_else(|error| {
                        panic!(
                            "{error}: phase={}; exited={}; last_stop={:?}; inbox={:?}; events={:?}",
                            self.progress.phase(),
                            self.exited,
                            self.last_stop,
                            self.inbox,
                            self.events
                        )
                    })
                    .as_nanos()
                    .try_into()
                    .unwrap(),
            )
        };
        let mut outputs = Vec::new();
        let stop = self
            .machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock: &self.clock,
                    rng: &mut self.rng,
                    inbox: pv::Inbox::new(&mut self.inbox, now),
                    outbox: pv::Outbox::new(&mut outputs, now, upto),
                },
                upto,
            )
            .unwrap();
        self.last_stop = Some(stop);
        let reached = match stop {
            StopReason::BoundaryReached => upto,
            StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
        };
        assert!(outputs.iter().all(|output| output.at <= reached));
        self.events.extend(outputs);
        if let StopReason::Exited { reason, .. } = stop {
            assert_eq!(reason, super::ExitReason::Shutdown);
        }
        self.exited = matches!(stop, StopReason::Exited { .. });
        self.clock.set(upto.as_nanos());
    }
    fn offer(&mut self, id: u32, command: Command) {
        self.inbox.push_back(pv::Input {
            at: Timestamp::from_nanos(self.clock.now()),
            payload: pv::Request::Application {
                id: NonZeroU32::new(id).unwrap(),
                command,
            },
        });
    }
    fn ack_count(&self, id: u32) -> usize {
        self.events.iter().filter(|output| matches!(output.payload, Response::CommandAcknowledged { id: actual, result: pv::CommandResult::Completed } if actual.get() == id)).count()
    }
    fn until_ack(&mut self, id: u32) {
        while self.ack_count(id) == 0 {
            self.run(5_000_000);
        }
    }
    fn finish(&mut self) -> Vec<pv::Output> {
        self.until_ack(71);
        self.offer(72, Command::Resume);
        self.until_ack(72);
        self.offer(73, Command::Stop);
        self.until_ack(73);
        assert!(!self
            .events
            .iter()
            .any(|output| matches!(output.payload, Response::ApplicationOutcome { .. })));
        self.offer(74, Command::Start);
        self.until_ack(74);
        while !self
            .events
            .iter()
            .any(|output| matches!(output.payload, Response::ApplicationOutcome { code: 23 }))
        {
            self.run(5_000_000);
        }
        assert!(!self.exited);
        let before = self.events.len();
        self.run(0);
        assert_eq!(self.events.len(), before);
        for id in 71..=74 {
            assert_eq!(self.ack_count(id), 1);
        }
        assert_eq!(
            self.events
                .iter()
                .filter(|output| matches!(
                    output.payload,
                    Response::ApplicationOutcome { code: 23 }
                ))
                .count(),
            1
        );
        let stdout: Vec<_> = self
            .events
            .iter()
            .filter_map(|output| match &output.payload {
                Response::ApplicationOutput {
                    stream: pv::Stream::Stdout,
                    bytes,
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert_eq!(stdout, b"app\0out\n");
        self.events
            .iter()
            .filter(|output| !matches!(output.payload, Response::BootConsole { .. }))
            .cloned()
            .collect()
    }
    fn restart_and_shutdown(&mut self) -> Vec<pv::Output> {
        self.offer(75, Command::Start);
        self.until_ack(75);
        while !self
            .events
            .iter()
            .any(|output| matches!(output.payload, Response::ApplicationOutcome { code: 23 }))
        {
            self.run(5_000_000);
        }
        assert!(!self.exited);
        assert_eq!(
            self.events
                .iter()
                .filter(|output| matches!(output.payload, Response::ApplicationOutcome { .. }))
                .count(),
            1
        );
        let outcome = self
            .events
            .iter()
            .position(|output| matches!(output.payload, Response::ApplicationOutcome { .. }))
            .unwrap();
        assert!(!self.events[outcome + 1..]
            .iter()
            .any(|output| matches!(output.payload, Response::ApplicationOutput { .. })));
        self.inbox.push_back(pv::Input {
            at: Timestamp::from_nanos(self.clock.now()),
            payload: pv::Request::Shutdown,
        });
        self.run(0);
        while !self.exited {
            self.run(5_000_000);
        }
        self.events
            .iter()
            .filter(|output| !matches!(output.payload, Response::BootConsole { .. }))
            .cloned()
            .collect()
    }
}

pub fn check<M, F>(machine: M, restore: F)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
    F: Fn(&M::Snapshot) -> M,
{
    // guest_service_fixture.c sleeps one guest second on each completed Start.
    // Allow 10 simulated seconds for kernel/init/Ready, one second each for
    // Pause/Resume/Stop/two Starts/Shutdown protocol work, and two one-second sleeps.
    // This bounds the entire finite sequence, not each poll; clock always advances
    // to the requested limit, so no same-instant advancement rounds are needed.
    let mut original = Session::new(machine, 0, SeededRng::new(7), 18_000_000_000);
    while !original.events.iter().any(|output| {
        matches!(
            output.payload,
            Response::ApplicationOutput {
                stream: pv::Stream::Stderr,
                ..
            }
        )
    }) {
        original.run(5_000_000);
    }
    original.events.clear();
    original.offer(71, Command::Pause);
    original.run(0);
    assert!(
        original.inbox.is_empty(),
        "zero-budget run must accept ownership"
    );
    assert_eq!(
        original.ack_count(71),
        0,
        "guest instructions must not acknowledge during zero budget"
    );
    let snapshot = original.machine.snapshot().unwrap();
    let now = original.clock.now();
    let rng = original.rng.clone();
    // Finish and destroy the original before restoring: mutable execution must
    // not alter the owned snapshot or its accepted command and guest RX state.
    let deadline = original.deadline;
    let expected = original.finish();
    drop(original);
    let mut branch = Session::new(restore(&snapshot), now, rng.clone(), deadline);
    assert_eq!(branch.finish(), expected);
    drop(branch);
    let mut second = Session::new(restore(&snapshot), now, rng, deadline);
    assert_eq!(second.finish(), expected);
    let stopped_app = second.machine.snapshot().unwrap();
    let now = second.clock.now();
    let rng = second.rng.clone();
    second.events.clear();
    let expected = second.restart_and_shutdown();
    drop(second);
    let mut restored = Session::new(restore(&stopped_app), now, rng, deadline);
    restored.run(0);
    assert!(
        restored.events.is_empty(),
        "snapshot must not replay old outcome"
    );
    assert_eq!(restored.restart_and_shutdown(), expected);
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    // Portable stopped-machine fixture: records actual requested limits. A
    // blocked guest must still retain this helper's caller-clock update policy.
    #[derive(Default)]
    struct Stopped {
        limits: Vec<u64>,
        complete_at: Option<u64>,
    }
    impl Machine for Stopped {
        type Context = ();
        type Error = std::convert::Infallible;
        type Snapshot = ();
        fn run(
            &mut self,
            context: &mut RunContext<'_, ()>,
            upto: Timestamp,
        ) -> Result<StopReason, Self::Error> {
            self.limits.push(upto.as_nanos());
            if self.complete_at == Some(upto.as_nanos()) && upto.as_nanos() > context.clock.now() {
                context
                    .outbox
                    .emit(pv::Output {
                        at: upto,
                        payload: Response::ApplicationOutcome { code: 23 },
                    })
                    .unwrap();
                return Ok(StopReason::BoundaryReached);
            }
            Ok(StopReason::Blocked {
                reached: Timestamp::from_nanos(context.clock.now()),
                deadline: None,
            })
        }
        fn snapshot(&mut self) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    #[test]
    fn deadline_edge_caps_actual_run_and_preserves_zero_budget() {
        let mut session = Session::new(
            Stopped {
                complete_at: Some(7),
                ..Default::default()
            },
            0,
            SeededRng::new(7),
            7,
        );
        // Oversized quantum must not leak to the machine/outbox.
        session.run(10);
        assert_eq!(session.machine.limits, [7]);
        assert_eq!(session.clock.now(), 7);
        assert!(matches!(
            session.events[0].payload,
            Response::ApplicationOutcome { code: 23 }
        ));
        // Completion exactly at the horizon is valid, including a subsequent
        // non-executing ownership assertion at that same clock position.
        session.run(0);
        assert_eq!(session.machine.limits, [7, 7]);
        assert_eq!(session.events.len(), 1);
    }

    #[test]
    fn snapshot_branches_keep_absolute_horizon_and_stalled_guest_is_finite() {
        let mut original = Session::new(Stopped::default(), 0, SeededRng::new(7), 7);
        original.run(3);
        let mut branch = Session::new(
            Stopped::default(),
            original.clock.now(),
            original.rng.clone(),
            original.deadline,
        );
        for session in [&mut original, &mut branch] {
            session.run(10);
            assert_eq!(session.clock.now(), 7);
            assert_eq!(session.machine.limits.last(), Some(&7));
            let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| session.run(1)));
            assert!(
                failure.is_err(),
                "unfinished blocked guest must exhaust its original horizon"
            );
            assert_eq!(session.clock.now(), 7);
        }
    }
}
