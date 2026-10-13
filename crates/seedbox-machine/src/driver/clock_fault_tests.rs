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
