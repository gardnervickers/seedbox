//! Public Driver event stream: order, causes, control facts and failure prefixes.
use seedbox_driver::driver::{
    Action, Cause, Driver, Error, Event, EventKind, FaultAction, FaultId, FaultOrigin, MachineId,
    SettlementStatus, StepOutcome, Target, Transition, TriggerResult,
};
use seedbox_machine::{
    execution::ExecutionConfig,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Output, Request, Response, SimulationControlId},
    rng::RngStream,
};
use std::{collections::VecDeque, io, num::NonZeroU32, time::Duration};
#[path = "support/driver_definition.rs"]
mod driver_definition;
#[path = "support/input_key.rs"]
mod input_key;
use input_key::{auto_key, key};

/// The machine and bound boot of a single-machine fault record.
fn machine_boot(target: &seedbox_driver::driver::FaultTarget) -> (MachineId, u64) {
    match target {
        seedbox_driver::driver::FaultTarget::Machine { machine, boot } => (*machine, *boot),
        other => panic!("expected a machine target: {other:?}"),
    }
}

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn call_id(request: u32) -> SimulationControlId {
    SimulationControlId {
        session: NonZeroU32::new(1).unwrap(),
        request: NonZeroU32::new(request).unwrap(),
    }
}
fn call(time: u64, request: u32, mut operation: serde_json::Value) -> Output {
    operation["version"] = 1.into();
    operation["id"] = request.into();
    Output {
        at: at(time),
        payload: Response::SimulationControl {
            id: call_id(request),
            payload: serde_json::to_vec(&operation).unwrap(),
        },
    }
}
fn on(op: &str, target: &str) -> serde_json::Value {
    serde_json::json!({"op": op, "target": {"machine": target}})
}
fn console(time: u64, bytes: &[u8]) -> Output {
    Output {
        at: at(time),
        payload: Response::BootConsole {
            bytes: bytes.to_vec(),
        },
    }
}

/// Emits Ready on its first positive run per boot, then its script; acknowledges
/// application commands at admission time.
#[derive(Clone, Default)]
struct Scripted {
    script: VecDeque<Output>,
    ready: bool,
    exited: Option<ExitReason>,
    fail_on_crash: bool,
}
impl Machine for Scripted {
    type Context = ();
    type Error = io::Error;
    type Snapshot = Self;
    fn run(&mut self, c: &mut RunContext<'_, ()>, upto: Timestamp) -> io::Result<StopReason> {
        let now = c.clock.now();
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash if self.fail_on_crash => {
                    return Err(io::Error::other("uncertain crash"))
                }
                Request::Crash => self.exited = Some(ExitReason::Crashed),
                Request::Reboot => {
                    self.exited = None;
                    self.ready = false;
                }
                Request::Application { id, .. } if self.exited.is_none() => c
                    .outbox
                    .emit(Output {
                        at: at(now),
                        payload: Response::CommandAcknowledged {
                            id,
                            result: CommandResult::Completed,
                        },
                    })
                    .unwrap(),
                _ => {}
            }
        }
        if let Some(reason) = self.exited {
            return Ok(StopReason::Exited {
                reached: at(now),
                reason,
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

/// Machines are named `oracle` followed by `names`; returns their identities.
fn world(
    quantum: u64,
    workers: usize,
    names: &[&str],
    script: impl Fn(&str) -> Scripted + Send + 'static,
) -> (Driver<Scripted>, Vec<MachineId>) {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(quantum),
        workers,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        names
            .iter()
            .map(|&n| driver_definition::definition(n))
            .collect(),
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        move |definition| Ok(script(&definition.name)),
    )
    .unwrap();
    let ids = driver
        .with_barrier(|b| Ok(b.machines().map(|(id, _)| id).collect()))
        .unwrap();
    (driver, ids)
}

fn run_to(driver: &mut Driver<Scripted>, upto: u64) -> Vec<Event> {
    let mut events = Vec::new();
    loop {
        let outcome = driver.step(at(upto)).unwrap();
        // Hosts drain after every step.
        events.extend(driver.drain_observations().events);
        match outcome {
            StepOutcome::BarrierReached { at: now } if now < at(upto) => {}
            _ => return events,
        }
    }
}

fn transitions(events: &[Event]) -> Vec<(MachineId, u64, Transition, Cause, Timestamp)> {
    events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Transition {
                machine,
                boot,
                change,
                cause,
            } => Some((machine, boot, change, cause, e.at)),
            _ => None,
        })
        .collect()
}

