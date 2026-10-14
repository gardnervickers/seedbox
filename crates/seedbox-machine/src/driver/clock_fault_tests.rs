//! Clock faults through guest control: drift as a Driver-applied rate,
//! realtime steps as guest commands, their handles, events and determinism.
use super::*;
use std::sync::Mutex;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Observed {
    /// (boot, step) for every acknowledged realtime step.
    steps: Vec<(u64, i64)>,
    replies: Vec<serde_json::Value>,
    /// (boot, bytes) drawn from the machine's own RNG at each boot's start.
    entropy: Vec<(u64, [u8; 8])>,
}

/// Emits scripted control calls at machine times, acknowledges realtime
/// steps, and models one guest kernel per boot.
#[derive(Clone)]
struct Guest {
    script: VecDeque<(u64, String)>,
    request: u32,
    ready: bool,
    boot: u64,
    exited: bool,
    observed: Arc<Mutex<Observed>>,
}
impl Machine for Guest {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let start = c.clock.now();
        let emit = |c: &mut RunContext<'_, ()>, payload| {
            c.outbox
                .emit(Output {
                    at: at(start),
                    payload,
                })
                .unwrap();
        };
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => {
                    self.exited = true;
                }
                Request::Reboot => {
                    self.exited = false;
                    self.ready = false;
                    self.boot += 1;
                }
                Request::StepRealtime { id, nanoseconds } => {
                    self.observed
                        .lock()
                        .unwrap()
                        .steps
                        .push((self.boot, nanoseconds.get()));
                    emit(
                        c,
                        Response::CommandAcknowledged {
                            id,
                            result: crate::pv::CommandResult::Completed,
                        },
                    );
                }
                Request::SimulationReply { payload, .. } => self
                    .observed
                    .lock()
                    .unwrap()
                    .replies
                    .push(serde_json::from_slice(&payload).unwrap()),
                _ => {}
            }
        }
        if self.exited {
            return Ok(StopReason::Exited {
                reached: at(start),
                reason: ExitReason::Crashed,
            });
        }
        if !self.ready {
            if upto.as_nanos() == start {
                // Boot work remains runnable.
                return Ok(StopReason::BoundaryReached);
            }
            self.ready = true;
            let mut bytes = [0; 8];
            c.rng.fill_bytes(&mut bytes);
            self.observed
                .lock()
                .unwrap()
                .entropy
                .push((self.boot, bytes));
            emit(c, Response::Ready);
        }
        if let Some(&(due, _)) = self
            .script
            .front()
            .filter(|(due, _)| *due <= upto.as_nanos())
        {
            let (_, call) = self.script.pop_front().unwrap();
            self.request += 1;
            let payload = format!(r#"{{"version":1,"id":{},{call}}}"#, self.request).into_bytes();
            c.outbox
                .emit(Output {
                    at: at(due.max(start)),
                    payload: Response::SimulationControl {
                        id: crate::pv::SimulationControlId {
                            session: NonZeroU32::new(1).unwrap(),
                            request: NonZeroU32::new(self.request).unwrap(),
                        },
                        payload,
                    },
                })
                .unwrap();
            return Ok(StopReason::Blocked {
                reached: at(due.max(start)),
                deadline: self
                    .script
                    .front()
                    .map(|&(t, _)| at(t.max(due.max(start) + 1))),
            });
        }
        Ok(StopReason::Blocked {
            reached: at(start),
            deadline: self.script.front().map(|&(t, _)| at(t)),
        })
    }
    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn scenario(workers: usize, seed: u64) -> (Vec<Event>, Observed) {
    let oracle = Arc::new(Mutex::new(Observed::default()));
    let target = Arc::new(Mutex::new(Observed::default()));
    let t = r#""target":{"machine":"fixture-1"}"#;
    let script = [
        (
            100,
            format!(r#""op":"clock_drift",{t},"rate_ppb":100000000"#),
        ),
        (200, format!(r#""op":"clock_skew",{t},"offset_ns":-5000"#)),
        (300, format!(r#""op":"clock_drift",{t},"rate_ppb":5"#)),
        (400, format!(r#""op":"crash",{t}"#)),
        (500, format!(r#""op":"reboot",{t}"#)),
        (600, r#""op":"heal","handle":"p2""#.to_string()),
        (700, r#""op":"heal","handle":"p1""#.to_string()),
        (
            800,
            format!(r#""op":"clock_skew",{t},"max_offset_ns":1000000"#),
        ),
        (
            900,
            format!(r#""op":"clock_drift",{t},"max_rate_ppb":1000"#),
        ),
    ];
    let machines = vec![
        Guest {
            script: script.into_iter().collect(),
            request: 0,
            ready: false,
            boot: 0,
            exited: false,
            observed: oracle.clone(),
        },
        Guest {
            script: VecDeque::new(),
            request: 0,
            ready: false,
            boot: 0,
            exited: false,
            observed: target.clone(),
        },
    ];
    let mut driver = fixture::driver(
        at(0),
        Duration::from_nanos(50),
        workers,
        crate::rng::SeededRng::new(seed),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap();
    let mut events = driver.drain_events();
    while driver.now() < at(2_000) {
        if matches!(driver.step(at(2_000)).unwrap(), StepOutcome::Blocked { .. }) {
            break;
        }
        events.extend(driver.drain_events());
    }
    events.extend(driver.drain_events());
    let mut observed = target.lock().unwrap().clone();
    observed.replies = oracle.lock().unwrap().replies.clone();
    (events, observed)
}

#[test]
fn drift_and_steps_follow_handles_across_reboot_with_resolved_events() {
    let (events, observed) = scenario(1, 7);
    let admitted: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(record) => Some(record.action),
            _ => None,
        })
        .collect();
    let (skew, drift) = match admitted[6..] {
        [FaultAction::StepRealtime(skew), FaultAction::Drift(drift)] => (skew, drift),
        _ => panic!("{admitted:?}"),
    };
    // The second drift (rate 5) is refused at admission: drifts never stack.
    assert_eq!(
        admitted[..6],
        [
            FaultAction::Drift(ClockRate::new(100_000_000).unwrap()),
            FaultAction::StepRealtime(-5000),
            FaultAction::Crash,
            FaultAction::Reboot,
            FaultAction::Heal,
            FaultAction::Heal,
        ]
    );
    assert!(skew.unsigned_abs() <= 1_000_000 && drift.ppb().unsigned_abs() <= 1000);
    let refused = &observed.replies[2];
    assert_eq!(refused["error"]["code"], "invalid_state");
    // Steps reach the guest of their boot; healing a lost step sends nothing.
    assert_eq!(observed.steps, [(0, -5000), (1, skew)]);
    let settled: Vec<_> = fixture::settlements(&events)
        .into_iter()
        .map(|(record, status, _, _)| (record.action, status))
        .collect();
    assert!(settled
        .iter()
        .all(|(_, status)| *status == SettlementStatus::Applied));
    let healed_step = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultSettled { record, detail, .. }
                if record.action == FaultAction::Heal
                    && record.handle.as_ref().map(ToString::to_string).as_deref() == Some("p2") =>
            {
                Some(detail.clone())
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(
        healed_step,
        "realtime step ended with its machine incarnation"
    );
    // Drift survives the reboot until healed, then nominal; transitions are faults.
    let rates: Vec<_> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Transition {
                change: Transition::ClockRate(rate),
                cause: Cause::Fault(_),
                machine: MachineId(1),
                ..
            } => Some((e.at.as_nanos(), rate.ppb())),
            _ => None,
        })
        .collect();
    assert_eq!(rates, [(100, 100_000_000), (700, 0), (900, drift.ppb())]);
    let replies: Vec<_> = observed.replies.iter().map(|r| &r["result"]).collect();
    assert_eq!(
        replies[0],
        &serde_json::json!({"handle":"p1","rate_ppb":100_000_000})
    );
    assert_eq!(
        replies[1],
        &serde_json::json!({"handle":"p2","offset_ns":-5000})
    );
    assert_eq!(replies[7]["offset_ns"], skew);
    assert_eq!(replies[8]["rate_ppb"], drift.ppb());
}

#[test]
fn seeded_draws_and_translated_streams_are_independent_of_workers() {
    let serial = scenario(1, 7);
    assert_eq!(serial, scenario(2, 7));
    assert_eq!(serial, scenario(4, 7));
    // Draws come from the simulation RNG: another seed draws differently.
    assert_ne!(serial.1.steps, scenario(1, 8).1.steps);
}

/// A guest-free oracle and a stepping target driven only by host inputs.
fn host_driver(workers: usize) -> (Driver<Guest>, Arc<Mutex<Observed>>) {
    let target = Arc::new(Mutex::new(Observed::default()));
    let guest = |observed| Guest {
        script: VecDeque::new(),
        request: 0,
        ready: false,
        boot: 0,
        exited: false,
        observed,
    };
    let machines = vec![guest(Arc::default()), guest(target.clone())];
    let driver = fixture::driver(
        at(0),
        Duration::from_nanos(50),
        workers,
        crate::rng::SeededRng::new(7),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap();
    (driver, target)
}
fn key(text: &str) -> InputKey {
    InputKey::new(text).unwrap()
}
fn target() -> Target {
    Target::Machine(MachineId(1))
}
fn step_action(nanoseconds: i64) -> Action {
    Action::StepRealtime {
        target: target(),
        nanoseconds: std::num::NonZeroI64::new(nanoseconds).unwrap(),
    }
}
fn drift_action(ppb: i64) -> Action {
    Action::Drift {
        target: target(),
        rate: ClockRate::new(ppb).unwrap(),
    }
}
fn advance(driver: &mut Driver<Guest>, end: u64, events: &mut Vec<Event>) {
    while driver.now() < at(end) {
        if matches!(driver.step(at(end)).unwrap(), StepOutcome::Blocked { .. }) {
            break;
        }
        events.extend(driver.drain_events());
    }
    events.extend(driver.drain_events());
}

fn host_scenario(workers: usize) -> (Vec<Event>, Observed) {
    let (mut driver, observed) = host_driver(workers);
    let mut events = driver.drain_events();
    driver
        .with_barrier(|b| {
            b.schedule(
                at(100),
                key("drift"),
                Action::Drift {
                    target: Target::Name("fixture-1".into()),
                    rate: ClockRate::new(100_000_000).unwrap(),
                },
            )?;
            b.schedule(at(100), key("again"), drift_action(5))?;
            b.schedule(at(200), key("back"), step_action(-2_000))?;
            b.schedule(
                at(400),
                key("heal-back"),
                Action::Heal(HealTarget::Input(key("back"))),
            )
        })
        .unwrap();
    advance(&mut driver, 300, &mut events);
    // The applied step's handle keeps its key live until healed.
    driver
        .with_barrier(|b| {
            assert!(matches!(
                b.schedule(at(300), key("back"), step_action(1)),
                Err(Error::LiveInputKey(_))
            ));
            Ok(())
        })
        .unwrap();
    advance(&mut driver, 450, &mut events);
    driver
        .with_barrier(|b| {
            // Healed: the key is free again.
            b.schedule(at(500), key("back"), step_action(7))?;
            b.schedule(
                at(600),
                key("heal-drift"),
                Action::Heal(HealTarget::Input(key("drift"))),
            )
        })
        .unwrap();
    advance(&mut driver, 1_000, &mut events);
    let observed = observed.lock().unwrap().clone();
    (events, observed)
}

#[test]
fn host_drift_step_and_heal_follow_keys_and_publish_resolved_payloads() {
    let (events, observed) = host_scenario(1);
    assert_eq!(observed.steps, [(0, -2_000), (0, 2_000), (0, 7)]);
    let admitted: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultAdmitted(record) => Some((
                e.at.as_nanos(),
                match &record.origin {
                    FaultOrigin::Input { key } => key.as_str().to_owned(),
                    other => panic!("{other:?}"),
                },
                record.action,
                record.handle.as_ref().map(ToString::to_string),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        admitted,
        [
            (
                100,
                "drift".into(),
                FaultAction::Drift(ClockRate::new(100_000_000).unwrap()),
                Some("p1".into())
            ),
            // The refused duplicate drift consumed p2, as a throttle would.
            (
                200,
                "back".into(),
                FaultAction::StepRealtime(-2_000),
                Some("p3".into())
            ),
            (
                400,
                "heal-back".into(),
                FaultAction::Heal,
                Some("p3".into())
            ),
            (
                500,
                "back".into(),
                FaultAction::StepRealtime(7),
                Some("p4".into())
            ),
            (
                600,
                "heal-drift".into(),
                FaultAction::Heal,
                Some("p1".into())
            ),
        ]
    );
    // Drifts never stack: the second is refused at admission with its key.
    let rejected: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::InputRejected { key, rejection } => {
                Some((key.as_str().to_owned(), rejection.reason))
            }
            _ => None,
        })
        .collect();
    assert_eq!(rejected, [("again".into(), RejectionReason::InvalidState)]);
    assert!(fixture::settlements(&events)
        .iter()
        .all(|(_, status, _, _)| *status == SettlementStatus::Applied));
    let rates: Vec<_> = events
        .iter()
        .filter_map(|e| match e.kind {
            EventKind::Transition {
                change: Transition::ClockRate(rate),
                cause: Cause::Fault(_),
                ..
            } => Some((e.at.as_nanos(), rate.ppb())),
            _ => None,
        })
        .collect();
    assert_eq!(rates, [(100, 100_000_000), (600, 0)]);
}

#[test]
fn host_clock_inputs_are_independent_of_workers() {
    let serial = host_scenario(1);
    assert_eq!(serial, host_scenario(2));
    assert_eq!(serial, host_scenario(4));
}

#[test]
fn host_realtime_steps_are_bounded_at_enqueue_without_mutation() {
    let (mut driver, observed) = host_driver(1);
    let bound = crate::oracle::MAX_CLOCK_OFFSET_NS as i64;
    let before = driver.drain_events();
    driver
        .with_barrier(|b| {
            for outside in [i64::MIN, -bound - 1, bound + 1, i64::MAX] {
                assert!(matches!(
                    b.schedule(at(10), key("step"), step_action(outside)),
                    Err(Error::InvalidClockStep(n)) if n.get() == outside
                ));
            }
            // Nothing was queued or reserved: the key is still free.
            b.schedule(at(10), key("step"), step_action(-bound))
        })
        .unwrap();
    assert!(driver.drain_events().is_empty() && !before.is_empty());
    let mut events = Vec::new();
    advance(&mut driver, 100, &mut events);
    let first = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultAdmitted(record) => Some(record.fault),
            _ => None,
        })
        .unwrap();
    assert_eq!(first, FaultId(1), "rejected enqueues allocated no fault ID");
    assert_eq!(observed.lock().unwrap().steps, [(0, -bound)]);
}

#[test]
fn host_clock_effects_hold_before_applying_and_bind_their_boot() {
    let (mut driver, observed) = host_driver(1);
    driver
        .with_barrier(|b| {
            b.schedule(at(100), key("drift"), drift_action(100_000_000))?;
            b.schedule(at(300), key("crash"), Action::Crash(target()))?;
            b.schedule(at(300), key("late"), step_action(5))?;
            b.pause_before_next_fault(PlaybackRate::Unlimited)
        })
        .unwrap();
    let mut events = driver.drain_events();
    let held = loop {
        let outcome = driver.step(at(1_000)).unwrap();
        events.extend(driver.drain_events());
        if let StepOutcome::BeforeFault { at: when, fault } = outcome {
            break (when, fault);
        }
    };
    assert_eq!(held.0, at(100));
    assert_eq!(
        held.1.action,
        FaultAction::Drift(ClockRate::new(100_000_000).unwrap())
    );
    // Held before the rate changed.
    assert!(!events.iter().any(|e| matches!(
        e.kind,
        EventKind::Transition {
            change: Transition::ClockRate(_),
            ..
        }
    )));
    driver
        .with_barrier(|b| b.set_rate(PlaybackRate::Unlimited))
        .unwrap();
    advance(&mut driver, 1_000, &mut events);
    // The step admitted after the crash's admission is bound to the crashed
    // boot: it settles without reaching any guest.
    let late = fixture::settlements(&events)
        .into_iter()
        .find(|(record, ..)| record.action == FaultAction::StepRealtime(5))
        .unwrap();
    assert_eq!(late.1, SettlementStatus::Unavailable);
    assert!(observed.lock().unwrap().steps.is_empty());
}

/// Seeded clock faults draw from each target's own clock-fault stream, derived
/// from its machine seed: they never shift later machine seeds, a machine's
/// own RNG, or another target's draws.
fn isolation(workers: usize, perturb: bool) -> Vec<Observed> {
    let observed: Vec<_> = (0..4)
        .map(|_| Arc::new(Mutex::new(Observed::default())))
        .collect();
    let skew = |machine: usize| {
        format!(
            r#""op":"clock_skew","target":{{"machine":"fixture-{machine}"}},"max_offset_ns":1000000"#
        )
    };
    let mut script = Vec::new();
    if perturb {
        script.extend([(100, skew(1)), (150, skew(1))]);
    }
    script.push((200, skew(2)));
    let mut scripts = [script, vec![], vec![], vec![]];
    let observers = observed.clone();
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(50),
        workers,
        crate::rng::SeededRng::new(11),
        || Ok(()),
        fixture::definition(0),
        vec![fixture::definition(1), fixture::definition(2)],
        vec![fixture::definition(3)],
        move |definition| {
            let index: usize = definition
                .name
                .strip_prefix("fixture-")
                .unwrap()
                .parse()
                .unwrap();
            Ok(Guest {
                script: std::mem::take(&mut scripts[index]).into_iter().collect(),
                request: 0,
                ready: false,
                boot: 0,
                exited: false,
                observed: observers[index].clone(),
            })
        },
    )
    .unwrap();
    driver
        .with_barrier(|b| {
            b.schedule(
                at(300),
                InputKey::new("spawn").unwrap(),
                Action::Spawn {
                    name: "fixture-3".into(),
                },
            )
        })
        .unwrap();
    while driver.now() < at(1_000) {
        if matches!(driver.step(at(1_000)).unwrap(), StepOutcome::Blocked { .. }) {
            break;
        }
    }
    observed.iter().map(|o| o.lock().unwrap().clone()).collect()
}

#[test]
fn clock_fault_draws_do_not_perturb_seeds_entropy_or_other_targets() {
    for workers in [1, 2, 4] {
        let base = isolation(workers, false);
        let perturbed = isolation(workers, true);
        assert_eq!(base, isolation(1, false), "workers {workers}");
        // Target 1 was stepped twice more; target 2's draw is unchanged.
        assert_eq!(perturbed[1].steps.len(), 2);
        assert_eq!(base[2].steps, perturbed[2].steps);
        assert_eq!(base[2].steps.len(), 1);
        // Every machine's own entropy, including the later spawn's seed, is
        // unchanged by the extra clock-fault draws.
        for machine in 0..4 {
            assert_eq!(
                base[machine].entropy, perturbed[machine].entropy,
                "{machine}"
            );
            assert_eq!(base[machine].entropy.len(), 1);
        }
    }
}
