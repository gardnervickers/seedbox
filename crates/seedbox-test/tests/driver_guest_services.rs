#![cfg(unix)]
//! Native public Driver acceptance with ordinary targets and a required oracle.
//! Guest inputs are shared with isolated Machine checks through explicit fixture paths.
#[path = "support/epoch.rs"]
mod epoch;
use seedbox_machine as machine_crate;
#[allow(dead_code)]
#[path = "support/native_driver.rs"]
mod native_driver;
#[path = "support/prepared_application.rs"]
mod prepared;
use native_driver::auto_key;
fn key(text: &str) -> seedbox_driver::driver::InputKey {
    seedbox_driver::driver::InputKey::new(text).unwrap()
}
use seedbox_driver::driver::{
    Action, Driver, Event, EventKind, FaultOrigin, HealTarget, MachineId, MachineResponse,
    RejectionReason, SettlementStatus, StepOutcome, Target,
};
use seedbox_machine::{
    execution::ExecutionConfig,
    machine::{ExitReason, Machine, StopReason, Timestamp},
    pv::{CommandResult, Response, SimulationControlId, Stream},
    rng::RngStream,
};
use seedbox_test_time::{GuestProgress, HangGuard};
use std::{
    fmt::Debug,
    fs,
    path::PathBuf,
    time::{Duration, Instant},
};

const QUANTUM: u64 = 5_000_000;
struct FixtureRng(u8);
impl RngStream for FixtureRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            *byte = self.0;
            self.0 = self.0.wrapping_add(1);
        }
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("{name} is required"))
        .into()
}