fn assert_dense(events: &[Event], first: u64) {
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event.sequence, first + index as u64);
    }
    for pair in events.windows(2) {
        assert!(pair[0].position.boundary_index <= pair[1].position.boundary_index);
        assert!(pair[0].position.at <= pair[1].position.at);
    }
}

#[test]
fn outputs_sort_within_a_commit_and_separate_commits_keep_publication_order() {
    let (mut driver, ids) = world(10, 2, &["a", "b"], |name| Scripted {
        script: match name {
            "a" => [console(3, b"a3"), console(7, b"a7"), console(13, b"a13")].into(),
            "b" => [console(3, b"b3"), console(5, b"b5")].into(),
            _ => VecDeque::new(),
        },
        ..Scripted::default()
    });
    let initial = driver.drain_observations().events;
    assert_eq!(initial.len(), 3);
    for (index, event) in initial.iter().enumerate() {
        assert_eq!(event.sequence, index as u64);
        assert_eq!(event.position.boundary_index, 0);
        assert_eq!(event.at, at(0));
        assert!(matches!(event.kind,
            EventKind::MachineAdded { machine, cause: Cause::Initial, .. } if machine == ids[index]));
    }
    driver
        .with_barrier(|b| b.schedule(at(10), auto_key(), Action::Pause(Target::Machine(ids[2]))))
        .unwrap();
    assert!(
        driver.drain_observations().events.is_empty(),
        "enqueueing publishes nothing"
    );
    let events = run_to(&mut driver, 20);
    assert_dense(&events, 3);
    let labelled: Vec<_> = events
        .iter()
        .map(|e| match &e.kind {
            EventKind::Output(o) => match &o.payload {
                Response::Ready => (format!("ready{}", o.machine == ids[1]), e.position),
                Response::BootConsole { bytes } => {
                    (String::from_utf8(bytes.clone()).unwrap(), e.position)
                }
                other => panic!("{other:?}"),
            },
            EventKind::Transition { change, .. } => (format!("{change:?}"), e.position),
            EventKind::FaultAdmitted(_) => ("admitted".into(), e.position),
            EventKind::FaultSettled { .. } => ("settled".into(), e.position),
            other => panic!("{other:?}"),
        })
        .collect();
    let names: Vec<_> = labelled.iter().map(|(n, _)| n.as_str()).collect();
    // One commit: time, then identity, then emission order. The pause input is
    // admitted after that commit at its own barrier, and its effect applies at
    // the next one, at the same time; the next execution commit follows.
    assert_eq!(
        names,
        [
            "readyfalse",
            "readytrue",
            "readyfalse",
            "a3",
            "b3",
            "b5",
            "a7",
            "admitted",
            "Paused",
            "settled",
            "a13"
        ]
    );
    assert_eq!(labelled[0].1, labelled[6].1);
    assert_eq!(labelled[6].1.at, at(10));
    assert_eq!(
        labelled[7].1.boundary_index,
        labelled[6].1.boundary_index + 1
    );
    assert_eq!(labelled[8].1, labelled[9].1);
    assert_eq!(
        labelled[8].1.boundary_index,
        labelled[7].1.boundary_index + 1
    );
    assert_eq!(labelled[8].1.at, at(10));
    assert_eq!(labelled[10].1.at, at(20));
    assert!(
        driver.drain_observations().events.is_empty(),
        "each event is taken once"
    );
}

