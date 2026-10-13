//! Host inputs (`Barrier::schedule`): enqueue validation, due-time admission
//! through the control policy, live keys, heal, holds, cutoff and determinism.
use seedbox_machine::{
    driver::{
        Action, Cause, Driver, Error, Event, EventKind, FaultAction, FaultHandle, FaultId,
        FaultOrigin, HealTarget, InputKey, InvalidInputKey, MachineId, PlaybackRate,
        RejectionReason, SettlementStatus, StepOutcome, Target,
    },
    execution::ExecutionConfig,
    machine::{Machine, RunContext, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Output, Request, Response, SimulationControlId},
    rng::RngStream,
};
use std::{collections::VecDeque, io, num::NonZeroU32, time::Duration};
#[path = "support/driver_definition.rs"]
mod driver_definition;
#[path = "support/input_key.rs"]
mod input_key;
use input_key::key;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn call(time: u64, request: u32, mut operation: serde_json::Value) -> Output {
    operation["version"] = 1.into();
    operation["id"] = request.into();
    Output {
        at: at(time),
        payload: Response::SimulationControl {
            id: SimulationControlId {
                session: NonZeroU32::new(1).unwrap(),
                request: NonZeroU32::new(request).unwrap(),
            },
            payload: serde_json::to_vec(&operation).unwrap(),
        },
    }
}
fn on(op: &str, target: &str) -> serde_json::Value {
    serde_json::json!({"op": op, "target": {"machine": target}})
}

/// Emits Ready on its first positive run per boot, then its script, and
/// acknowledges application commands, recording their IDs.
#[derive(Clone, Default)]
struct Scripted {
    script: VecDeque<Output>,
    ready: bool,
    crashed: bool,
    fail_at: Option<u64>,
    commands: Vec<(u32, ApplicationCommand)>,
}
impl Machine for Scripted {
    type Context = ();
    type Error = io::Error;
    type Snapshot = Self;
    fn run(&mut self, c: &mut RunContext<'_, ()>, upto: Timestamp) -> io::Result<StopReason> {
        let now = c.clock.now();
        if self.fail_at.is_some_and(|fail| upto.as_nanos() > fail) {
            return Err(io::Error::other("scripted failure"));
        }
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => self.crashed = true,
                Request::Reboot => {
                    self.crashed = false;
                    self.ready = false;
                }
                Request::Application { id, command } if !self.crashed => {
                    self.commands.push((id.get(), command));
                    c.outbox
                        .emit(Output {
                            at: at(now),
                            payload: Response::CommandAcknowledged {
                                id,
                                result: CommandResult::Completed,
                            },
                        })
                        .unwrap();
                }
                _ => {}
            }
        }
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: at(now),
                reason: seedbox_machine::machine::ExitReason::Crashed,
            });
        }
        if upto.as_nanos() > now {
            if !self.ready {
                self.ready = true;
                c.outbox
                    .emit(Output {
                        at: at(now),
                        payload: Response::Ready,
                    })
                    .unwrap();
            }
            while self.script.front().is_some_and(|output| output.at <= upto) {
                c.outbox.emit(self.script.pop_front().unwrap()).unwrap();
            }
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> io::Result<Self> {
        Ok(self.clone())
    }
}

struct Rng;
impl RngStream for Rng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}