struct Session<M: Machine<Context = ()>, T = Instant> {
    driver: Driver<M>,
    ids: Vec<MachineId>,
    events: Vec<MachineResponse>,
    /// Every drained event, including control facts.
    all: Vec<Event>,
    started: T,
}
impl<M, T> Session<M, T>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    fn now(&mut self) -> Timestamp {
        self.driver
            .with_barrier(|b| Ok(b.playback().position.at))
            .unwrap()
    }
    fn drain(&mut self) {
        let drained = self.driver.drain_events();
        self.all.extend(drained.iter().cloned());
        let responses = drained
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Output(output) => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.events.extend(responses);
    }
    /// Final status of the fault admitted for host input `name`.
    fn settled(&self, name: &str) -> Option<SettlementStatus> {
        self.all.iter().find_map(|event| match &event.kind {
            EventKind::FaultSettled { record, status, .. }
                if record.origin == FaultOrigin::Input { key: key(name) } =>
            {
                Some(*status)
            }
            _ => None,
        })
    }
    fn rejected(&self, name: &str) -> Option<RejectionReason> {
        self.all.iter().find_map(|event| match &event.kind {
            EventKind::InputRejected {
                key: found,
                rejection,
            } if *found == key(name) => Some(rejection.reason),
            _ => None,
        })
    }
    /// Schedule inputs at the current time and admit them at the next barrier.
    fn apply(&mut self, inputs: impl IntoIterator<Item = (String, Action)>) {
        let now = self.now();
        self.driver
            .with_barrier(|b| {
                for (name, action) in inputs {
                    b.schedule(now, key(&name), action)?;
                }
                Ok(())
            })
            .unwrap();
        self.driver.step(now).unwrap();
        self.drain();
    }
    fn ack(&self, machine: MachineId, boot: u64, command_id: u32) -> bool {
        self.events.iter().any(|event| event.machine == machine && event.boot == boot &&
            matches!(event.payload, Response::CommandAcknowledged { id, result: CommandResult::Completed } if id.get() == command_id))
    }
    fn ready(&self, machine: MachineId, boot: u64) -> bool {
        self.events.iter().any(|event| {
            event.machine == machine
                && event.boot == boot
                && matches!(event.payload, Response::Ready)
        })
    }
    fn initial_control(&self, machine: MachineId, boot: u64) -> bool {
        self.events.iter().any(|event| {
            event.machine == machine
                && event.boot == boot
                && matches!(&event.payload, Response::SimulationControl { id, payload }
                if id.request.get() == 1 && payload == br#"{"version":1,"id":1,"op":"now"}"#)
        })
    }
    /// The last committed control call of this boot.
    fn control_id(&self, machine: MachineId, boot: u64) -> SimulationControlId {
        self.events
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Response::SimulationControl { id, .. }
                    if event.machine == machine && event.boot == boot =>
                {
                    Some(id)
                }
                _ => None,
            })
            .expect("committed control call")
    }
    fn output(&self, machine: MachineId, boot: u64, stream: Stream) -> Vec<u8> {
        self.events
            .iter()
            .filter_map(|event| match &event.payload {
                Response::ApplicationOutput {
                    stream: actual,
                    bytes,
                } if event.machine == machine && event.boot == boot && *actual == stream => {
                    Some(bytes.as_slice())
                }
                _ => None,
            })
            .flatten()
            .copied()
            .collect()
    }
    fn all_exited(&mut self) -> bool {
        self.driver
            .with_barrier(|b| {
                Ok(self
                    .ids
                    .iter()
                    .all(|&id| matches!(b.stopped(id).unwrap(), StopReason::Exited { .. })))
            })
            .unwrap()
    }
}
// Keep unrelated legacy owners unchanged; only registered RPC/epoch callers
// instantiate the same observation helpers with infrastructure-only timing.
impl<M> Session<M>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    fn pump_until(&mut self, predicate: impl Fn(&Self) -> bool) {
        for _ in 0..20000 {
            if predicate(self) {
                return;
            }
            assert!(
                self.started.elapsed() < Duration::from_secs(180),
                "native Driver deadline: {:?}",
                self.events
            );
            let upto = at(self.now().as_nanos() + QUANTUM);
            self.driver.step(upto).unwrap();
            self.drain();
        }
        panic!(
            "native Driver failed to produce expected responses: {:?}",
            self.all
                .iter()
                .filter(|event| !matches!(&event.kind, EventKind::Output(output)
                    if matches!(output.payload, Response::BootConsole { .. })))
                .collect::<Vec<_>>()
        );
    }
}
impl<M> Session<M, HangGuard>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    fn bounded_step(
        &mut self,
        progress: &mut GuestProgress,
        last: &mut Option<StepOutcome>,
    ) -> StepOutcome {
        self.started.remaining().unwrap();
        let now = self.now();
        let upto = at(progress
            .next(
                Duration::from_nanos(now.as_nanos()),
                Duration::from_nanos(QUANTUM),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{error}: phase={}; last_stop={last:?}; events={:?}",
                    progress.phase(),
                    self.all
                )
            })
            .as_nanos()
            .try_into()
            .unwrap());
        let outcome = self.driver.step(upto).unwrap();
        *last = Some(outcome.clone());
        self.drain();
        outcome
    }
}
/// Host inputs drive real guests through the same control policy as guest
/// calls: machine pause/resume, throttle/heal, every process command, graceful
/// shutdown, crash and reboot. Process commands settle on the guest's own
/// acknowledgment, never on queue acceptance.
fn scenario<M>(
    workers: usize,
    definition: machine_crate::definition::MachineDefinition,
    mut construct: impl FnMut(&machine_crate::definition::MachineDefinition) -> M + Send + 'static,
) -> Vec<Event>
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(7),
        || Ok(()),
        {
            let mut oracle = definition.clone();
            oracle.name = "oracle".into();
            oracle
        },
        {
            let mut first = definition.clone();
            first.name = "alpha".into();
            let mut second = definition;
            second.name = "beta".into();
            vec![first, second]
        },
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    // A fixed separate oracle is held alive while the two ordinary targets
    // exercise crash/reboot/shutdown. Driver owns automatic control dispatch.
    let oracle = driver
        .with_barrier(|b| {
            let oracle = b.machines().next().unwrap().0;
            b.schedule(
                at(0),
                key("oracle-pause"),
                Action::Pause(Target::Machine(oracle)),
            )?;
            Ok(oracle)
        })
        .unwrap();
    let ids: Vec<MachineId> = driver
        .with_barrier(|b| Ok(b.machines().skip(1).map(|(id, _)| id).collect()))
        .unwrap();
    let mut session = Session {
        driver,
        ids: ids.clone(),
        events: Vec::new(),
        all: Vec::new(),
        started: Instant::now(),
    };
    session.pump_until(|s| {
        s.ids.iter().all(|&id| {
            s.ready(id, 0)
                && s.initial_control(id, 0)
                && s.output(id, 0, Stream::Stdout) == b"app\0out\n"
                && s.output(id, 0, Stream::Stderr) == b"\xff\0z"
        })
    });

    // Observe actual Linux timer blocking before pausing. A runnable enum alone
    // would not prove that a configuration change preserves timer deadlines.
    let mut timer_blocked = false;
    for _ in 0..100 {
        timer_blocked = session
            .driver
            .with_barrier(|b| {
                Ok(session.ids.iter().all(|&id| {
                    matches!(
                        b.stopped(id).unwrap(),
                        StopReason::Blocked {
                            deadline: Some(_),
                            ..
                        }
                    )
                }))
            })
            .unwrap();
        if timer_blocked {
            break;
        }
        let upto = at(session.now().as_nanos() + QUANTUM);
        session.driver.step(upto).unwrap();
        session.drain();
    }
    assert!(
        timer_blocked,
        "both Linux guests must expose pending timer deadlines"
    );
    let each = |name: &str, action: &dyn Fn(Target) -> Action| {
        ids.iter()
            .enumerate()
            .map(|(index, &id)| (format!("{name}-{index}"), action(Target::Machine(id))))
            .collect::<Vec<_>>()
    };
    let deadline = Duration::from_secs(5);
    let all_settled = |s: &Session<M>, name: &str| {
        (0..2).all(|index| s.settled(&format!("{name}-{index}")) == Some(SettlementStatus::Applied))
    };

    // Machine pause executes no guest work. Throttle on paused machines
    // preserves their blocked state and deadline; later work uses the price.
    session.apply(each("pause", &Action::Pause));
    assert!(all_settled(&session, "pause"));
    let stopped = session
        .driver
        .with_barrier(|b| {
            session
                .ids
                .iter()
                .map(|&id| b.stopped(id))
                .collect::<Result<Vec<_>, _>>()
        })
        .unwrap();
    assert!(stopped.iter().all(|stop| matches!(
        stop,
        StopReason::Blocked {
            deadline: Some(_),
            ..
        }
    )));
    session.apply(
        ids.iter()
            .zip([1001, 2000])
            .enumerate()
            .map(|(index, (&id, factor))| {
                (
                    format!("throttle-{index}"),
                    Action::Throttle {
                        target: Target::Machine(id),
                        config: ExecutionConfig::new(factor).unwrap(),
                    },
                )
            }),
    );
    assert!(all_settled(&session, "throttle"));
    // Ordinary control policy refuses process commands for a paused machine.
    session.apply(each("frozen-early", &|target| Action::PauseProcess {
        target,
        deadline,
    }));
    assert!((0..2).all(|index| {
        session.rejected(&format!("frozen-early-{index}")) == Some(RejectionReason::InvalidState)
    }));
    session
        .driver
        .with_barrier(|b| {
            for (&id, old) in session.ids.iter().zip(&stopped) {
                assert!(b.paused(id)?);
                assert_eq!(&b.stopped(id)?, old);
            }
            Ok(())
        })
        .unwrap();
    assert!(session.ids.iter().all(|&id| !session.ack(id, 0, 1)));
    session.apply(each("resume", &Action::Resume));
    // Each process command is dispatched with the boot's next command ID and
    // settles on the guest's acknowledgment.
    session.apply(each("freeze", &|target| Action::PauseProcess {
        target,
        deadline,
    }));
    session.pump_until(|s| all_settled(s, "freeze"));
    assert!(session.ids.iter().all(|&id| session.ack(id, 0, 1)));
    session.apply(
        (0..2)
            .map(|index| {
                (
                    format!("heal-{index}"),
                    Action::Heal(HealTarget::Input(key(&format!("throttle-{index}")))),
                )
            })
            .chain(each("thaw", &|target| Action::ResumeProcess {
                target,
                deadline,
            })),
    );
    assert!(all_settled(&session, "heal"));
    session.pump_until(|s| all_settled(s, "thaw"));
    session.apply(each("stop", &|target| Action::Stop { target, deadline }));
    session.pump_until(|s| all_settled(s, "stop"));
    session.apply(each("restart", &|target| Action::Restart {
        target,
        deadline,
    }));
    session.pump_until(|s| {
        all_settled(s, "restart")
            && s.ids
                .iter()
                .all(|&id| s.output(id, 0, Stream::Stdout) == b"app\0out\napp\0out\n")
    });
    assert!(
        session
            .ids
            .iter()
            .all(|&id| (1..=4).all(|command| session.ack(id, 0, command)))
    );
    session.apply(each("shutdown", &Action::Shutdown));
    for _ in 0..20000 {
        if session.all_exited() {
            break;
        }
        assert!(session.started.elapsed() < Duration::from_secs(180));
        let upto = at(session.now().as_nanos() + QUANTUM);
        session.driver.step(upto).unwrap();
        session.drain();
    }
    assert!(session.all_exited());
    assert!(all_settled(&session, "shutdown"));
    // An exited (uncrashed) machine cannot reboot until it is crashed. A Reboot
    // queued with the Crash would be admitted first, so it follows separately.
    let retired = session
        .ids
        .iter()
        .map(|&id| (id, session.control_id(id, 0)))
        .collect::<Vec<_>>();
    session.apply(each("crash", &Action::Crash));
    assert!(all_settled(&session, "crash"));
    session.apply(each("reboot", &Action::Reboot));
    assert!(all_settled(&session, "reboot"));
    session.pump_until(|s| {
        s.ids.iter().all(|&id| {
            s.ready(id, 1)
                && s.initial_control(id, 1)
                && s.output(id, 1, Stream::Stdout) == b"app\0out\n"
                && s.output(id, 1, Stream::Stderr) == b"\xff\0z"
        })
    });
    let fresh = session
        .ids
        .iter()
        .map(|&id| (id, session.control_id(id, 1)))
        .collect::<Vec<_>>();
    for (&(_, old), &(_, new)) in retired.iter().zip(&fresh) {
        assert_eq!(new.session.get(), old.session.get() + 1);
        assert_eq!(new.request.get(), 1);
    }
    // The fresh boot's allocator restarts; the new boot is bound at admission.
    session.apply(each("freeze-again", &|target| Action::PauseProcess {
        target,
        deadline,
    }));
    session.pump_until(|s| all_settled(s, "freeze-again"));
    assert!(session.ids.iter().all(|&id| session.ack(id, 1, 1)));
    session.apply(each("pause-again", &Action::Pause));
    session.apply(each("thaw-paused", &|target| Action::ResumeProcess {
        target,
        deadline,
    }));
    assert!((0..2).all(|index| {
        session.rejected(&format!("thaw-paused-{index}")) == Some(RejectionReason::InvalidState)
    }));
    session.apply(each("resume-again", &Action::Resume));
    session.apply(
        each("thaw-again", &|target| Action::ResumeProcess {
            target,
            deadline,
        })
        .into_iter()
        .chain(each("shutdown-again", &Action::Shutdown)),
    );
    for _ in 0..20000 {
        if session.all_exited() {
            break;
        }
        assert!(
            session.started.elapsed() < Duration::from_secs(180),
            "native Driver shutdown deadline"
        );
        let upto = at(session.now().as_nanos() + QUANTUM);
        session.driver.step(upto).unwrap();
        session.drain();
    }
    assert!(session.all_exited());
    assert!(all_settled(&session, "thaw-again"));
    for &id in &session.ids {
        assert!(
            session.ack(id, 1, 2),
            "shutdown overtook application resume"
        );
        assert_eq!(
            session.output(id, 0, Stream::Stdout),
            b"app\0out\napp\0out\ncleaned\n",
            "commanded Stop terminates without the graceful cleanup"
        );
        assert_eq!(
            session.output(id, 1, Stream::Stdout),
            b"app\0out\ncleaned\n"
        );
        assert_eq!(session.output(id, 1, Stream::Stderr), b"\xff\0z");
    }
    assert!(
        !session
            .events
            .iter()
            .any(|event| matches!(event.payload, Response::ApplicationOutcome { .. }))
    );
    // Each application session emits its one control call; Restart starts a
    // second session in boot zero. Driver replies automatically.
    for &machine in &session.ids {
        for (boot, sessions) in [(0, 2), (1, 1)] {
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| event.machine == machine
                        && event.boot == boot
                        && matches!(event.payload, Response::SimulationControl { .. }))
                    .count(),
                sessions,
            );
        }
    }
    // The same fixed oracle has remained alive throughout target lifecycle
    // changes. Its termination, rather than target shutdown, finishes Driver.
    let now = session.now();
    session
        .driver
        .with_barrier(|b| {
            b.schedule(
                now,
                key("oracle-crash"),
                Action::Crash(Target::Machine(oracle)),
            )
        })
        .unwrap();
    let finished = StepOutcome::Finished {
        at: now,
        reason: ExitReason::Crashed,
    };
    assert_eq!(session.driver.step(now).unwrap(), finished);
    assert_eq!(
        session.driver.step(at(now.as_nanos() + QUANTUM)).unwrap(),
        finished
    );
    session.drain();
    assert!(session.all_exited());
    // Boot serial output is diagnostic; compare the full event stream,
    // including canonical timestamps, fault IDs, keys, boot tags and order.
    session
        .all
        .into_iter()
        .filter(|event| {
            !matches!(&event.kind, EventKind::Output(output)
                if matches!(output.payload, Response::BootConsole { .. }))
        })
        .collect()
}