#[test]
fn host_inputs_publish_effective_changes_with_their_fault_cause() {
    let (mut driver, ids) = world(10, 2, &["m"], |_| Scripted::default());
    let m = ids[1];
    let slow = ExecutionConfig::new(2000).unwrap();
    driver.drain_observations();
    let mut events = Vec::new();
    // One input per barrier, all at time 5: each is admitted against the state
    // its predecessor left. Repeats are rejected rather than ignored.
    for (index, action) in [
        Action::Pause(Target::Machine(m)),
        Action::Pause(Target::Machine(m)),
        Action::Resume(Target::Machine(m)),
        Action::Resume(Target::Machine(m)),
        Action::Throttle {
            target: Target::Machine(m),
            config: slow,
        },
        Action::Shutdown(Target::Machine(m)),
        Action::Crash(Target::Machine(m)),
        Action::Shutdown(Target::Machine(m)),
        Action::Pause(Target::Machine(m)),
        Action::Reboot(Target::Machine(m)),
    ]
    .into_iter()
    .enumerate()
    {
        driver
            .with_barrier(|b| b.schedule(at(5), key(&format!("input-{index}")), action))
            .unwrap();
        events.extend(run_to(&mut driver, 5));
    }
    let faults = fault_ids(&events);
    let fault = |n: usize| Cause::Fault(faults[n]);
    assert_eq!(
        transitions(&events),
        [
            (m, 0, Transition::Paused, fault(0), at(5)),
            (m, 0, Transition::Resumed, fault(1), at(5)),
            (m, 0, Transition::ExecutionSet(slow), fault(2), at(5)),
            (m, 0, Transition::ShutdownRequested, fault(3), at(5)),
            (m, 0, Transition::Crashed, fault(4), at(5)),
            (m, 0, Transition::Rebooted { new_boot: 1 }, fault(5), at(5)),
        ]
    );
    let rejected: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::InputRejected { key, .. } => Some(key.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(rejected, ["input-1", "input-3", "input-7", "input-8"]);
    for e in &events {
        if let EventKind::FaultAdmitted(record) = &e.kind {
            assert!(matches!(record.origin, FaultOrigin::Input { .. }));
            assert_eq!(record.requested_at, at(5));
        }
    }
}

fn fault_script(name: &str) -> Scripted {
    let script = match name {
        "caller" => vec![
            call(2, 1, on("pause", "target")),
            call(3, 2, on("resume", "target")),
            call(
                4,
                3,
                serde_json::json!({"op":"throttle","target":{"machine":"target"},"factor_permille":2000}),
            ),
            call(5, 4, serde_json::json!({"op":"heal","handle":"p1"})),
            call(6, 5, on("crash", "target")),
            call(7, 6, on("stop", "target")),
            call(8, 7, on("reboot", "target")),
            call(10, 8, on("pause", "target")),
            call(10, 9, on("pause", "target")),
            call(11, 10, on("resume", "target")),
        ],
        _ => vec![],
    };
    Scripted {
        script: script.into(),
        ..Scripted::default()
    }
}

fn fault_world(workers: usize) -> (Vec<Event>, Vec<MachineId>) {
    let (mut driver, ids) = world(1, workers, &["caller", "target"], fault_script);
    let mut events = driver.drain_observations().events;
    events.extend(run_to(&mut driver, 14));
    (events, ids)
}

#[test]
fn fault_admission_settlement_and_transitions_are_incremental_pairs() {
    let (events, ids) = fault_world(2);
    assert_dense(&events, 0);
    let (caller, target) = (ids[1], ids[2]);
    let fault = |n: usize| Cause::Fault(fault_ids(&events)[n - 1]);
    assert_eq!(
        transitions(&events),
        [
            (target, 0, Transition::Paused, fault(1), at(2)),
            (target, 0, Transition::Resumed, fault(2), at(3)),
            (
                target,
                0,
                Transition::ExecutionSet(ExecutionConfig::new(2000).unwrap()),
                fault(3),
                at(4)
            ),
            (
                target,
                0,
                Transition::ExecutionSet(ExecutionConfig::default()),
                fault(4),
                at(5)
            ),
            (target, 0, Transition::Crashed, fault(5), at(6)),
            (
                target,
                0,
                Transition::Rebooted { new_boot: 1 },
                fault(7),
                at(8)
            ),
            (target, 1, Transition::Paused, fault(8), at(10)),
            (target, 1, Transition::Resumed, fault(10), at(11)),
        ]
    );
    let expected = [
        (FaultAction::Pause, SettlementStatus::Applied, 0),
        (FaultAction::Resume, SettlementStatus::Applied, 0),
        (
            FaultAction::SetExecution(ExecutionConfig::new(2000).unwrap()),
            SettlementStatus::Applied,
            0,
        ),
        (FaultAction::Heal, SettlementStatus::Applied, 0),
        (FaultAction::Crash, SettlementStatus::Applied, 0),
        (
            FaultAction::Application(ApplicationCommand::Stop),
            SettlementStatus::Unavailable,
            0,
        ),
        (FaultAction::Reboot, SettlementStatus::Applied, 0),
        (FaultAction::Pause, SettlementStatus::Applied, 1),
        (FaultAction::Pause, SettlementStatus::Rejected, 1),
        (FaultAction::Resume, SettlementStatus::Applied, 1),
    ];
    let ids_in_order = fault_ids(&events);
    assert_eq!(ids_in_order.len(), expected.len());
    for (index, (fault, (action, status, boot))) in ids_in_order.iter().zip(expected).enumerate() {
        let admitted = events
            .iter()
            .position(|e| matches!(&e.kind, EventKind::FaultAdmitted(r) if r.fault == *fault))
            .unwrap();
        let settled: Vec<_> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(&e.kind, EventKind::FaultSettled { record, .. } if record.fault == *fault))
            .collect();
        assert_eq!(settled.len(), 1, "one settlement per fault");
        let (settled_at, settled) = settled[0];
        assert!(admitted < settled_at);
        let EventKind::FaultAdmitted(record) = &events[admitted].kind else {
            unreachable!()
        };
        assert_eq!(
            record.origin,
            FaultOrigin::Control {
                machine: caller,
                call: call_id(index as u32 + 1)
            }
        );
        assert_eq!(
            (machine_boot(&record.target), record.action),
            ((target, boot), action)
        );
        // Throttle installs the handle that its heal names.
        let handle = (index == 2 || index == 3).then(|| "p1".to_string());
        assert_eq!(record.handle.as_ref().map(ToString::to_string), handle);
        let EventKind::FaultSettled {
            record: repeated,
            status: actual,
            outcome_at,
            ..
        } = &settled.kind
        else {
            unreachable!()
        };
        assert_eq!(repeated, record);
        assert_eq!(*actual, status);
        // Only Applied settlements follow a published change of that fault.
        let changed = transitions(&events[admitted..settled_at])
            .iter()
            .any(|t| t.3 == Cause::Fault(*fault));
        assert_eq!(changed, status == SettlementStatus::Applied);
        assert_eq!(*outcome_at, Some(events[admitted].at));
    }
}