/// `oracle` followed by `names`, plus spawnable `spawn`; returns the initial IDs.
fn world(
    workers: usize,
    names: &[&str],
    spawn: &[&str],
    script: impl Fn(&str) -> Scripted + Send + 'static,
) -> (Driver<Scripted>, Vec<MachineId>) {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(1),
        workers,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        names
            .iter()
            .map(|&n| driver_definition::definition(n))
            .collect(),
        spawn
            .iter()
            .map(|&n| driver_definition::definition(n))
            .collect(),
        move |definition| Ok(script(&definition.name)),
    )
    .unwrap();
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect()))
        .unwrap();
    (driver, ids)
}
fn run_to(driver: &mut Driver<Scripted>, upto: u64) -> Vec<Event> {
    let mut events = driver.drain_events();
    loop {
        let outcome = driver.step(at(upto)).unwrap();
        events.extend(driver.drain_events());
        match outcome {
            StepOutcome::BarrierReached { at: now } if now < at(upto) => {}
            _ => return events,
        }
    }
}
fn schedule(driver: &mut Driver<Scripted>, time: u64, name: &str, action: Action) {
    driver
        .with_barrier(|b| b.schedule(at(time), key(name), action))
        .unwrap();
}
fn admitted(events: &[Event]) -> Vec<(FaultId, FaultOrigin, FaultAction, u64)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(r) => Some((r.fault, r.origin.clone(), r.action, r.boot)),
            _ => None,
        })
        .collect()
}
fn input_record(events: &[Event], name: &str) -> Option<seedbox_machine::driver::FaultRecord> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::FaultAdmitted(r) if r.origin == FaultOrigin::Input { key: key(name) } => {
            Some(r.clone())
        }
        _ => None,
    })
}
fn settlement(events: &[Event], name: &str) -> Option<SettlementStatus> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::FaultSettled { record, status, .. }
            if record.origin == FaultOrigin::Input { key: key(name) } =>
        {
            Some(*status)
        }
        _ => None,
    })
}
fn rejection(events: &[Event], name: &str) -> Option<RejectionReason> {
    events.iter().find_map(|e| match &e.kind {
        EventKind::InputRejected { key: k, rejection } if *k == key(name) => Some(rejection.reason),
        _ => None,
    })
}
fn slow() -> ExecutionConfig {
    ExecutionConfig::new(2000).unwrap()
}

fn guest_script(name: &str, heal: bool) -> Scripted {
    let mut script = vec![
        call(
            2,
            1,
            serde_json::json!({"op":"throttle","target":{"machine":"target"},"factor_permille":3000}),
        ),
        call(3, 2, on("stop", "target")),
        call(5, 4, on("restart", "target")),
    ];
    if heal {
        script.insert(
            2,
            call(4, 3, serde_json::json!({"op":"heal","handle":"p1"})),
        );
    }
    Scripted {
        script: if name == "caller" { script } else { vec![] }.into(),
        ..Scripted::default()
    }
}
fn guest_faults(name: &str) -> Scripted {
    guest_script(name, true)
}

#[test]
fn enqueue_reserves_no_fault_handle_or_command_ids() {
    let run = |future: bool| {
        let (mut driver, ids) = world(2, &["caller", "target"], &[], guest_faults);
        if future {
            // Not yet due: queued only, and never reaching admission.
            schedule(
                &mut driver,
                1000,
                "future-throttle",
                Action::Throttle {
                    target: Target::Machine(ids[2]),
                    config: slow(),
                },
            );
            schedule(
                &mut driver,
                1000,
                "future-stop",
                Action::Stop {
                    target: Target::Name("target".into()),
                    deadline: Duration::from_nanos(10),
                },
            );
        }
        let events = run_to(&mut driver, 10);
        let commands = driver
            .with_barrier(|b| Ok(b.machine(ids[2])?.commands.clone()))
            .unwrap();
        (events, commands)
    };
    let (without, commands) = run(false);
    let (with, with_commands) = run(true);
    assert_eq!(with, without);
    assert_eq!(with_commands, commands);
    assert_eq!(
        commands,
        [
            (1, ApplicationCommand::Stop),
            (2, ApplicationCommand::Start)
        ]
    );
    let faults: Vec<_> = admitted(&with).into_iter().map(|a| a.0).collect();
    assert_eq!(faults.len(), 4);
    assert_eq!(
        input_record(&with, "future-throttle"),
        None,
        "a pending input is never admitted early"
    );
    let handle = with.iter().find_map(|e| match &e.kind {
        EventKind::FaultAdmitted(r) => r.handle.clone(),
        _ => None,
    });
    assert_eq!(handle.map(|h| h.to_string()).as_deref(), Some("p1"));
}

