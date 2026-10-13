//! Declared creation uses the control FIFO, not host construction or worker order.
use super::*;
use crate::{
    pv::{Output, SimulationControlId, Stream},
    rng::RngStreamExt,
};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn call(n: u32, fields: &str) -> Response {
    Response::SimulationControl {
        id: SimulationControlId {
            session: NonZeroU32::new(1).unwrap(),
            request: NonZeroU32::new(n).unwrap(),
        },
        payload: format!("{{\"version\":1,\"id\":{n},{fields}}}").into_bytes(),
    }
}
fn spawn(n: u32, name: &str) -> Response {
    call(n, &format!(r#""op":"spawn","machine":"{name}""#))
}
#[derive(Clone, Default)]
struct Probe {
    name: String,
    rounds: VecDeque<Vec<Response>>,
    replies: Vec<serde_json::Value>,
    random: Option<u64>,
    crashed: bool,
    calls: Vec<(u64, u64, u32)>,
}
impl Machine for Probe {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = at(c.clock.now());
        self.calls.push((
            now.as_nanos(),
            upto.as_nanos(),
            c.execution.factor_permille(),
        ));
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => self.crashed = true,
                Request::Reboot => self.crashed = false,
                Request::SimulationReply { payload, .. } => {
                    self.replies.push(serde_json::from_slice(&payload).unwrap())
                }
                _ => {}
            }
        }
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: now,
                reason: ExitReason::Crashed,
            });
        }
        if upto > now {
            if self.random.is_none() {
                let mut bytes = [0; 8];
                c.rng.fill_bytes(&mut bytes);
                self.random = Some(u64::from_le_bytes(bytes));
                c.outbox
                    .emit(Output {
                        at: now,
                        payload: Response::ApplicationOutput {
                            stream: Stream::Stdout,
                            bytes: self.name.as_bytes().to_vec(),
                        },
                    })
                    .unwrap();
            }
            for payload in self.rounds.pop_front().unwrap_or_default() {
                c.outbox.emit(Output { at: now, payload }).unwrap();
            }
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
struct CountingRng {
    inner: SeededRng,
    draws: Arc<AtomicUsize>,
}
impl RngStream for CountingRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        assert_eq!(bytes.len(), 8);
        self.draws.fetch_add(1, Ordering::SeqCst);
        self.inner.fill_bytes(bytes);
    }
}
fn build(
    workers: usize,
    rounds: Vec<Vec<Response>>,
    fail: Option<&'static str>,
) -> (Driver<Probe>, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
    let draws = Arc::new(AtomicUsize::new(0));
    let built = Arc::new(Mutex::new(vec![]));
    let captured = built.clone();
    let mut oracle_rounds = Some(rounds);
    let driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        workers,
        CountingRng {
            inner: SeededRng::new(123),
            draws: draws.clone(),
        },
        || Ok(()),
        fixture::definition(0),
        vec![],
        (1..=3).map(fixture::definition).collect(),
        move |definition| {
            captured.lock().unwrap().push(definition.name.clone());
            if fail == Some(definition.name.as_str()) {
                return Err(std::io::Error::other("spawn fixture construction failure"));
            }
            Ok(Probe {
                name: definition.name.clone(),
                rounds: if definition.name == "fixture-0" {
                    oracle_rounds.take().unwrap().into()
                } else {
                    VecDeque::new()
                },
                ..Default::default()
            })
        },
    )
    .unwrap();
    (driver, draws, built)
}
#[test]
fn oracle_only_spawn_ids_seeds_names_and_events_follow_admission_not_definition_order() {
    let run = |workers| {
        let (mut driver, draws, built) = build(
            workers,
            vec![vec![spawn(1, "fixture-2"), spawn(2, "fixture-1")]],
            None,
        );
        assert_eq!(draws.load(Ordering::SeqCst), 1);
        assert_eq!(*built.lock().unwrap(), ["fixture-0"]);
        driver.step(at(10)).unwrap();
        assert_eq!(draws.load(Ordering::SeqCst), 3);
        assert_eq!(
            *built.lock().unwrap(),
            ["fixture-0", "fixture-2", "fixture-1"]
        );
        assert!(driver.machines[1..].iter().all(|e| e.boot == 0
            && e.reached == at(10)
            && !e.paused
            && e.execution == ExecutionConfig::default()
            && e.machine.as_ref().unwrap().calls.is_empty()));
        let mut events = driver.drain_events();
        for (fault, id, name) in [(1, 1, "fixture-2"), (2, 2, "fixture-1")] {
            let added = events.iter().position(|e| matches!(e.kind, EventKind::MachineAdded { machine, cause: Cause::Fault(f) } if machine == MachineId(id) && f == FaultId(fault))).unwrap();
            let settled = events.iter().position(|e| matches!(&e.kind, EventKind::FaultSettled { record, status: SettlementStatus::Applied, .. } if record.fault == FaultId(fault) && record.target == MachineId(id) && record.boot == 0 && record.action == FaultAction::Spawn)).unwrap();
            let reply = events.iter().position(|e| matches!(e.kind, EventKind::ReplyAdmitted { id, .. } if id.request.get() == fault)).unwrap();
            assert!(added < settled && settled < reply);
            assert_eq!(
                driver.machines[0].machine.as_ref().unwrap().replies[fault as usize - 1]["result"],
                serde_json::json!({"machine":name})
            );
        }
        driver.step(at(20)).unwrap();
        events.extend(driver.drain_events());
        let mut global = SeededRng::new(123);
        for e in &driver.machines {
            let mut expected = SeededRng::new(global.next_u64());
            assert_eq!(
                e.machine.as_ref().unwrap().random,
                Some(expected.next_u64())
            );
        }
        let last_reply = events
            .iter()
            .rposition(|e| matches!(e.kind, EventKind::ReplyAdmitted { .. }))
            .unwrap();
        let first_output = events
            .iter()
            .position(|e| matches!(&e.kind, EventKind::Output(o) if o.machine != MachineId(0)))
            .unwrap();
        assert!(last_reply < first_output);
        events
    };
    let serial = run(1);
    assert_eq!(serial, run(2));
    assert_eq!(serial, run(4));
}
#[test]
fn unknown_duplicate_and_already_spawned_rejections_allocate_nothing() {
    let (mut driver, draws, _) = build(
        2,
        vec![
            vec![
                spawn(1, "unknown"),
                spawn(2, "fixture-0"),
                spawn(3, "fixture-2"),
                spawn(4, "fixture-2"),
                spawn(5, "fixture-1"),
            ],
            vec![spawn(6, "fixture-2")],
        ],
        None,
    );
    driver.step(at(10)).unwrap();
    driver.step(at(20)).unwrap();
    assert_eq!(draws.load(Ordering::SeqCst), 3);
    assert_eq!(driver.machines.len(), 3);
    let records: Vec<_> = driver
        .drain_events()
        .into_iter()
        .filter_map(|e| {
            if let EventKind::FaultAdmitted(r) = e.kind {
                Some(r)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        records
            .iter()
            .map(|r| (r.fault, r.target))
            .collect::<Vec<_>>(),
        [(FaultId(1), MachineId(1)), (FaultId(2), MachineId(2))]
    );
    let replies = &driver.machines[0].machine.as_ref().unwrap().replies;
    assert_eq!(replies.len(), 6);
    for n in [1, 2, 4, 6] {
        let reply = replies.iter().find(|r| r["id"] == n).unwrap();
        assert_eq!(reply["ok"], false);
        assert!(reply["effect_at_ns"].is_null());
    }
}
#[test]
fn duplicate_definitions_across_all_lists_fail_before_factories_or_rng() {
    for (initial, spawnable) in [
        (vec![0], vec![]),
        (vec![], vec![0]),
        (vec![1, 1], vec![]),
        (vec![1], vec![1]),
        (vec![], vec![1, 1]),
    ] {
        let draws = Arc::new(AtomicUsize::new(0));
        let result = Driver::<Probe>::new(
            at(0),
            Duration::from_nanos(10),
            1,
            CountingRng {
                inner: SeededRng::new(0),
                draws: draws.clone(),
            },
            || panic!("duplicate invoked context factory"),
            fixture::definition(0),
            initial.into_iter().map(fixture::definition).collect(),
            spawnable.into_iter().map(fixture::definition).collect(),
            |_| panic!("duplicate invoked machine factory"),
        );
        assert!(matches!(result, Err(Error::DuplicateName(_))));
        assert_eq!(draws.load(Ordering::SeqCst), 0);
    }
}
#[test]
fn spawned_target_supports_throttle_crash_reboot_and_snapshot() {
    let (mut driver, draws, _) = build(
        4,
        vec![
            vec![spawn(1, "fixture-1")],
            vec![call(
                2,
                r#""op":"throttle","target":{"machine":"fixture-1"},"factor_permille":2000"#,
            )],
            vec![call(3, r#""op":"crash","target":{"machine":"fixture-1"}"#)],
            vec![call(4, r#""op":"reboot","target":{"machine":"fixture-1"}"#)],
        ],
        None,
    );
    driver.step(at(10)).unwrap();
    let snapshot = driver.with_barrier(|b| b.snapshot(MachineId(1))).unwrap();
    assert_eq!(snapshot.name, "fixture-1");
    assert!(snapshot.calls.is_empty());
    driver.step(at(20)).unwrap();
    assert_eq!(driver.machines[1].execution.factor_permille(), 2000);
    driver.step(at(30)).unwrap();
    assert!(matches!(
        driver.machines[1].stopped,
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    driver.step(at(40)).unwrap();
    assert_eq!(driver.machines[1].boot, 1);
    assert!(!driver.machines[1].paused);
    assert_eq!(driver.machines[1].execution.factor_permille(), 2000);
    assert_eq!(draws.load(Ordering::SeqCst), 2);
    assert!(driver.machines[0]
        .machine
        .as_ref()
        .unwrap()
        .replies
        .iter()
        .all(|r| r["ok"] == true));
    assert_eq!(fixture::settlements(&driver.drain_events()).len(), 4);
}
#[test]
fn construction_failure_publishes_failed_and_preserves_confirmed_prefix_without_retry() {
    let (mut driver, draws, built) = build(
        2,
        vec![vec![
            spawn(1, "fixture-2"),
            spawn(2, "fixture-1"),
            spawn(3, "fixture-3"),
        ]],
        Some("fixture-1"),
    );
    assert!(matches!(driver.step(at(10)), Err(Error::Construction(_))));
    assert_eq!(draws.load(Ordering::SeqCst), 2);
    assert_eq!(
        *built.lock().unwrap(),
        ["fixture-0", "fixture-2", "fixture-1"]
    );
    let events = driver.drain_events();
    let settlements = fixture::settlements(&events);
    assert_eq!(settlements.len(), 2);
    assert_eq!(settlements[0].1, SettlementStatus::Applied);
    assert_eq!(settlements[1].1, SettlementStatus::Failed);
    assert_eq!(settlements[1].0.target, MachineId(2));
    assert_eq!(settlements[1].0.boot, 0);
    assert_eq!(settlements[1].0.action, FaultAction::Spawn);
    assert!(events.iter().any(|e| matches!(
        e.kind,
        EventKind::MachineAdded {
            machine: MachineId(1),
            cause: Cause::Fault(FaultId(1))
        }
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e.kind, EventKind::ReplyAdmitted { .. })));
    assert_eq!(fixture::outputs(&events).len(), 4);
    assert!(matches!(driver.step(at(20)), Err(Error::Failed)));
    assert!(driver.drain_events().is_empty());
}
#[test]
fn final_batch_and_stopped_pending_calls_never_create_or_draw_seed() {
    let (mut driver, draws, built) = build(
        1,
        vec![vec![
            spawn(1, "fixture-1"),
            Response::ApplicationOutcome { code: 0 },
        ]],
        None,
    );
    let finished = driver.step(at(10)).unwrap();
    assert!(matches!(finished, StepOutcome::Finished { .. }));
    assert_eq!(driver.step(at(20)).unwrap(), finished);
    assert_eq!(draws.load(Ordering::SeqCst), 1);
    assert_eq!(*built.lock().unwrap(), ["fixture-0"]);
    assert!(fixture::settlements(&driver.drain_events()).is_empty());
    let (mut driver, draws, built) = build(1, vec![], None);
    driver
        .events
        .entry(at(0))
        .or_default()
        .push_back(Scheduled::Control(control::Event::Call {
            source: MachineId(0),
            boot: 0,
            id: SimulationControlId {
                session: NonZeroU32::new(1).unwrap(),
                request: NonZeroU32::new(1).unwrap(),
            },
            observed: 0,
            payload: br#"{"version":1,"id":1,"op":"spawn","machine":"fixture-1"}"#.to_vec(),
        }));
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
    assert_eq!(draws.load(Ordering::SeqCst), 1);
    assert_eq!(*built.lock().unwrap(), ["fixture-0"]);
}
#[test]
fn finish_and_stop_cut_off_already_admitted_spawn_effects() {
    let (mut driver, draws, built) = build(
        1,
        vec![vec![
            call(1, r#""op":"crash","target":{"machine":"fixture-0"}"#),
            spawn(2, "fixture-1"),
        ]],
        None,
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished {
            at: at(10),
            reason: ExitReason::Crashed
        }
    );
    let events = driver.drain_events();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::FaultAdmitted(r) if r.action == FaultAction::Spawn && r.target == MachineId(1))));
    assert_eq!(draws.load(Ordering::SeqCst), 1);
    assert_eq!(*built.lock().unwrap(), ["fixture-0"]);
    assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::FaultSettled { record, .. } if record.action == FaultAction::Spawn)));
    let (mut driver, draws, built) = build(1, vec![], None);
    let Response::SimulationControl { id, payload } = spawn(1, "fixture-1") else {
        unreachable!()
    };
    driver.control.record_call(MachineId(0), id, 0);
    let plans = driver
        .control
        .invoke_plans(|service, view| {
            service.admit(
                view,
                control::Control::source(MachineId(0)),
                id,
                0,
                0,
                &payload,
            )
        })
        .unwrap();
    driver.control_plans(plans).unwrap();
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
    assert_eq!(draws.load(Ordering::SeqCst), 1);
    assert_eq!(*built.lock().unwrap(), ["fixture-0"]);
    let events = driver.drain_events();
    assert!(events
        .iter()
        .any(|e| matches!(&e.kind, EventKind::FaultAdmitted(r) if r.action == FaultAction::Spawn)));
    assert!(fixture::settlements(&events).is_empty());
}

#[test]
fn spawned_machine_has_equal_control_access() {
    let (mut driver, draws, _) = build(2, vec![vec![spawn(1, "fixture-1")]], None);
    driver.step(at(10)).unwrap();
    driver.machines[1]
        .machine
        .as_mut()
        .unwrap()
        .rounds
        .push_back(vec![spawn(1, "fixture-2")]);
    driver.step(at(20)).unwrap();
    assert_eq!(draws.load(Ordering::SeqCst), 3);
    assert_eq!(
        driver.machines[2].machine.as_ref().unwrap().name,
        "fixture-2"
    );
    assert_eq!(
        driver.machines[1].machine.as_ref().unwrap().replies[0]["ok"],
        true
    );
}