fn tcg(workers: usize) -> Vec<Event> {
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
    scenario(workers, definition, move |definition| {
        backend.create(definition).unwrap()
    })
}
#[test]
#[ignore = "requires patched QEMU and SEEDBOX_TCG_SERVICE_INITRD static guest fixtures"]
fn tcg_driver_queues_boot_tags_and_worker_count_match() {
    let serial = tcg(1);
    assert_eq!(serial, tcg(2));
    assert_eq!(serial, tcg(4));
}
#[cfg(seedbox_pv)]
fn pv(workers: usize) -> Vec<Event> {
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
    scenario(workers, definition, move |definition| {
        backend.create(definition).unwrap()
    })
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV hypervisor and SEEDBOX_PV_SERVICE_INITRD static guest fixtures"]
fn pv_driver_queues_boot_tags_and_worker_count_match() {
    assert_eq!(pv(1), pv(4));
}

/// A real guest consumes every automatic RPC reply. The peer's Stop/Restart
/// completions come from its supervisor, not from a host acceptance stub.
fn automatic_rpc_scenario<M>(
    workers: usize,
    mut base: machine_crate::definition::MachineDefinition,
    application: Vec<u8>,
    construct: impl FnMut(&machine_crate::definition::MachineDefinition) -> M + Send + 'static,
) -> (Vec<StepOutcome>, Vec<MachineResponse>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    install_application(&mut base, &application);
    let mut oracle_definition = base.clone();
    oracle_definition.name = "oracle".into();
    oracle_definition.application.launch.argv = vec!["/application".into(), "oracle".into()];
    base.name = "peer".into();
    base.application.launch.argv = vec!["/application".into(), "peer".into()];
    automatic_rpc_driver(workers, oracle_definition, base, construct)
}

fn install_application(
    base: &mut machine_crate::definition::MachineDefinition,
    application: &[u8],
) {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(application.len() as u64);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", application)
        .unwrap();
    base.application.filesystem =
        machine_crate::definition::FilesystemArchive::new(tar.into_inner().unwrap()).unwrap();
}

fn automatic_rpc_driver<M>(
    workers: usize,
    oracle_definition: machine_crate::definition::MachineDefinition,
    base: machine_crate::definition::MachineDefinition,
    mut construct: impl FnMut(&machine_crate::definition::MachineDefinition) -> M + Send + 'static,
) -> (Vec<StepOutcome>, Vec<MachineResponse>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(9),
        || Ok(()),
        oracle_definition,
        vec![base],
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    let (oracle, peer) = driver
        .with_barrier(|b| {
            let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
            b.schedule(at(0), auto_key(), Action::Pause(Target::Machine(ids[0])))?;
            Ok((ids[0], ids[1]))
        })
        .unwrap();
    let mut session = Session {
        driver,
        ids: vec![peer],
        events: Vec::new(),
        all: Vec::new(),

        started: HangGuard::new(
            "automatic native RPC infrastructure",
            Duration::from_secs(180),
        ),
    };
    // Peer role only prints then pauses. Allow 10 simulated seconds for
    // kernel/init/Ready. At most Pause admission, Ready and output may yield
    // a same-instant Driver boundary before the next positive step.
    let mut progress =
        GuestProgress::new("RPC peer boot", Duration::ZERO, Duration::from_secs(10), 3).unwrap();
    let mut last = None;
    while !(session.ready(peer, 0) && session.output(peer, 0, Stream::Stdout) == b"PEER_RUNNING\n")
    {
        session.bounded_step(&mut progress, &mut last);
    }
    let now = session.now();
    session
        .driver
        .with_barrier(|b| b.schedule(now, auto_key(), Action::Resume(Target::Machine(oracle))))
        .unwrap();
    // driver-simulation-control.c oracle: eight RPCs, no sleep/compute loop.
    // Oracle was held unbooted: reserve 10s kernel/init plus 1s protocol work
    // per RPC. The finite same-instant transitions are eight call admissions,
    // eight replies, six fault settlements, two app acks and two Ready records.
    let start = Duration::from_nanos(now.as_nanos());
    let mut progress = GuestProgress::new(
        "automatic RPC oracle boot/eight calls",
        start,
        start + Duration::from_secs(18),
        26,
    )
    .unwrap();
    let mut outcomes = Vec::new();
    let mut saw_paused = false;
    let mut saw_resumed = false;
    let mut saw_stopped = false;
    let finished = loop {
        let outcome = session.bounded_step(&mut progress, &mut last);
        outcomes.push(outcome.clone());
        let bytes = session.output(oracle, 0, Stream::Stdout);
        let output = std::str::from_utf8(&bytes).unwrap();
        if !saw_paused && output.contains("\nPAUSED\n") {
            assert!(session.driver.with_barrier(|b| b.paused(peer)).unwrap());
            saw_paused = true;
        }
        if !saw_resumed && output.contains("\nRESUMED\n") {
            assert!(!session.driver.with_barrier(|b| b.paused(peer)).unwrap());
            saw_resumed = true;
        }
        if !saw_stopped && output.contains("\nSTOPPED\n") {
            assert_eq!(session.output(peer, 0, Stream::Stdout), b"PEER_RUNNING\n");
            saw_stopped = true;
        }
        if let StepOutcome::Finished { at, reason } = outcome {
            assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
            break at;
        }
        assert!(matches!(outcome, StepOutcome::BarrierReached { .. }));
    };
    assert!(saw_paused && saw_resumed && saw_stopped);
    assert_eq!(
        session.output(peer, 0, Stream::Stdout),
        b"PEER_RUNNING\nPEER_RUNNING\n"
    );
    assert!(session.output(peer, 0, Stream::Stderr).is_empty());
    assert!(session.output(oracle, 0, Stream::Stderr).is_empty());
    assert!(session.ready(oracle, 0));
    session
        .driver
        .with_barrier(|b| {
            assert!(!matches!(b.stopped(peer)?, StopReason::Exited { .. }));
            assert!(!b.paused(peer)?);
            // Application exit finishes Driver while the oracle guest survives.
            assert!(!matches!(b.stopped(oracle)?, StopReason::Exited { .. }));
            Ok(())
        })
        .unwrap();
    let calls: Vec<_> = session
        .events
        .iter()
        .filter(|event| {
            event.machine == oracle && matches!(event.payload, Response::SimulationControl { .. })
        })
        .collect();
    assert_eq!(calls.len(), 8);
    let output = session.output(oracle, 0, Stream::Stdout);
    let output = std::str::from_utf8(&output).unwrap();
    let replies: Vec<serde_json::Value> = output
        .lines()
        .filter_map(|line| line.strip_prefix("RPC_REPLY "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect();
    assert_eq!(replies.len(), 8, "{output}");
    let expected_operations = [
        "now", "pause", "throttle", "heal", "resume", "stop", "restart", "now",
    ];
    for (index, ((call, reply), operation)) in calls
        .iter()
        .zip(&replies)
        .zip(expected_operations)
        .enumerate()
    {
        let Response::SimulationControl { id, payload } = &call.payload else {
            unreachable!()
        };
        assert_eq!(id.session.get(), 1);
        assert_eq!(id.request.get() as usize, index + 1);
        let request: serde_json::Value = serde_json::from_slice(payload).unwrap();
        assert_eq!(request["version"], 1);
        assert_eq!(request["id"], index + 1);
        assert_eq!(request["op"], operation);
        assert_eq!(reply["version"], 1);
        assert_eq!(reply["id"], index + 1);
        assert_eq!(reply["ok"], true);
        assert!(reply.get("error").is_none());
        assert_eq!(reply["requested_at_ns"], call.at.as_nanos());
        let admitted = reply["admitted_at_ns"].as_u64().unwrap();
        let replied = reply["replied_at_ns"].as_u64().unwrap();
        assert!(
            call.at.as_nanos() <= admitted && admitted <= replied && replied <= finished.as_nanos()
        );
        if operation == "now" {
            assert_eq!(reply["result"]["now_ns"], admitted);
            assert_eq!(admitted, replied);
            if let Some(effect) = reply["effect_at_ns"].as_u64() {
                assert_eq!(effect, admitted);
            } else {
                assert!(reply["effect_at_ns"].is_null());
            }
        } else if !matches!(operation, "stop" | "restart") {
            assert_eq!(reply["effect_at_ns"], admitted);
            assert_eq!(admitted, replied);
        }
    }
    let throttle_request: serde_json::Value = match &calls[2].payload {
        Response::SimulationControl { payload, .. } => serde_json::from_slice(payload).unwrap(),
        _ => unreachable!(),
    };
    assert_eq!(throttle_request["factor_permille"], 2000);
    let heal_request: serde_json::Value = match &calls[3].payload {
        Response::SimulationControl { payload, .. } => serde_json::from_slice(payload).unwrap(),
        _ => unreachable!(),
    };
    assert!(replies[2]["result"]["handle"].as_str().is_some());
    assert_eq!(heal_request["handle"], replies[2]["result"]["handle"]);
    let acknowledgments: Vec<_> = session
        .events
        .iter()
        .filter_map(|event| match event.payload {
            Response::CommandAcknowledged { id, result } if event.machine == peer => {
                assert_eq!(result, CommandResult::Completed);
                Some((id, event.at))
            }
            _ => None,
        })
        .collect();
    assert_eq!(acknowledgments.len(), 2);
    assert_ne!(acknowledgments[0].0, acknowledgments[1].0);
    for ((_, acknowledged), reply) in acknowledgments.iter().zip(&replies[5..7]) {
        assert_eq!(reply["effect_at_ns"], acknowledged.as_nanos());
        assert!(reply["admitted_at_ns"].as_u64().unwrap() <= acknowledged.as_nanos());
        assert!(acknowledged.as_nanos() <= reply["replied_at_ns"].as_u64().unwrap());
    }
    assert_eq!(
        session
            .events
            .iter()
            .filter(|event| event.machine == oracle
                && matches!(event.payload, Response::ApplicationOutcome { code: 0 }))
            .count(),
        1
    );
    assert!(output.ends_with("DONE\n"), "{output}");
    assert_eq!(
        session
            .driver
            .step(at(finished.as_nanos() + QUANTUM))
            .unwrap(),
        StepOutcome::Finished {
            at: finished,
            reason: ExitReason::ProgramExit { code: 0 }
        }
    );
    // Include canonical barriers and every typed response, including boot bytes.
    // A synthetic reply stub cannot satisfy the guest's eight receive calls.
    (outcomes, session.events)
}

#[test]
#[ignore = "requires native TCG owned kernel/helper and driver-control application"]
fn tcg_driver_guest_receives_automatic_replies_and_real_peer_effects() {
    let run = |workers| {
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
        automatic_rpc_scenario(
            workers,
            definition,
            fs::read(input("SEEDBOX_TCG_DRIVER_CONTROL_APPLICATION")).unwrap(),
            move |definition| backend.create(definition).unwrap(),
        )
    };
    let serial = run(1);
    assert_eq!(serial, run(2));
    assert_eq!(serial, run(4));
}

#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV kernel/helper and driver-control application"]
fn pv_driver_guest_receives_automatic_replies_and_real_peer_effects() {
    let run = |workers| {
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
        automatic_rpc_scenario(
            workers,
            definition,
            fs::read(input("SEEDBOX_PV_DRIVER_CONTROL_APPLICATION")).unwrap(),
            move |definition| backend.create(definition).unwrap(),
        )
    };
    assert_eq!(run(1), run(2));
}

#[test]
#[ignore = "requires native TCG owned kernel/helper and driver-control application"]
fn tcg_native_repeat_run_has_identical_trace() {
    for workers in [1, 2, 4] {
        assert_eq!(tcg(workers), tcg(workers), "workers={workers}");
    }
}

/// Real guest spawn with an oracle-only initial set and a separately declared peer.
fn spawn_scenario<M>(
    workers: usize,
    mut definition: machine_crate::definition::MachineDefinition,
    application: Vec<u8>,
    factory: impl FnMut(&machine_crate::definition::MachineDefinition) -> Result<M, M::Error>
    + Send
    + 'static,
) -> (Vec<StepOutcome>, Vec<seedbox_driver::driver::Event>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    use seedbox_driver::driver::{Cause, FaultAction, SettlementStatus};
    install_application(&mut definition, &application);
    let mut oracle = definition.clone();
    oracle.name = "oracle".into();
    oracle.application.launch.argv = vec!["/application".into(), "spawn-oracle".into()];
    definition.name = "peer".into();
    definition.application.launch.argv = vec!["/application".into(), "peer".into()];
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(9),
        || Ok(()),
        oracle,
        vec![],
        vec![definition],
        seedbox_driver::driver::NetworkConfig::default(),
        factory,
    )
    .unwrap();
    let initial = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(initial.len(), 1);
    let mut events = driver.drain_events();
    let mut outcomes = Vec::new();
    let started = Instant::now();
    let mut now = at(0);
    for _ in 0..20000 {
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "native spawn deadline"
        );
        let outcome = driver.step(at(now.as_nanos() + QUANTUM)).unwrap();
        events.extend(driver.drain_events());
        outcomes.push(outcome.clone());
        match outcome {
            StepOutcome::BarrierReached { at } => now = at,
            StepOutcome::Finished {
                reason: ExitReason::ProgramExit { code: 0 },
                ..
            } => break,
            _ => panic!("unexpected native spawn outcome: {outcome:?}"),
        }
    }
    assert!(matches!(
        outcomes.last(),
        Some(StepOutcome::Finished {
            reason: ExitReason::ProgramExit { code: 0 },
            ..
        })
    ));
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect::<Vec<_>>()))
        .unwrap();
    assert_eq!(ids.len(), 2);
    let output = |id| {
        events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Output(MachineResponse {
                    machine,
                    payload:
                        Response::ApplicationOutput {
                            stream: Stream::Stdout,
                            bytes,
                        },
                    ..
                }) if *machine == id => Some(bytes.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect::<Vec<_>>()
    };
    assert_eq!(output(ids[1]), b"PEER_RUNNING\n");
    let oracle_bytes = output(ids[0]);
    let text = std::str::from_utf8(&oracle_bytes).unwrap();
    assert!(text.contains("\nSPAWN_APPLIED\n"), "{text}");
    assert!(text.ends_with("DONE\n"), "{text}");
    let replies: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|line| line.strip_prefix("RPC_REPLY "))
        .map(|json| serde_json::from_str(json).unwrap())
        .collect();
    assert_eq!(replies.len(), 2);
    assert_eq!(replies[0]["ok"], true);
    assert_eq!(replies[0]["result"], serde_json::json!({"machine":"peer"}));
    let added = events.iter().position(|e| matches!(e.kind, EventKind::MachineAdded { machine, cause: Cause::Fault(_), .. } if machine == ids[1])).unwrap();
    let settled = events.iter().position(|e| matches!(&e.kind, EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.target == seedbox_driver::driver::FaultTarget::Machine { machine: ids[1], boot: 0 } && record.action == FaultAction::Spawn)).unwrap();
    let reply = events
        .iter()
        .position(
            |e| matches!(e.kind, EventKind::ReplyAdmitted { id, .. } if id.request.get() == 1),
        )
        .unwrap();
    let first_output = events
        .iter()
        .position(|e| matches!(&e.kind, EventKind::Output(o) if o.machine == ids[1]))
        .unwrap();
    assert!(added < settled && settled < reply && reply < first_output);
    driver
        .with_barrier(|b| {
            b.snapshot(ids[1])?;
            Ok(())
        })
        .unwrap();
    (outcomes, events)
}
#[test]
#[ignore = "requires native TCG owned kernel/helper and driver-control application"]
fn tcg_driver_oracle_spawns_declared_peer_workers_match() {
    let run = |workers| {
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
        spawn_scenario(
            workers,
            definition,
            fs::read(input("SEEDBOX_TCG_DRIVER_CONTROL_APPLICATION")).unwrap(),
            move |d| backend.create(d),
        )
    };
    assert_eq!(run(1), run(4));
}
#[cfg(seedbox_pv)]
#[test]
#[ignore = "requires native PV kernel/helper and driver-control application"]
fn pv_driver_oracle_spawns_declared_peer_workers_match() {
    let run = |workers| {
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
        spawn_scenario(
            workers,
            definition,
            fs::read(input("SEEDBOX_PV_DRIVER_CONTROL_APPLICATION")).unwrap(),
            move |d| backend.create(d),
        )
    };
    assert_eq!(run(1), run(4));
}