#[test]
fn invalid_inputs_error_without_changing_the_queue() {
    let (mut driver, ids) = world(1, &["target"], &["spare"], |_| Scripted::default());
    let target = Target::Machine(ids[1]);
    assert_eq!(
        InputKey::new("bad key"),
        Err(InvalidInputKey("bad key".into()))
    );
    run_to(&mut driver, 5);
    driver
        .with_barrier(|b| {
            b.schedule(at(5), key("live"), Action::Pause(target.clone()))?;
            let errors = [
                b.schedule(at(4), key("past"), Action::Pause(target.clone())),
                b.schedule(at(5), key("live"), Action::Crash(target.clone())),
                b.schedule(
                    at(5),
                    key("name"),
                    Action::Pause(Target::Name("nobody".into())),
                ),
                b.schedule(
                    at(5),
                    key("zero"),
                    Action::Stop {
                        target: target.clone(),
                        deadline: Duration::ZERO,
                    },
                ),
                b.schedule(
                    at(5),
                    key("overflow"),
                    Action::Stop {
                        target: target.clone(),
                        deadline: Duration::from_nanos(u64::MAX),
                    },
                ),
                b.schedule(
                    at(5),
                    key("spawn"),
                    Action::Spawn {
                        name: "target".into(),
                    },
                ),
            ];
            assert!(matches!(errors[0], Err(Error::PastTime { .. })));
            assert!(matches!(&errors[1], Err(Error::LiveInputKey(k)) if *k == key("live")));
            assert!(matches!(&errors[2], Err(Error::UnknownName(n)) if n == "nobody"));
            assert!(matches!(errors[3], Err(Error::InvalidDeadline(_))));
            assert!(matches!(errors[4], Err(Error::InvalidDeadline(_))));
            assert!(matches!(&errors[5], Err(Error::UnknownName(n)) if n == "target"));
            Ok(())
        })
        .unwrap();
    // A foreign identity is unknown here.
    let (mut other, _) = world(1, &["target"], &[], |_| Scripted::default());
    let foreign = other
        .with_barrier(|b| b.add(|| Ok(Scripted::default())))
        .unwrap();
    assert!(matches!(
        driver.with_barrier(|b| b.schedule(at(5), key("foreign"), Action::Crash(Target::Machine(foreign)))),
        Err(Error::UnknownMachine(found)) if found == foreign
    ));
    // Only the valid input reaches admission; failed keys stay unused.
    let events = run_to(&mut driver, 5);
    let inputs: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(r) => Some(r.origin.clone()),
            EventKind::InputRejected { key, .. } => Some(FaultOrigin::Input { key: key.clone() }),
            _ => None,
        })
        .collect();
    assert_eq!(inputs, [FaultOrigin::Input { key: key("live") }]);
    driver
        .with_barrier(|b| {
            for name in ["past", "zero", "name", "overflow", "spawn", "foreign"] {
                b.schedule(at(6), key(name), Action::Resume(target.clone()))?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn boot_binds_when_the_input_becomes_due() {
    let (mut driver, ids) = world(1, &["target"], &[], |_| Scripted::default());
    let target = ids[1];
    // All scheduled at time zero; each binds the boot current at its due time.
    schedule(
        &mut driver,
        2,
        "crash",
        Action::Crash(Target::Machine(target)),
    );
    schedule(
        &mut driver,
        3,
        "reboot",
        Action::Reboot(Target::Machine(target)),
    );
    schedule(
        &mut driver,
        5,
        "stop",
        Action::Stop {
            target: Target::Name("target".into()),
            deadline: Duration::from_nanos(100),
        },
    );
    let events = run_to(&mut driver, 10);
    let boots: Vec<_> = ["crash", "reboot", "stop"]
        .iter()
        .map(|name| input_record(&events, name).unwrap().boot)
        .collect();
    assert_eq!(boots, [0, 0, 1]);
    assert_eq!(settlement(&events, "stop"), Some(SettlementStatus::Applied));
    let commands = driver
        .with_barrier(|b| Ok(b.machine(target)?.commands.clone()))
        .unwrap();
    assert_eq!(commands, [(1, ApplicationCommand::Stop)]);
}

#[test]
fn heal_by_key_or_handle_restores_nominal_and_rejects_inactive_targets() {
    let (mut driver, ids) = world(1, &["caller", "target", "other"], &[], guest_faults);
    let (target, other) = (ids[2], ids[3]);
    // Healing a key whose throttle is not yet admitted is rejected.
    schedule(
        &mut driver,
        1,
        "too-early",
        Action::Heal(HealTarget::Input(key("by-key"))),
    );
    schedule(
        &mut driver,
        1,
        "by-key",
        Action::Throttle {
            target: Target::Machine(other),
            config: slow(),
        },
    );
    let events = run_to(&mut driver, 3);
    assert_eq!(
        rejection(&events, "too-early"),
        Some(RejectionReason::InvalidState)
    );
    assert_eq!(
        settlement(&events, "by-key"),
        Some(SettlementStatus::Applied)
    );
    // Host and guest throttles share one handle allocator.
    let ours = input_record(&events, "by-key").unwrap().handle.unwrap();
    let guest: FaultHandle = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultAdmitted(r) if matches!(r.origin, FaultOrigin::Control { .. }) => {
                r.handle.clone()
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (ours.to_string(), guest.to_string()),
        ("p1".into(), "p2".into())
    );
    schedule(
        &mut driver,
        3,
        "heal-key",
        Action::Heal(HealTarget::Input(key("by-key"))),
    );
    let events = run_to(&mut driver, 3);
    assert_eq!(
        settlement(&events, "heal-key"),
        Some(SettlementStatus::Applied)
    );
    let record = input_record(&events, "heal-key").unwrap();
    assert_eq!((record.action, record.target), (FaultAction::Heal, other));
    driver
        .with_barrier(|b| {
            assert_eq!(b.machines().count(), 4);
            // The healed handle is inactive, and the released key owns nothing.
            b.schedule(
                at(3),
                key("heal-handle"),
                Action::Heal(HealTarget::Handle(ours.clone())),
            )?;
            b.schedule(
                at(3),
                key("by-key"),
                Action::Heal(HealTarget::Input(key("by-key"))),
            )?;
            // A host heal of the guest's handle, and a fresh host throttle.
            b.schedule(
                at(4),
                key("heal-guest"),
                Action::Heal(HealTarget::Handle(guest.clone())),
            )?;
            b.schedule(
                at(4),
                key("again"),
                Action::Throttle {
                    target: Target::Machine(other),
                    config: slow(),
                },
            )
        })
        .unwrap();
    let events = run_to(&mut driver, 5);
    assert_eq!(
        rejection(&events, "heal-handle"),
        Some(RejectionReason::InvalidState)
    );
    assert_eq!(
        rejection(&events, "by-key"),
        Some(RejectionReason::InvalidState)
    );
    assert_eq!(
        settlement(&events, "heal-guest"),
        Some(SettlementStatus::Applied)
    );
    let fresh = input_record(&events, "again").unwrap().handle.unwrap();
    assert_eq!(fresh.to_string(), "p3");
    schedule(
        &mut driver,
        5,
        "heal-fresh",
        Action::Heal(HealTarget::Handle(fresh.clone())),
    );
    let events = run_to(&mut driver, 5);
    assert_eq!(
        settlement(&events, "heal-fresh"),
        Some(SettlementStatus::Applied)
    );
    let record = input_record(&events, "heal-fresh").unwrap();
    assert_eq!(
        (record.action, record.handle),
        (FaultAction::Heal, Some(fresh))
    );
    // Healing released the throttle's key.
    schedule(
        &mut driver,
        5,
        "again",
        Action::Pause(Target::Machine(target)),
    );
}

fn hold_script(name: &str) -> Scripted {
    let script = match name {
        "caller" => vec![
            call(
                2,
                1,
                serde_json::json!({"op":"arm_trigger","source":{"machine":"source"},"marker":b"needle",
                    "match_timeout_ns":20,"fault":{"action":"stop","target":{"machine":"target"}}}),
            ),
            call(7, 2, on("pause", "target")),
        ],
        "source" => vec![Output {
            at: at(5),
            payload: Response::BootConsole {
                bytes: b"xxneedle".to_vec(),
            },
        }],
        _ => vec![],
    };
    Scripted {
        script: script.into(),
        ..Scripted::default()
    }
}

#[test]
fn pause_before_next_fault_holds_trigger_guest_and_input_effects_alike() {
    let (mut driver, ids) = world(1, &["caller", "source", "target"], &[], hold_script);
    let target = ids[3];
    schedule(
        &mut driver,
        9,
        "throttle",
        Action::Throttle {
            target: Target::Machine(target),
            config: slow(),
        },
    );
    // A rejected input (target already paused) never consumes the arm.
    schedule(
        &mut driver,
        8,
        "rejected",
        Action::Pause(Target::Machine(target)),
    );
    driver
        .with_barrier(|b| b.pause_before_next_fault(PlaybackRate::Unlimited))
        .unwrap();
    let mut holds = Vec::new();
    let mut events = Vec::new();
    for _ in 0..100 {
        let outcome = driver.step(at(12)).unwrap();
        events.extend(driver.drain_events());
        if let StepOutcome::BeforeFault { at, fault } = &outcome {
            // Repeated steps return the same hold without progress.
            assert_eq!(driver.step(*at).unwrap(), outcome);
            driver
                .with_barrier(|b| {
                    assert_eq!(b.playback().before_fault.as_ref(), Some(fault));
                    assert!(matches!(
                        b.schedule(*at, key("held"), Action::Pause(Target::Machine(target))),
                        Err(Error::BeforeFault)
                    ));
                    b.pause_before_next_fault(PlaybackRate::Unlimited)
                })
                .unwrap();
            holds.push((*at, fault.origin.clone(), fault.action));
        }
        if matches!(outcome, StepOutcome::BarrierReached { at } if at == self::at(12)) {
            break;
        }
    }
    assert_eq!(
        holds,
        [
            (
                at(5),
                FaultOrigin::Trigger { trigger: 1 },
                FaultAction::Application(ApplicationCommand::Stop)
            ),
            (
                at(7),
                FaultOrigin::Control {
                    machine: ids[1],
                    call: SimulationControlId {
                        session: NonZeroU32::new(1).unwrap(),
                        request: NonZeroU32::new(2).unwrap(),
                    }
                },
                FaultAction::Pause
            ),
            (
                at(9),
                FaultOrigin::Input {
                    key: key("throttle")
                },
                FaultAction::SetExecution(slow())
            ),
        ]
    );
    assert_eq!(
        rejection(&events, "rejected"),
        Some(RejectionReason::InvalidState)
    );
    // Every held effect applied exactly once.
    let applied = events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::FaultSettled {
                    status: SettlementStatus::Applied,
                    ..
                }
            )
        })
        .count();
    assert_eq!(applied, 3);
    driver
        .with_barrier(|b| {
            assert!(b.paused(target)?);
            assert_eq!(b.machine(target)?.commands, [(1, ApplicationCommand::Stop)]);
            Ok(())
        })
        .unwrap();
}

