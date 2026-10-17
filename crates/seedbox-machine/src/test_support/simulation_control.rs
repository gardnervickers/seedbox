use crate::{
    clock::Clock,
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{self, SimulationControlId},
    rng::RngStream,
};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{
    collections::VecDeque,
    fmt::Debug,
    num::NonZeroU32,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
struct TestClock(AtomicU64);
impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

#[derive(Clone)]
struct TestRng(u64);
impl RngStream for TestRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0 as u8;
            self.0 = self.0.wrapping_add(1);
        }
    }
}

/// Caller-owned immutable provenance; the machine payload does not claim a boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub boot: u64,
    pub at: Timestamp,
    pub id: SimulationControlId,
    pub payload: Vec<u8>,
}

pub struct Session<M> {
    pub machine: M,
    now: TestClock,
    rng: TestRng,
    inbox: VecDeque<pv::Input>,
    pub outputs: Vec<pv::Output>,
    pub calls: Vec<Call>,
    boot: u64,
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
    pub fn new(machine: M) -> Self {
        // simulation-control.c has no sleeps or compute loops: six bounded
        // request/reply exchanges, two app commands, and at most two reboots.
        // Budget 10 simulated seconds per kernel/init boot (three boots), plus
        // one second for each of the eight protocol transitions. No same-instant
        // rounds: run commits its requested end, including Blocked stops.
        Self::continued(machine, 0, TestRng(7), 0, 38_000_000_000)
    }
    fn continued(machine: M, now: u64, rng: TestRng, boot: u64, deadline: u64) -> Self {
        Self {
            machine,
            now: TestClock(AtomicU64::new(now)),
            rng,
            inbox: VecDeque::new(),
            outputs: Vec::new(),
            calls: Vec::new(),
            boot,
            exited: false,

            guard: HangGuard::new(
                "native simulation-control infrastructure",
                Duration::from_secs(120),
            ),
            progress: GuestProgress::new(
                "control fixture boot/exchanges/resets",
                Duration::from_nanos(now),
                Duration::from_nanos(deadline),
                0,
            )
            .unwrap(),
            deadline,
            last_stop: None,
        }
    }
    pub fn run(&mut self, budget: u64) {
        self.guard.remaining().unwrap();
        assert!(
            !self.exited
                || (budget == 0
                    && self
                        .inbox
                        .iter()
                        .any(|input| matches!(input.payload, pv::Request::Reboot))),
            "guest exited without an admitted reboot; outputs={:?}",
            self.outputs
        );
        let start = at(self.now.now());
        // Keep zero-budget reply/reboot ownership checks separate from progress.
        let end = if budget == 0 {
            start
        } else {
            at(self.progress.next(Duration::from_nanos(start.as_nanos()), Duration::from_nanos(budget))
                .unwrap_or_else(|error| panic!("{error}: phase={}; exited={}; last_stop={:?}; inbox={:?}; calls={:?}; outputs={:?}", self.progress.phase(), self.exited, self.last_stop, self.inbox, self.calls, self.outputs)).as_nanos().try_into().unwrap())
        };
        let mut outputs = Vec::new();
        let stop = self
            .machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &self.now,
                    rng: &mut self.rng,
                    execution: Default::default(),
                    inbox: pv::Inbox::new(&mut self.inbox, start),
                    outbox: pv::Outbox::new(&mut outputs, start, end),
                },
                end,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "control native failure {error:?}; outputs={:?}",
                    self.outputs
                )
            });
        self.last_stop = Some(stop);
        let reached = match stop {
            StopReason::BoundaryReached => end,
            StopReason::Blocked { reached, .. } | StopReason::Exited { reached, .. } => reached,
        };
        assert!(
            outputs
                .iter()
                .all(|output| output.at >= start && output.at <= reached)
        );
        // Publication happens only after the synchronous machine has stopped.
        for output in &outputs {
            if let pv::Response::SimulationControl { id, payload } = &output.payload {
                assert!(
                    !self
                        .calls
                        .iter()
                        .any(|call| call.boot == self.boot && call.id == *id),
                    "duplicate published call"
                );
                self.calls.push(Call {
                    boot: self.boot,
                    at: output.at,
                    id: *id,
                    payload: payload.clone(),
                });
            }
        }
        self.outputs.extend(outputs);
        self.exited = matches!(stop, StopReason::Exited { .. });
        self.now.0.store(end.as_nanos(), Ordering::Relaxed);
    }
    pub fn until_call(&mut self, marker: &str) -> Call {
        let payload = format!("\"fixture\":\"{marker}\"");
        loop {
            if let Some(call) = self.calls.iter().find(|call| {
                call.boot == self.boot && String::from_utf8_lossy(&call.payload).contains(&payload)
            }) {
                return call.clone();
            }
            self.run(5_000_000);
        }
    }
    fn stdout(&self) -> Vec<u8> {
        self.outputs
            .iter()
            .filter_map(|output| match &output.payload {
                pv::Response::ApplicationOutput {
                    stream: pv::Stream::Stdout,
                    bytes,
                } => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }
    pub fn until_marker(&mut self, marker: &[u8]) {
        while !self
            .stdout()
            .windows(marker.len())
            .any(|part| part == marker)
        {
            self.run(5_000_000);
        }
    }
    pub fn reply(&mut self, call: &Call, marker: &str) -> bool {
        // Intentionally offer even a retired boot's reply through the ordinary
        // inbox. Lifetime-unique public sessions let the machine discard it.
        if self.exited {
            return false;
        }
        self.inbox.push_back(pv::Input {
            at: at(self.now.now()),
            payload: pv::Request::SimulationReply {
                id: call.id,
                payload: format!(
                    "{{\"version\":1,\"id\":{},\"fixture_reply\":\"{marker}\"}}",
                    call.id.request
                )
                .into_bytes(),
            },
        });
        true
    }
    pub fn accept_zero(&mut self) {
        let outputs = self.outputs.len();
        let calls = self.calls.len();
        self.run(0);
        assert!(
            self.inbox.is_empty(),
            "machine must accept pending reply ownership"
        );
        assert_eq!(
            self.outputs.len(),
            outputs,
            "zero budget executes no guest response code"
        );
        assert_eq!(self.calls.len(), calls);
    }
    fn command(&mut self, id: u32, command: pv::ApplicationCommand) {
        let id = NonZeroU32::new(id).unwrap();
        self.inbox.push_back(pv::Input {
            at: at(self.now.now()),
            payload: pv::Request::Application { id, command },
        });
        while !self.outputs.iter().any(|output| matches!(output.payload,
            pv::Response::CommandAcknowledged { id: actual, result: pv::CommandResult::Completed } if actual == id)) { self.run(5_000_000); }
    }
    fn reboot(&mut self, crash_first: bool) {
        if crash_first {
            self.inbox.push_back(pv::Input {
                at: at(self.now.now()),
                payload: pv::Request::Crash,
            });
            self.accept_zero();
            assert!(self.exited);
        }
        self.inbox.push_back(pv::Input {
            at: at(self.now.now()),
            payload: pv::Request::Reboot,
        });
        self.boot = self.boot.checked_add(1).unwrap();
        self.accept_zero();
    }
}

pub fn round_trip<M: Machine<Context = ()>>(machine: M)
where
    M::Error: Debug,
{
    let mut session = Session::new(machine);
    let first = session.until_call("first");
    assert_eq!((first.id.session.get(), first.id.request.get()), (1, 1));
    assert_eq!(first.payload, br#"{"version":1,"id":1,"fixture":"first"}"#);
    assert!(first.at > at(0));
    assert!(session.reply(&first, "first"));
    session.accept_zero();
    let second = session.until_call("second");
    assert_eq!((second.id.session.get(), second.id.request.get()), (1, 2));
    assert!(second.at > first.at);
    session.until_marker(b"FIRST\n");
    assert!(session.reply(&second, "second"));
    let abandoned = session.until_call("abandoned");
    let reconnected = session.until_call("reconnected");
    assert_eq!(
        (abandoned.id.session.get(), abandoned.id.request.get()),
        (1, 3)
    );
    assert_eq!(
        (reconnected.id.session.get(), reconnected.id.request.get()),
        (2, 1)
    );
    assert!(reconnected.at >= abandoned.at);
    // Same-time FIFO supplies stale first, current second. Only current bytes
    // may reach the new socket; the fixture checks the exact response line.
    assert!(session.reply(&abandoned, "stale"));
    assert!(session.reply(&reconnected, "reconnected"));
    session.accept_zero();
    let before_stop = session.until_call("before-stop");
    session.until_marker(b"RECONNECTED\n");
    session.command(100, pv::ApplicationCommand::Stop);
    session.command(101, pv::ApplicationCommand::Start);
    let restarted = session.until_call("restarted");
    assert_eq!(
        (restarted.id.session.get(), restarted.id.request.get()),
        (3, 1)
    );
    assert!(session.reply(&before_stop, "stale"));
    assert!(session.reply(&restarted, "restarted"));
    session.accept_zero();
    session.until_marker(b"RESTARTED\n");
    assert_eq!(
        session.stdout(),
        b"FIRST\nSECOND\nCLOSED\nRECONNECTED\nRESTARTED\n"
    );
    assert_eq!(session.calls.len(), 6);
    assert!(
        !session
            .outputs
            .iter()
            .any(|output| matches!(output.payload, pv::Response::ApplicationOutcome { .. }))
    );
}

pub fn late_reply_after_reset<M: Machine<Context = ()>>(machine: M)
where
    M::Error: Debug,
{
    let mut session = Session::new(machine);
    let mut retired = session.until_call("first");
    for (reset, crash_first) in [false, true].into_iter().enumerate() {
        let accepted = session.calls.clone();
        session.reboot(crash_first);
        // There is no fresh wire session yet. The old public identity must
        // already be retired, with no caller-side boot filter.
        assert!(session.reply(&retired, "stale-before-connect"));
        session.accept_zero();
        assert_eq!(
            session.calls, accepted,
            "reset cannot revoke published host calls"
        );
        let fresh = session.until_call("first");
        assert_eq!(fresh.boot, retired.boot + 1);
        assert_eq!(fresh.id.session.get(), retired.id.session.get() + 1);
        assert_eq!(fresh.id.request.get(), 1);
        assert_ne!(fresh.id, retired.id);
        // Retired reply arrives first, while a new wire-session-one request is
        // outstanding. Retargeting would fail the fixture's exact reply check.
        assert!(session.reply(&retired, "stale-after-connect"));
        assert!(session.reply(&fresh, "first"));
        session.accept_zero();
        let expected = b"FIRST\n".repeat(reset + 1);
        while session.stdout() != expected {
            session.run(5_000_000);
        }
        assert!(session.calls.iter().any(|call| call == &retired));
        retired = fresh;
    }
}

pub fn pending_snapshot<M, F>(machine: M, restore: F)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
    F: Fn(&M::Snapshot) -> M,
{
    fn finish<M: Machine<Context = ()>>(
        session: &mut Session<M>,
        published: SimulationControlId,
    ) -> Vec<pv::Output>
    where
        M::Error: Debug,
    {
        let second = session.until_call("second");
        assert!(
            session.calls.iter().all(|call| call.id != published),
            "snapshot re-emitted an already published call"
        );
        session.reply(&second, "second");
        session.accept_zero();
        session.until_marker(b"SECOND\n");
        assert!(session.stdout().starts_with(b"FIRST\nSECOND\n"));
        session
            .outputs
            .iter()
            .filter(|output| !matches!(output.payload, pv::Response::BootConsole { .. }))
            .cloned()
            .collect()
    }
    let mut original = Session::new(machine);
    let retired = original.until_call("first");
    original.reboot(false);
    let first = original.until_call("first");
    assert!(
        first.id.session.get() > 1,
        "capture public session differs from fresh wire session one"
    );
    original.reply(&retired, "stale-before-capture");
    original.reply(&first, "first");
    original.accept_zero();
    let snapshot = original.machine.snapshot().unwrap();
    let now = original.now.now();
    let rng = original.rng.clone();
    let boot = original.boot;
    let deadline = original.deadline;
    original.outputs.clear();
    original.calls.clear();
    let expected = finish(&mut original, first.id);
    drop(original);
    for _ in 0..2 {
        let mut restored = Session::continued(restore(&snapshot), now, rng.clone(), boot, deadline);
        assert_eq!(
            finish(&mut restored, first.id),
            expected,
            "pending reply continuation must deliver exactly once"
        );
    }
}

pub fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