fn boot_epoch(workers: usize) -> Vec<MachineResponse> {
    let mut oracle = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    install_application(
        &mut oracle,
        &fs::read(input("SEEDBOX_TCG_DRIVER_CONTROL_APPLICATION")).unwrap(),
    );
    oracle.application.launch.argv = vec!["/application".into(), "epoch".into()];
    let mut backend = seedbox_machine_tcg::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
        epoch::TEST_EPOCH_SECONDS,
    )
    .unwrap();
    let driver = Driver::new(
        at(0),
        Duration::from_nanos(QUANTUM),
        workers,
        FixtureRng(9),
        || Ok(()),
        oracle,
        vec![],
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| backend.create(definition),
    )
    .unwrap();
    let mut session = Session {
        driver,
        ids: vec![],
        events: vec![],
        all: vec![],

        started: HangGuard::new("native boot-epoch infrastructure", Duration::from_secs(180)),
    };
    // Epoch role makes nine clock reads, prints CLOCKS and exits: no sleeps
    // or compute loop. 10s kernel/init allowance plus 1s probe/exit protocol.
    // Ready, output and outcome are the only three same-instant transitions.
    let mut progress = GuestProgress::new(
        "boot epoch nine-read probe",
        Duration::ZERO,
        Duration::from_secs(11),
        3,
    )
    .unwrap();
    let mut last = None;
    loop {
        let result = session.bounded_step(&mut progress, &mut last);
        if let StepOutcome::Finished { reason, .. } = result {
            assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
            break;
        }
    }
    session.events
}