#[test]
fn pending_inputs_are_never_admitted_after_stop_or_failure() {
    // Permanent stop.
    let (mut driver, ids) = world(1, &["target"], &[], |_| Scripted::default());
    schedule(
        &mut driver,
        3,
        "after-stop",
        Action::Pause(Target::Machine(ids[1])),
    );
    driver.drain_events();
    driver
        .with_barrier(|b| {
            b.stop();
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Stopped { at: at(0) }
    );
    assert!(driver.drain_events().is_empty());
    // Failure before the input is due.
    let (mut driver, ids) = world(1, &["target"], &[], |name| Scripted {
        fail_at: (name == "target").then_some(2),
        ..Scripted::default()
    });
    schedule(
        &mut driver,
        5,
        "after-failure",
        Action::Pause(Target::Machine(ids[1])),
    );
    let mut result = Ok(StepOutcome::Blocked { at: at(0) });
    for _ in 0..10 {
        result = driver.step(at(10));
        if result.is_err() {
            break;
        }
    }
    assert!(matches!(result, Err(Error::Machine { .. })));
    assert!(input_record(&driver.drain_events(), "after-failure").is_none());
    assert!(matches!(driver.step(at(10)), Err(Error::Failed)));
    assert!(driver.drain_events().is_empty());
}

fn mixed(workers: usize, by_name: bool) -> Vec<Event> {
    let (mut driver, ids) = world(
        workers,
        &["caller", "target", "other"],
        &["spare"],
        |name| guest_script(name, false),
    );
    let target = |id: MachineId, name: &str| {
        if by_name {
            Target::Name(name.into())
        } else {
            Target::Machine(id)
        }
    };
    let (t, o) = (ids[2], ids[3]);
    for (time, name, action) in [
        (1, "pause", Action::Pause(target(o, "other"))),
        (
            1,
            "spawn",
            Action::Spawn {
                name: "spare".into(),
            },
        ),
        (
            1,
            "spawn-again",
            Action::Spawn {
                name: "spare".into(),
            },
        ),
        (
            2,
            "throttle",
            Action::Throttle {
                target: target(o, "other"),
                config: slow(),
            },
        ),
        (3, "resume", Action::Resume(target(o, "other"))),
        (
            6,
            "freeze",
            Action::PauseProcess {
                target: target(t, "target"),
                deadline: Duration::from_nanos(50),
            },
        ),
        (
            7,
            "thaw",
            Action::ResumeProcess {
                target: target(t, "target"),
                deadline: Duration::from_nanos(50),
            },
        ),
        (6, "crash", Action::Crash(target(o, "other"))),
        (7, "reboot", Action::Reboot(target(o, "other"))),
        (8, "heal", Action::Heal(HealTarget::Input(key("throttle")))),
        (8, "shutdown", Action::Shutdown(target(o, "other"))),
    ] {
        schedule(&mut driver, time, name, action);
    }
    run_to(&mut driver, 12)
}

#[test]
fn name_targets_match_identity_targets_and_streams_are_worker_independent() {
    let reference = mixed(1, false);
    assert_eq!(mixed(1, true), reference);
    for workers in [2, 4] {
        assert_eq!(mixed(workers, false), reference);
        assert_eq!(mixed(workers, true), reference);
    }
    // Spawn admits once through the shared path; the duplicate is rejected.
    let spawned = reference
        .iter()
        .find_map(|e| match e.kind {
            EventKind::MachineAdded {
                machine,
                cause: Cause::Fault(fault),
                ..
            } => Some((machine, fault)),
            _ => None,
        })
        .unwrap();
    let spawn = input_record(&reference, "spawn").unwrap();
    assert_eq!(
        (spawn.action, spawn.target, spawn.fault),
        (FaultAction::Spawn, spawned.0, spawned.1)
    );
    assert_eq!(
        rejection(&reference, "spawn-again"),
        Some(RejectionReason::InvalidRequest)
    );
    for name in [
        "pause", "throttle", "resume", "freeze", "thaw", "crash", "reboot", "heal", "shutdown",
    ] {
        assert_eq!(
            settlement(&reference, name),
            Some(SettlementStatus::Applied),
            "{name}"
        );
    }
}

#[test]
fn oracle_completion_cuts_off_pending_inputs_without_per_key_events() {
    let (mut driver, ids) = world(1, &["target"], &[], |name| Scripted {
        script: if name == "oracle" {
            vec![Output {
                at: at(3),
                payload: Response::ApplicationOutcome { code: 0 },
            }]
        } else {
            vec![]
        }
        .into(),
        ..Scripted::default()
    });
    schedule(
        &mut driver,
        2,
        "before",
        Action::Pause(Target::Machine(ids[1])),
    );
    schedule(
        &mut driver,
        6,
        "after",
        Action::Pause(Target::Machine(ids[1])),
    );
    let events = run_to(&mut driver, 10);
    assert!(matches!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished { at: t, .. } if t == at(3)
    ));
    assert_eq!(
        settlement(&events, "before"),
        Some(SettlementStatus::Applied)
    );
    assert!(input_record(&events, "after").is_none());
    assert_eq!(rejection(&events, "after"), None);
    assert!(driver.drain_events().is_empty());
    // Completion also refuses further scheduling.
    assert!(matches!(
        driver.with_barrier(|b| b.schedule(
            at(6),
            key("late"),
            Action::Resume(Target::Machine(ids[1]))
        )),
        Err(Error::Completed)
    ));
}