fn fault_ids(events: &[Event]) -> Vec<FaultId> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(record) => Some(record.fault),
            _ => None,
        })
        .collect()
}

fn trigger_script(name: &str) -> Scripted {
    let arm = |marker: &[u8], timeout: u64, target: &str| {
        serde_json::json!({"op":"arm_trigger","source":{"machine":"source"},"marker":marker,
            "match_timeout_ns":timeout,"fault":{"action":"stop","target":{"machine":target}}})
    };
    let script = match name {
        "oracle" => vec![Output {
            at: at(8),
            payload: Response::ApplicationOutcome { code: 0 },
        }],
        "caller" => vec![
            call(2, 1, arm(b"needle", 20, "target")),
            call(2, 2, arm(b"never", 3, "target")),
            call(2, 3, arm(b"gone", 20, "target")),
            call(2, 4, arm(b"late", 100, "target")),
            call(2, 5, arm(b"needle", 20, "peer")),
            call(
                3,
                6,
                serde_json::json!({"op":"cancel_trigger","trigger_id":3}),
            ),
        ],
        "source" => vec![console(5, b"xxneedle")],
        _ => vec![],
    };
    Scripted {
        script: script.into(),
        ..Scripted::default()
    }
}

fn trigger_world(workers: usize) -> (Vec<Event>, Vec<MachineId>, StepOutcome) {
    let (mut driver, ids) = world(
        1,
        workers,
        &["caller", "source", "target", "peer"],
        trigger_script,
    );
    driver
        .with_barrier(|b| {
            b.schedule(
                at(1),
                key("peer-pause"),
                Action::Pause(Target::Machine(ids[4])),
            )
        })
        .unwrap();
    let mut events = driver.drain_observations().events;
    let mut outcome;
    loop {
        outcome = driver.step(at(20)).unwrap();
        events.extend(driver.drain_observations().events);
        if !matches!(outcome, StepOutcome::BarrierReached { .. }) {
            break;
        }
    }
    (events, ids, outcome)
}