#[test]
#[ignore = "requires native TCG owned kernel/helper and driver-control application"]
fn tcg_native_boot_realtime_is_epoch_plus_machine_counter() {
    let serial = boot_epoch(1);
    assert_eq!(serial, boot_epoch(2));
    assert_eq!(serial, boot_epoch(4));
    let output: Vec<_> = serial
        .iter()
        .filter_map(|event| match &event.payload {
            Response::ApplicationOutput {
                stream: Stream::Stdout,
                bytes,
            } => Some(bytes.as_slice()),
            _ => None,
        })
        .flatten()
        .copied()
        .collect();
    let output = std::str::from_utf8(&output).unwrap();
    let clocks: Vec<u64> = output
        .strip_prefix("CLOCKS ")
        .unwrap()
        .trim()
        .split(' ')
        .map(|n| n.parse().unwrap())
        .collect();
    check_boot_epoch_clocks(&clocks);
}

/// The clocks have distinct origins. Bracket REALTIME and RAW samples with
/// MONOTONIC reads and compare signed offset intervals, not absolute readings.
fn check_boot_epoch_clocks(clocks: &[u64]) {
    let [m0, rt0, m1, raw0, m2, rt1, m3, raw1, m4]: [u64; 9] =
        clocks.try_into().expect("complete CLOCKS probe");
    assert!(m0 <= m1 && m1 <= m2 && m2 <= m3 && m3 <= m4);
    assert!(rt0 <= rt1, "realtime regressed within an unstepped boot");
    assert!(raw0 <= raw1, "raw clock regressed");
    // Bound sampling/preemption, not clock conversion error. This is the same
    // 1ms sampling allowance used by the shared native clock-fault scenarios.
    assert!(m4 - m0 < 1_000_000, "clock probe exceeded 1ms");
    let lower = (i128::from(rt0) - i128::from(m1)).max(i128::from(rt1) - i128::from(m3));
    let upper = (i128::from(rt0) - i128::from(m0)).min(i128::from(rt1) - i128::from(m2));
    assert!(lower <= upper, "RT/MONO offset changed across the probe");
    // RAW may lag MONOTONIC, so subtraction must be signed. Its offset must
    // nevertheless stay consistent: a frozen or wrongly paced RAW is invalid.
    let lower = (i128::from(raw0) - i128::from(m2)).max(i128::from(raw1) - i128::from(m4));
    let upper = (i128::from(raw0) - i128::from(m1)).min(i128::from(raw1) - i128::from(m3));
    assert!(lower <= upper, "RAW/MONO offset changed across the probe");
    let epoch = u64::from(epoch::TEST_EPOCH_SECONDS) * 1_000_000_000;
    for (realtime, before) in [(rt0, m0), (rt1, m2)] {
        let since_epoch = realtime - epoch;
        assert!(since_epoch > before && since_epoch - before < 1_000_000_000);
    }
    // No sample-quantization assertion: the former 10MHz RISC-V CSR does
    // not describe x86 POSIX clocks. Even a 1GHz TSC can back a kernel that
    // still reports 1ms POSIX resolution before high-resolution activation.
}

