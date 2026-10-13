//! Native accepted-command snapshot continuation, separate from host codec tests.
use crate as machine_crate;
#[path = "../../tests/support/prepared_application.rs"]
mod prepared;
use super::{Machine, RunContext, StopReason, Timestamp};
use crate::{
    clock::{Clock, SimulationClock},
    pv::{self, ApplicationCommand as Command, Response},
    rng::SeededRng,
};
use std::{
    collections::VecDeque,
    fmt::Debug,
    fs,
    num::NonZeroU32,
    path::PathBuf,
    time::{Duration, Instant},
};

struct Session<M> {
    machine: M,
    clock: SimulationClock,
    rng: SeededRng,
    inbox: VecDeque<pv::Input>,
    events: Vec<pv::Output>,
    exited: bool,
    started: Instant,
}
impl<M: Machine<Context = ()>> Session<M>
where
    M::Error: Debug,
{
    fn new(machine: M, now: u64, rng: SeededRng) -> Self {
        Self {
            machine,
            clock: SimulationClock::new(now),
            rng,
            inbox: VecDeque::new(),
            events: Vec::new(),
            exited: false,
            started: Instant::now(),
        }
    }
    fn run(&mut self, budget: u64) {
        assert!(!self.exited);
        assert!(
            self.started.elapsed() < Duration::from_secs(120),
            "snapshot continuation timed out"
        );
        let now = Timestamp::from_nanos(self.clock.now());
        let upto = Timestamp::from_nanos(now.as_nanos() + budget);
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

fn check<M, F>(machine: M, restore: F)
where
    M: Machine<Context = ()>,
    M::Error: Debug,
    F: Fn(&M::Snapshot) -> M,
{
    let mut original = Session::new(machine, 0, SeededRng::new(7));
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
    let expected = original.finish();
    drop(original);
    let mut branch = Session::new(restore(&snapshot), now, rng.clone());
    assert_eq!(branch.finish(), expected);
    drop(branch);
    let mut second = Session::new(restore(&snapshot), now, rng);
    assert_eq!(second.finish(), expected);
    let stopped_app = second.machine.snapshot().unwrap();
    let now = second.clock.now();
    let rng = second.rng.clone();
    second.events.clear();
    let expected = second.restart_and_shutdown();
    drop(second);
    let mut restored = Session::new(restore(&stopped_app), now, rng);
    restored.run(0);
    assert!(
        restored.events.is_empty(),
        "snapshot must not replay old outcome"
    );
    assert_eq!(restored.restart_and_shutdown(), expected);
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}

#[test]
#[ignore = "requires patched QEMU and static guest service initramfs"]
fn tcg_accepted_application_command_snapshot_continues_exactly_once() {
    use super::tcg::TcgMachine;
    let executable = input("SEEDBOX_TCG_QEMU");
    let bootstrap = super::prepared_fixture::bootstrap("TCG");
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let machine = TcgMachine::new(
        &executable,
        fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap(),
        &bootstrap,
        definition,
    )
    .unwrap();
    check(machine, |snapshot| {
        TcgMachine::from_snapshot(&executable, snapshot).unwrap()
    });
}
// Native PV architectural snapshots: macOS/HVF and x86-64 Linux/KVM.
#[cfg(any(
    all(target_arch = "aarch64", target_os = "macos"),
    all(target_arch = "x86_64", target_os = "linux")
))]
#[test]
#[ignore = "requires a native PV worker with snapshots, kernel and static guest service initramfs"]
fn pv_accepted_application_command_snapshot_continues_exactly_once() {
    use super::pv::PVMachine;
    let executable = input("SEEDBOX_PV_WORKER");
    let bootstrap = super::prepared_fixture::bootstrap("PV");
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let machine = PVMachine::new(&executable, &bootstrap, definition).unwrap();
    check(machine, |snapshot| {
        PVMachine::from_snapshot(&executable, snapshot).unwrap()
    });
}