#[test]
fn triggers_publish_arm_and_final_resolution_before_caused_admission() {
    let (events, ids, outcome) = trigger_world(2);
    assert_eq!(
        outcome,
        StepOutcome::Finished {
            at: at(8),
            reason: ExitReason::ProgramExit { code: 0 }
        }
    );
    assert_dense(&events, 0);
    let (source, target, peer) = (ids[2], ids[3], ids[4]);
    let armed: Vec<_> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::TriggerArmed {
                trigger,
                source: s,
                target: t,
                deadline,
            } => Some((trigger, s, t, deadline, e.at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        armed,
        [
            (1, source, Some(target), at(22), at(2)),
            (2, source, Some(target), at(5), at(2)),
            (3, source, Some(target), at(22), at(2)),
            (4, source, Some(target), at(102), at(2)),
            (5, source, Some(peer), at(22), at(2)),
        ]
    );
    let resolved: Vec<_> = events
        .iter()
        .enumerate()
        .filter_map(|(index, e)| match e.kind {
            EventKind::TriggerResolved {
                trigger,
                result,
                fault,
            } => Some((index, trigger, result, fault, e.at)),
            _ => None,
        })
        .collect();
    let summary: Vec<_> = resolved.iter().map(|r| (r.1, r.2, r.4)).collect();
    assert_eq!(
        summary,
        [
            (3, TriggerResult::Cancelled, at(3)),
            (1, TriggerResult::Fired, at(5)),
            (5, TriggerResult::Rejected, at(5)),
            (2, TriggerResult::Expired, at(5)),
            (4, TriggerResult::CutOff, at(8)),
        ]
    );
    let (fired_index, _, _, fault, _) = resolved[1];
    let fault = fault.expect("fired trigger names its fault");
    let EventKind::FaultAdmitted(record) = &events[fired_index + 1].kind else {
        panic!("fault admission follows its trigger resolution");
    };
    assert_eq!(record.fault, fault);
    assert_eq!(record.origin, FaultOrigin::Trigger { trigger: 1 });
    assert_eq!(machine_boot(&record.target).0, target);
    assert!(events.iter().any(|e| matches!(&e.kind,
        EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.fault == fault)));
    assert!(resolved.iter().all(|r| r.3.is_none() || r.1 == 1));
}

#[test]
fn reply_admission_excludes_discarded_and_retired_replies() {
    let (mut driver, ids) = world(1, 2, &["caller", "other"], |name| Scripted {
        script: match name {
            "caller" => vec![
                call(2, 1, serde_json::json!({"op":"now"})),
                call(3, 2, serde_json::json!({"op":"sleep","duration_ns":5})),
                call(9, 3, serde_json::json!({"op":"now"})),
            ],
            "other" => vec![call(
                3,
                1,
                serde_json::json!({"op":"sleep","duration_ns":2}),
            )],
            _ => vec![],
        }
        .into(),
        ..Scripted::default()
    });
    let (caller, other) = (ids[1], ids[2]);
    driver
        .with_barrier(|b| {
            b.schedule(at(4), auto_key(), Action::Crash(Target::Machine(other)))?;
            b.schedule(at(4), auto_key(), Action::Crash(Target::Machine(caller)))?;
            b.schedule(at(5), auto_key(), Action::Reboot(Target::Machine(caller)))
        })
        .unwrap();
    let events = run_to(&mut driver, 12);
    let replies: Vec<_> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::ReplyAdmitted { machine, boot, id } => Some((machine, boot, id, e.at)),
            _ => None,
        })
        .collect();
    assert_eq!(
        replies,
        [
            (caller, 0, call_id(1), at(2)),
            (caller, 1, call_id(3), at(9))
        ]
    );
}