#[test]
fn epoch_probe_checks_each_clock_domain_and_stable_offset() {
    let epoch = u64::from(epoch::TEST_EPOCH_SECONDS) * 1_000_000_000;
    // Real x86 origins: RAW starts about55ms after MONOTONIC. Non-/100ns
    // readings are valid; the old cross-domain ordering rejected this case.
    let probe = [
        1_170_321_570,
        epoch + 1_961_609_377,
        1_170_321_620,
        1_115_079_216,
        1_170_321_700,
        epoch + 1_961_609_507,
        1_170_321_760,
        1_115_079_400,
        1_170_321_810,
    ];
    assert!(probe[3] < probe[0]);
    check_boot_epoch_clocks(&probe);
    for (label, index, bad) in [
        ("MONO regression", 2, probe[0] - 1),
        ("RT regression", 5, probe[1] - 1),
        ("RAW regression", 7, probe[3] - 1),
        ("RT offset jump", 5, probe[5] + 1_000),
        ("frozen RAW", 7, probe[3]),
        ("RAW offset jump", 7, probe[7] + 1_000),
        ("slow RAW", 7, probe[3] + 1),
    ] {
        let mut invalid = probe;
        invalid[index] = bad;
        assert!(
            std::panic::catch_unwind(|| check_boot_epoch_clocks(&invalid)).is_err(),
            "invalid clock probe accepted: {label}"
        );
    }
}
