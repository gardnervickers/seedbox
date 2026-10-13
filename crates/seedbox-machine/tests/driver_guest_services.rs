#![cfg(unix)]
//! Native public Driver acceptance with ordinary targets and a required oracle.
//! Guest inputs are shared with isolated Machine checks through explicit fixture paths.
use seedbox_machine as machine_crate;
#[path = "support/prepared_application.rs"]
mod prepared;
use seedbox_machine::{
    driver::{Action, Driver, EventKind, MachineId, MachineResponse, StepOutcome},
    execution::ExecutionConfig,
    machine::{ExitReason, Machine, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Request, Response, SimulationControlId, Stream},
    rng::RngStream,
};
use std::{
    fmt::Debug,
    fs,
    num::NonZeroU32,
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
fn app(id: u32, command: ApplicationCommand) -> Request {
    Request::Application {
        id: NonZeroU32::new(id).unwrap(),
        command,
    }
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("{name} is required"))
        .into()
}

struct Session<M: Machine<Context = ()>> {
    driver: Driver<M>,
    ids: Vec<MachineId>,
    events: Vec<MachineResponse>,
    started: Instant,
}
impl<M> Session<M>
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
        let responses = self
            .driver
            .drain_events()
            .into_iter()
            .filter_map(|event| match event.kind {
                EventKind::Output(output) => Some(output),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.events.extend(responses);
    }
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
            self.events
        );
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
                if u64::from(id.session.get()) == boot + 1 && id.request.get() == 1 &&
                    payload == br#"{"version":1,"id":1,"op":"now"}"#)
        })
    }
    fn control_id(&self, machine: MachineId, boot: u64) -> SimulationControlId {
        self.events
            .iter()
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
fn scenario<M>(
    workers: usize,
    definition: machine_crate::definition::MachineDefinition,
    mut construct: impl FnMut(&machine_crate::definition::MachineDefinition) -> M + Send + 'static,
) -> Vec<MachineResponse>
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
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    // A fixed separate oracle is held alive while the two ordinary targets
    // exercise crash/reboot/shutdown. Driver owns automatic control dispatch.
    let oracle = driver
        .with_barrier(|b| {
            let oracle = b.machines().next().unwrap().0;
            b.schedule(at(0), Action::Pause(oracle))?;
            Ok(oracle)
        })
        .unwrap();
    let ids = driver
        .with_barrier(|b| Ok(b.machines().skip(1).map(|(id, _)| id).collect()))
        .unwrap();
    let mut session = Session {
        driver,
        ids,
        events: Vec::new(),
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

    // Both identities may reuse the same command ID. Machine pause accepts
    // ordered input at a barrier but does not execute the guest to acknowledge it.
    let now = session.now();
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                b.schedule(now, Action::Pause(id))?;
            }
            Ok(())
        })
        .unwrap();
    session.driver.step(now).unwrap();
    session.drain();
    assert!(session.ids.iter().all(|&id| !session.ack(id, 0, 7)));
    // Configuration admission on paused machines executes no guest work and
    // preserves their blocked state/deadline. Later work uses the new price.
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
    session
        .driver
        .with_barrier(|b| {
            b.schedule(
                now,
                Action::SetExecution {
                    machine: session.ids[0],
                    config: ExecutionConfig::new(1001).unwrap(),
                },
            )?;
            b.schedule(
                now,
                Action::SetExecution {
                    machine: session.ids[1],
                    config: ExecutionConfig::new(2000).unwrap(),
                },
            )
        })
        .unwrap();
    session.driver.step(now).unwrap();
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
    // Ordinary input may make the device runnable even with no CPU work.
    // Offer it only after independently proving SetExecution preserves timers.
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                b.schedule_request(now, id, app(7, ApplicationCommand::Pause))?;
            }
            Ok(())
        })
        .unwrap();
    session.driver.step(now).unwrap();
    session.drain();
    assert!(session.ids.iter().all(|&id| !session.ack(id, 0, 7)));
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                assert!(b.paused(id)?);
                b.schedule(now, Action::Resume(id))?;
            }
            Ok(())
        })
        .unwrap();
    session.pump_until(|s| s.ids.iter().all(|&id| s.ack(id, 0, 7)));

    // Leave old-boot acknowledgments in Driver's response queue while each
    // Reboot is admitted. Draining afterward must retain the emitting boot tag.
    let now = session.now();
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                b.schedule_request(now, id, app(8, ApplicationCommand::Stop))?;
                b.schedule_request(now, id, Request::Shutdown)?;
            }
            Ok(())
        })
        .unwrap();
    for _ in 0..20000 {
        if session.all_exited() {
            break;
        }
        assert!(session.started.elapsed() < Duration::from_secs(180));
        let upto = at(session.now().as_nanos() + QUANTUM);
        session.driver.step(upto).unwrap();
    }
    assert!(session.all_exited());
    let reset = session.now();
    let late = at(reset.as_nanos() + 1);
    let retired = session
        .ids
        .iter()
        .map(|&id| (id, session.control_id(id, 0)))
        .collect::<Vec<_>>();
    session
        .driver
        .with_barrier(|b| {
            for &(id, old) in &retired {
                // Schedule before admitting Reboot, but deliver to the fresh
                // machine before it has created its first control connection.
                b.schedule_request(
                    late,
                    id,
                    Request::SimulationReply {
                        id: old,
                        payload: br#"{"version":1,"id":1,"stale":true}"#.to_vec(),
                    },
                )?;
                b.schedule_request(reset, id, Request::Reboot)?;
            }
            Ok(())
        })
        .unwrap();
    session.driver.step(reset).unwrap();
    session.drain();
    assert!(
        session.ids.iter().all(|&id| session.ack(id, 0, 8)),
        "old boot responses lost their association: {:?}",
        session
            .events
            .iter()
            .filter(|event| !matches!(event.payload, Response::BootConsole { .. }))
            .collect::<Vec<_>>()
    );
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
    for (&(machine, old), &(_, new)) in retired.iter().zip(&fresh) {
        assert_eq!(new.session.get(), old.session.get() + 1);
        assert_eq!(new.request.get(), 1);
        assert!(session.events.iter().any(|event| event.machine == machine
            && event.boot == 1
            && event.at > late
            && matches!(event.payload, Response::SimulationControl { id, .. } if id == new)));
    }
    let now = session.now();
    session
        .driver
        .with_barrier(|b| {
            for &(id, old) in &retired {
                // Adversarial retired replies travel through the ordinary inbox.
                // Driver already answered the fresh call; this old destination
                // must be ignored without disturbing subsequent guest commands.
                b.schedule_request(
                    now,
                    id,
                    Request::SimulationReply {
                        id: old,
                        payload: br#"{"version":1,"id":1,"stale":true}"#.to_vec(),
                    },
                )?;
                b.schedule_request(now, id, app(7, ApplicationCommand::Pause))?;
            }
            Ok(())
        })
        .unwrap();
    session.pump_until(|s| s.ids.iter().all(|&id| s.ack(id, 1, 7)));

    // Resume the application before graceful shutdown in the same FIFO while
    // machine execution is paused. No acknowledgment can appear until resume.
    let now = session.now();
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                b.schedule(now, Action::Pause(id))?;
                b.schedule_request(now, id, app(8, ApplicationCommand::Resume))?;
                b.schedule_request(now, id, Request::Shutdown)?;
            }
            Ok(())
        })
        .unwrap();
    session.driver.step(now).unwrap();
    session.drain();
    assert!(session.ids.iter().all(|&id| !session.ack(id, 1, 8)));
    session
        .driver
        .with_barrier(|b| {
            for &id in &session.ids {
                assert!(b.paused(id)?);
                b.schedule(now, Action::Resume(id))?;
            }
            Ok(())
        })
        .unwrap();
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
    for &id in &session.ids {
        assert!(
            session.ack(id, 1, 8),
            "shutdown overtook application resume"
        );
        assert_eq!(
            session.output(id, 1, Stream::Stdout),
            b"app\0out\ncleaned\n"
        );
        assert_eq!(session.output(id, 1, Stream::Stderr), b"\xff\0z");
    }
    assert!(!session
        .events
        .iter()
        .any(|event| matches!(event.payload, Response::ApplicationOutcome { .. })));
    assert!(session.driver.drain_events().is_empty());
    // Each ordinary machine emits one control call from each boot. Driver
    // interprets and replies automatically; emitted calls stay drainable.
    for &machine in &session.ids {
        for boot in [0, 1] {
            assert!(session.initial_control(machine, boot));
            assert_eq!(
                session
                    .events
                    .iter()
                    .filter(|event| event.machine == machine
                        && event.boot == boot
                        && matches!(event.payload, Response::SimulationControl { .. }))
                    .count(),
                1,
                "control publication must occur exactly once per machine and boot"
            );
        }
    }
    // The same fixed oracle has remained alive throughout target lifecycle
    // changes. Its termination, rather than target shutdown, finishes Driver.
    let now = session.now();
    session
        .driver
        .with_barrier(|b| b.schedule(now, Action::Crash(oracle)))
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
    assert!(session.all_exited());
    // Boot serial output is diagnostic; compare the full typed application and
    // control trace, including canonical timestamps, IDs, boot tags and order.
    session
        .events
        .into_iter()
        .filter(|event| !matches!(event.payload, Response::BootConsole { .. }))
        .collect()
}