#[test]
fn failed_step_keeps_confirmed_prefix_and_failure_path_settlement() {
    let (mut driver, ids) = world(1, 2, &["caller", "target"], |name| match name {
        "caller" => Scripted {
            script: [call(2, 1, on("crash", "target"))].into(),
            ..Scripted::default()
        },
        "target" => Scripted {
            fail_on_crash: true,
            ..Scripted::default()
        },
        _ => Scripted::default(),
    });
    let target = ids[2];
    let mut steps = 0;
    let error = loop {
        steps += 1;
        assert!(steps < 10);
        match driver.step(at(5)) {
            Ok(_) => {}
            Err(error) => break error,
        }
    };
    assert!(matches!(error, Error::Machine { id, .. } if id == target));
    // Not drained between steps: the full confirmed history is retained.
    let events = driver.drain_observations().events;
    assert_dense(&events, 0);
    let ready = events
        .iter()
        .filter(|e| matches!(&e.kind, EventKind::Output(o) if o.payload == Response::Ready))
        .count();
    assert_eq!(ready, 3);
    assert!(events.iter().any(|e| matches!(&e.kind,
        EventKind::Output(o) if matches!(o.payload, Response::SimulationControl { .. }))));
    let tail: Vec<_> = events.iter().rev().take(2).rev().collect();
    let EventKind::FaultAdmitted(record) = &tail[0].kind else {
        panic!("{:?}", tail[0]);
    };
    assert_eq!(record.action, FaultAction::Crash);
    let EventKind::FaultSettled {
        record: settled,
        status: SettlementStatus::Failed,
        outcome_at: None,
        ..
    } = &tail[1].kind
    else {
        panic!("{:?}", tail[1]);
    };
    assert_eq!(settled, record);
    assert!(
        transitions(&events).is_empty(),
        "an uncertain effect is not a change"
    );
    assert!(matches!(driver.step(at(5)), Err(Error::Failed)));
    assert!(driver.drain_observations().events.is_empty());
}

#[test]
fn successful_addition_survives_its_callback_error() {
    let (mut driver, _) = world(10, 1, &[], |_| Scripted::default());
    driver.drain_observations();
    let mut added = None;
    let result = driver.with_barrier(|b| {
        added = Some(b.add(|| Ok(Scripted::default()))?);
        assert!(b.add(|| Err(io::Error::other("no"))).is_err());
        Err::<(), _>(Error::Stopped)
    });
    assert!(matches!(result, Err(Error::Stopped)));
    let events = driver.drain_observations().events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, 1);
    assert!(matches!(events[0].kind,
        EventKind::MachineAdded { machine, cause: Cause::Host, .. } if Some(machine) == added));
}

#[test]
fn full_streams_do_not_depend_on_worker_count() {
    let faults = fault_world(1).0;
    let triggers = trigger_world(1).0;
    for workers in [2, 4] {
        assert_eq!(fault_world(workers).0, faults);
        assert_eq!(trigger_world(workers).0, triggers);
    }
}