#[test]
fn a_display_name_equal_to_an_internal_key_is_only_a_name() {
    // Machine 1 is named "machine:0"; no machine is named "machine:2".
    let (mut driver, ids) = world(1, &["machine:0", "caller", "other"], &[], |name| Scripted {
        script: if name == "caller" {
            vec![
                call(2, 1, on("pause", "machine:0")),
                call(3, 2, on("pause", "machine:3")),
                call(
                    3,
                    3,
                    serde_json::json!({"op":"pause","target":{"service":"other"}}),
                ),
            ]
        } else {
            vec![]
        }
        .into(),
        ..Scripted::default()
    });
    let (named, other) = (ids[1], ids[3]);
    schedule(
        &mut driver,
        1,
        "host",
        Action::Throttle {
            target: Target::Name("machine:0".into()),
            config: slow(),
        },
    );
    driver
        .with_barrier(|b| {
            assert!(matches!(
                b.schedule(at(1), key("missing"), Action::Pause(Target::Name("machine:3".into()))),
                Err(Error::UnknownName(n)) if n == "machine:3"
            ));
            Ok(())
        })
        .unwrap();
    let events = run_to(&mut driver, 5);
    assert_eq!(input_record(&events, "host").unwrap().target, named);
    let guest: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(r) if matches!(r.origin, FaultOrigin::Control { .. }) => {
                Some(r.target)
            }
            _ => None,
        })
        .collect();
    // "machine:3" resolves no display name (the key machine:3 exists, but is
    // never a wire target); the service name resolves its machine.
    assert_eq!(guest, [named, other]);
    driver
        .with_barrier(|b| {
            assert!(!b.paused(ids[0])?);
            assert!(b.paused(named)? && b.paused(other)?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn a_key_is_reusable_after_its_stale_boot_settlement() {
    let (mut driver, ids) = world(1, &["target"], &[], |_| Scripted::default());
    let target = Target::Machine(ids[1]);
    schedule(&mut driver, 1, "crash", Action::Crash(target.clone()));
    run_to(&mut driver, 2);
    // Both admitted against the crashed boot 0; the reboot effect applies
    // first, so the throttle is stale at its turn and installs nothing.
    schedule(&mut driver, 2, "reboot", Action::Reboot(target.clone()));
    schedule(
        &mut driver,
        2,
        "slow",
        Action::Throttle {
            target: target.clone(),
            config: slow(),
        },
    );
    let events = run_to(&mut driver, 2);
    assert_eq!(input_record(&events, "slow").unwrap().boot, 0);
    assert_eq!(
        settlement(&events, "slow"),
        Some(SettlementStatus::Unavailable)
    );
    assert!(!events.iter().any(|e| matches!(
        e.kind,
        EventKind::Transition {
            change: seedbox_machine::driver::Transition::ExecutionSet(_),
            ..
        }
    )));
    // The stale settlement released the key; its reuse binds the new boot.
    schedule(
        &mut driver,
        2,
        "slow",
        Action::Throttle {
            target,
            config: slow(),
        },
    );
    let events = run_to(&mut driver, 3);
    let record = input_record(&events, "slow").unwrap();
    assert_eq!(record.boot, 1);
    assert_eq!(settlement(&events, "slow"), Some(SettlementStatus::Applied));
    // The stale attempt's handle was released, so a fresh one is allocated.
    assert_eq!(record.handle.unwrap().to_string(), "p2");
}