fn tcg(workers: usize) -> Vec<MachineResponse> {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_TCG_SERVICE_INITRD")).unwrap(),
        256 << 20,
    );
    let mut backend = machine_crate::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
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
fn pv(workers: usize) -> Vec<MachineResponse> {
    let definition = prepared::definition(
        fs::read(input("SEEDBOX_PV_SERVICE_INITRD")).unwrap(),
        128 << 20,
    );
    let mut backend = machine_crate::machine::pv::Factory::new(
        &input("SEEDBOX_PV_WORKER"),
        &input("SEEDBOX_PV_ASSETS"),
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
    assert_eq!(pv(1), pv(2));
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
        move |definition| Ok(construct(definition)),
    )
    .unwrap();
    let (oracle, peer) = driver
        .with_barrier(|b| {
            let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
            b.schedule(at(0), Action::Pause(ids[0]))?;
            Ok((ids[0], ids[1]))
        })
        .unwrap();
    let mut session = Session {
        driver,
        ids: vec![peer],
        events: Vec::new(),
        started: Instant::now(),
    };
    session
        .pump_until(|s| s.ready(peer, 0) && s.output(peer, 0, Stream::Stdout) == b"PEER_RUNNING\n");
    let now = session.now();
    session
        .driver
        .with_barrier(|b| b.schedule(now, Action::Resume(oracle)))
        .unwrap();
    let mut outcomes = Vec::new();
    let mut saw_paused = false;
    let mut saw_resumed = false;
    let mut saw_stopped = false;
    let finished = loop {
        assert!(
            session.started.elapsed() < Duration::from_secs(180),
            "automatic native RPC timed out: {:?}",
            session.events
        );
        let upto = at(session.now().as_nanos() + QUANTUM);
        let outcome = session.driver.step(upto).unwrap();
        outcomes.push(outcome);
        session.drain();
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
        assert!(outcomes.len() < 20000);
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
        let mut backend = machine_crate::machine::tcg::Factory::new(
            &input("SEEDBOX_TCG_QEMU"),
            &input("SEEDBOX_TCG_ASSETS"),
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
        let mut backend = machine_crate::machine::pv::Factory::new(
            &input("SEEDBOX_PV_WORKER"),
            &input("SEEDBOX_PV_ASSETS"),
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
) -> (Vec<StepOutcome>, Vec<machine_crate::driver::Event>)
where
    M: Machine<Context = ()> + Send + 'static,
    M::Error: Debug + Send + 'static,
{
    use machine_crate::driver::{Cause, FaultAction, SettlementStatus};
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
        outcomes.push(outcome);
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
    let settled = events.iter().position(|e| matches!(&e.kind, EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.target == ids[1] && record.boot == 0 && record.action == FaultAction::Spawn)).unwrap();
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
        let mut backend = machine_crate::machine::tcg::Factory::new(
            &input("SEEDBOX_TCG_QEMU"),
            &input("SEEDBOX_TCG_ASSETS"),
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
        let mut backend = machine_crate::machine::pv::Factory::new(
            &input("SEEDBOX_PV_WORKER"),
            &input("SEEDBOX_PV_ASSETS"),
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
    let mut backend = machine_crate::machine::tcg::Factory::new(
        &input("SEEDBOX_TCG_QEMU"),
        &input("SEEDBOX_TCG_ASSETS"),
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
        move |definition| backend.create(definition),
    )
    .unwrap();
    let mut session = Session {
        driver,
        ids: vec![],
        events: vec![],
        started: Instant::now(),
    };
    loop {
        let upto = at(session.now().as_nanos() + QUANTUM);
        let result = session.driver.step(upto).unwrap();
        session.drain();
        if let StepOutcome::Finished { reason, .. } = result {
            assert_eq!(reason, ExitReason::ProgramExit { code: 0 });
            break;
        }
        assert!(session.started.elapsed() < Duration::from_secs(180));
    }
    session.events
}

#[test]
#[ignore = "requires native TCG owned kernel/helper and driver-control application"]
fn tcg_native_boot_realtime_observes_fixed_2000_epoch() {
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
    assert_eq!(clocks.len(), 4);
    assert!(clocks[0] <= clocks[3]);
    assert!(clocks[0] <= clocks[2] && clocks[2] <= clocks[3]);
    // Observation of today's QEMU goldfish RTC, not a definition-level epoch
    // contract. TCG hard-codes 2000-01-01; no host wall clock is consulted.
    // Linux hctosys initializes REALTIME from the RTC's whole seconds during
    // boot, after MONOTONIC has already advanced. Consequently the offset is
    // not precisely epoch: REALTIME - epoch lags MONOTONIC by boot work.
    let epoch = 946_684_800_000_000_000;
    assert!(clocks[1] > epoch);
    assert!(clocks[1] - epoch < clocks[0]);
    for clock in clocks {
        assert_eq!(clock % 100, 0, "current 10 MHz time CSR resolution");
    }
}
