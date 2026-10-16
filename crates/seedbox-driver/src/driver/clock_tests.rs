//! Driver translation between simulation time and per-machine time bases.
//! Machines here see only their own time base; expected values are computed
//! independently from the exact rational rate.
use super::*;
use std::sync::Mutex;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn rate(ppb: i64) -> ClockRate {
    ClockRate::new(ppb).unwrap()
}
/// floor(G(t)) for a single rate from `anchor`, with G(anchor) = base exactly.
fn mapped(anchor: u64, base: u64, ppb: i64, t: u64) -> u64 {
    let exact = i128::from(base) * 1_000_000_000
        + i128::from(t - anchor) * (1_000_000_000 + i128::from(ppb));
    (exact / 1_000_000_000) as u64
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Call {
    now: u64,
    upto: u64,
    inputs: Vec<u64>,
}
type Calls = Arc<Mutex<Vec<Call>>>;

/// Fires at most one machine-time timer per run, emitting its index at the
/// machine time it is observed; a busy machine always reaches its bound.
#[derive(Clone)]
struct Timers {
    timers: VecDeque<u64>,
    fired: u8,
    busy: bool,
    calls: Calls,
}
impl Timers {
    fn new(timers: &[u64], busy: bool) -> (Self, Calls) {
        let calls = Calls::default();
        (
            Self {
                timers: timers.iter().copied().collect(),
                fired: 0,
                busy,
                calls: calls.clone(),
            },
            calls,
        )
    }
}
impl Machine for Timers {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = (VecDeque<u64>, u8);
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let start = context.clock.now();
        let mut inputs = Vec::new();
        while let Some(input) = context.inbox.take() {
            inputs.push(input.at.as_nanos());
            if let Request::Application { id, .. } = input.payload {
                // Arm a relative timer in this machine's own time base.
                let due = start + 1_000;
                let index = self.timers.partition_point(|&t| t <= due);
                self.timers.insert(index, due);
                context
                    .outbox
                    .emit(crate::pv::Output {
                        at: at(start),
                        payload: Response::CommandAcknowledged {
                            id,
                            result: crate::pv::CommandResult::Completed,
                        },
                    })
                    .unwrap();
            }
        }
        self.calls.lock().unwrap().push(Call {
            now: start,
            upto: upto.as_nanos(),
            inputs,
        });
        let mut reached = start;
        if let Some(&due) = self.timers.front() {
            if due <= upto.as_nanos() {
                self.timers.pop_front();
                reached = due.max(start);
                self.fired += 1;
                context
                    .outbox
                    .emit(crate::pv::Output {
                        at: at(reached),
                        payload: Response::BootConsole {
                            bytes: vec![self.fired],
                        },
                    })
                    .unwrap();
            }
        }
        if self.busy {
            return Ok(StopReason::BoundaryReached);
        }
        Ok(StopReason::Blocked {
            reached: at(reached),
            deadline: self.timers.front().map(|&t| at(t)),
        })
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        Ok((self.timers.clone(), self.fired))
    }
}

fn driver(machines: Vec<Timers>, quantum: u64, workers: usize) -> Driver<Timers> {
    fixture::driver(
        at(0),
        Duration::from_nanos(quantum),
        workers,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || Ok(machines),
    )
    .unwrap()
}
/// Public host drift through the unified schedule, keyed by machine and time.
fn set_rate(driver: &mut Driver<Timers>, when: u64, id: usize, ppb: i64) {
    let key = InputKey::new(format!("drift-{id}-{when}")).unwrap();
    let action = Action::Drift {
        target: Target::Machine(MachineId(id)),
        rate: rate(ppb),
    };
    driver
        .with_barrier(|b| b.schedule(at(when), key, action))
        .unwrap();
}
/// Heal the drift that `set_rate(_, set, id, _)` installed: nominal rate.
fn heal_rate(driver: &mut Driver<Timers>, when: u64, id: usize, set: u64) {
    let key = InputKey::new(format!("heal-{id}-{when}")).unwrap();
    let drift = InputKey::new(format!("drift-{id}-{set}")).unwrap();
    driver
        .with_barrier(|b| b.schedule(at(when), key, Action::Heal(HealTarget::Input(drift))))
        .unwrap();
}
fn run_until(driver: &mut Driver<Timers>, end: u64) -> Vec<Event> {
    let mut events = driver.drain_events();
    while driver.now() < at(end) {
        match driver.step(at(end)).unwrap() {
            StepOutcome::Blocked { .. } => break,
            _ => events.extend(driver.drain_events()),
        }
    }
    events.extend(driver.drain_events());
    events
}
/// Fired timer indices with their simulation times, in publication order.
fn fired(events: &[Event]) -> Vec<(u64, usize, u8)> {
    fixture::outputs(events)
        .into_iter()
        .filter_map(|output| match output.payload {
            Response::BootConsole { bytes } => {
                Some((output.at.as_nanos(), output.machine.0, bytes[0]))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn fast_and_slow_timers_fire_at_the_earliest_converted_simulation_time() {
    // +10%: machine 1.1 s is simulation 1 s exactly; one more machine
    // nanosecond needs one more simulation nanosecond (never early).
    // -10%: machine 0.9 s is simulation 1 s; 900_000_001 needs 1_000_000_002.
    let (fast, fast_calls) = Timers::new(&[1_100_000_000, 1_100_000_001], false);
    let (slow, _) = Timers::new(&[900_000_000, 900_000_001], false);
    let (nominal, _) = Timers::new(&[1_000_000_000, 1_000_000_001], false);
    let mut driver = driver(vec![nominal, fast, slow], 250_000_000, 1);
    set_rate(&mut driver, 0, 1, 100_000_000);
    set_rate(&mut driver, 0, 2, -100_000_000);
    let events = run_until(&mut driver, 3_000_000_000);
    assert_eq!(
        fired(&events),
        [
            (1_000_000_000, 0, 1),
            (1_000_000_000, 1, 1),
            (1_000_000_000, 2, 1),
            (1_000_000_001, 0, 2),
            (1_000_000_001, 1, 2),
            (1_000_000_002, 2, 2),
        ]
    );
    // The fast machine's runs started at floor(G) of each simulation barrier.
    let starts: Vec<_> = fast_calls.lock().unwrap().iter().map(|c| c.now).collect();
    assert!(starts.contains(&1_100_000_000) && starts.contains(&1_100_000_001));
    let transitions: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Transition {
                machine,
                change: Transition::ClockRate(rate),
                cause: Cause::Fault(_),
                ..
            } => Some((event.at, *machine, rate.ppb())),
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions,
        [
            (at(0), MachineId(1), 100_000_000),
            (at(0), MachineId(2), -100_000_000)
        ]
    );
}

#[test]
fn every_machine_stops_at_common_barriers_mapped_to_its_own_time() {
    let rates = [0, 100_000_000, -33_333_333, 1];
    let mut machines = Vec::new();
    let mut calls = Vec::new();
    for _ in rates {
        let (machine, recorded) = Timers::new(&[], true);
        machines.push(machine);
        calls.push(recorded);
    }
    let mut driver = driver(machines, 1_000, 1);
    for (id, ppb) in rates.into_iter().enumerate() {
        set_rate(&mut driver, 0, id, ppb);
    }
    // Shorter host bounds create intermediate barriers inside quanta.
    for upto in [700, 1_000, 2_500, 5_000] {
        driver.step(at(upto)).unwrap();
        while driver.now() < at(upto) {
            driver.step(at(upto)).unwrap();
        }
    }
    let nominal: Vec<_> = calls[0].lock().unwrap().clone();
    let barriers: Vec<_> = nominal
        .iter()
        .filter(|call| call.upto > call.now)
        .map(|call| (call.now, call.upto))
        .collect();
    assert_eq!(
        barriers,
        [
            (0, 700),
            (700, 1_000),
            (1_000, 2_000),
            (2_000, 2_500),
            (2_500, 3_000),
            (3_000, 4_000),
            (4_000, 5_000)
        ]
    );
    for (recorded, ppb) in calls.iter().zip(rates) {
        let positive: Vec<_> = recorded
            .lock()
            .unwrap()
            .iter()
            .filter(|call| call.upto > call.now)
            .map(|call| (call.now, call.upto))
            .collect();
        let expected: Vec<_> = barriers
            .iter()
            .map(|&(from, to)| (mapped(0, 0, ppb, from), mapped(0, 0, ppb, to)))
            .collect();
        assert_eq!(positive, expected, "rate {ppb}");
        // Consecutive runs tile the machine's time without gaps or overlap.
        assert!(positive.windows(2).all(|pair| pair[0].1 == pair[1].0));
    }
}

#[test]
fn rate_change_keeps_accrued_offset_and_reconverts_pending_timers() {
    // Machine time 1.1e9 at simulation 1e9, then nominal: machine 3e9 is
    // simulation 1e9 + 1.9e9. Without the heal it would be 3e9 / 1.1.
    let (machine, calls) = Timers::new(&[3_000_000_000], false);
    let mut driver = driver(vec![machine], 1_000_000_000, 1);
    set_rate(&mut driver, 0, 0, 100_000_000);
    heal_rate(&mut driver, 1_000_000_000, 0, 0);
    let events = run_until(&mut driver, 5_000_000_000);
    assert_eq!(fired(&events), [(2_900_000_000, 0, 1)]);
    // The machine was never run for the rate change itself.
    let calls = calls.lock().unwrap();
    assert!(calls.iter().all(|call| call.now != 1_100_000_000));
    // The fractional offset survives a heal at a non-integral position.
    let (machine, _) = Timers::new(&[2_000_000_000], false);
    let mut driver = self::driver(vec![machine], 1_000_000_000, 1);
    set_rate(&mut driver, 0, 0, 1);
    heal_rate(&mut driver, 1_500_000_001, 0, 0);
    // G(1_500_000_001) = 1_500_000_002.500000001; machine 2e9 is reached at
    // 1_500_000_001 + 499_999_997.499999999, rounded up.
    assert_eq!(
        fired(&run_until(&mut driver, 3_000_000_000)),
        [(1_999_999_999, 0, 1)]
    );
}

#[test]
fn paused_machine_adopts_the_new_rate_on_its_zero_budget_resume() {
    let (machine, calls) = Timers::new(&[2_000_000_000], false);
    let mut driver = driver(vec![machine], 1_000_000_000, 1);
    driver
        .with_barrier(|b| {
            let target = || Target::Machine(MachineId(0));
            b.schedule(
                at(0),
                InputKey::new("pause").unwrap(),
                Action::Pause(target()),
            )?;
            // Raw rate change: this test is about the translation of a paused
            // machine's timer, independent of whether control policy admits
            // drift for paused targets (an open policy decision).
            b.schedule_clock_rate(at(1_000_000_000), MachineId(0), rate(100_000_000))?;
            b.schedule(
                at(2_000_000_000),
                InputKey::new("resume").unwrap(),
                Action::Resume(target()),
            )
        })
        .unwrap();
    let events = run_until(&mut driver, 4_000_000_000);
    // Overdue at resume: machine 2.1e9 >= 2e9, delivered at the resume time.
    assert_eq!(fired(&events), [(2_000_000_000, 0, 1)]);
    let calls = calls.lock().unwrap();
    let resume = calls.iter().find(|c| c.now == 2_100_000_000).unwrap();
    assert_eq!(resume.upto, 2_100_000_000, "zero-budget resume");
}

#[test]
fn inputs_are_offered_in_machine_time_and_outputs_return_in_simulation_time() {
    let (machine, calls) = Timers::new(&[], false);
    let mut driver = driver(vec![machine], 1_000_000_000, 1);
    set_rate(&mut driver, 0, 0, 100_000_000);
    // Raw queue entry: private timeline mechanics below the control policy.
    driver
        .events
        .entry(at(1_000_000_000))
        .or_default()
        .push_back(Scheduled::Input {
            id: MachineId(0),
            input: Input {
                at: at(1_000_000_000),
                payload: Request::Application {
                    id: NonZeroU32::new(1).unwrap(),
                    command: crate::pv::ApplicationCommand::Start,
                },
            },
        });
    let events = run_until(&mut driver, 3_000_000_000);
    let calls = calls.lock().unwrap();
    let call = calls.iter().find(|c| !c.inputs.is_empty()).unwrap();
    assert_eq!(
        (call.now, call.inputs.as_slice()),
        (1_100_000_000, &[1_100_000_000][..])
    );
    // The armed timer at machine +1000 ns is simulation +ceil(1000/1.1).
    assert_eq!(fired(&events), [(1_000_000_000 + 910, 0, 1)]);
    let acknowledged = fixture::outputs(&events)
        .into_iter()
        .find(|o| matches!(o.payload, Response::CommandAcknowledged { .. }))
        .unwrap();
    assert_eq!(acknowledged.at, at(1_000_000_000));
}

#[test]
fn fast_timers_due_at_their_own_blocking_time_rerun_without_invalid_stops() {
    // Machine 10 and 11 both first become visible at simulation 10 (G = 11).
    let (machine, calls) = Timers::new(&[10, 11], false);
    let mut driver = driver(vec![machine], 1_000, 1);
    set_rate(&mut driver, 0, 0, 100_000_000);
    let events = run_until(&mut driver, 100);
    assert_eq!(fired(&events), [(10, 0, 1), (10, 0, 2)]);
    let calls = calls.lock().unwrap();
    assert_eq!(calls.last().unwrap().now, 11);
}

#[test]
fn worker_count_never_changes_the_translated_event_stream() {
    let scenario = |workers| {
        let machines = [
            &[1_100_000_000, 2_000_000_000][..],
            &[900_000_000, 950_000_001],
            &[5, 7, 3_000_000_000],
        ]
        .iter()
        .map(|timers| Timers::new(timers, false).0)
        .collect();
        let mut driver = driver(machines, 100_000_000, workers);
        set_rate(&mut driver, 0, 0, 100_000_000);
        set_rate(&mut driver, 0, 1, -100_000_000);
        heal_rate(&mut driver, 1_000_000_000, 0, 0);
        set_rate(&mut driver, 1_000_000_001, 0, -7);
        set_rate(&mut driver, 1_200_000_000, 2, 33);
        run_until(&mut driver, 5_000_000_000)
    };
    let serial = scenario(1);
    assert_eq!(fired(&serial).len(), 7);
    assert_eq!(serial, scenario(2));
    assert_eq!(serial, scenario(4));
}

#[test]
fn snapshots_hold_machine_time_and_the_mapping_is_rebuilt_from_events() {
    let (machine, calls) = Timers::new(&[u64::MAX], true);
    let mut driver = driver(vec![machine], 1_000_000, 1);
    set_rate(&mut driver, 0, 0, 100_000_000);
    heal_rate(&mut driver, 3_000_000, 0, 0);
    set_rate(&mut driver, 3_000_001, 0, -50_000_001);
    let events = run_until(&mut driver, 6_000_000);
    // The machine-owned snapshot is unaffected by the Driver's mapping.
    let snapshot = driver.with_barrier(|b| b.snapshot(MachineId(0))).unwrap();
    assert_eq!(snapshot, (VecDeque::from([u64::MAX]), 0));
    // Rebuild the excluded mapping from its addition and ClockRate events.
    let mut mapping = None;
    for event in &events {
        match event.kind {
            EventKind::MachineAdded { .. } => mapping = Some(Projection::new(event.at.as_nanos())),
            EventKind::Transition {
                change: Transition::ClockRate(rate),
                ..
            } => mapping
                .as_mut()
                .unwrap()
                .set_drift(event.at.as_nanos(), i64::from(rate.ppb()))
                .unwrap(),
            _ => {}
        }
    }
    let mapping = mapping.unwrap();
    let position = calls.lock().unwrap().last().unwrap().upto;
    assert_eq!(mapping.floor(6_000_000), Ok(position));
    assert_eq!(position, 3_300_001 + mapped(0, 0, -50_000_001, 2_999_999));
}

#[test]
fn unrepresentable_machine_times_fail_or_never_fire() {
    // A slow timer at the end of machine time is unreachable: no wakeup.
    let (machine, _) = Timers::new(&[u64::MAX], false);
    let mut driver = driver(vec![machine], 1_000, 1);
    set_rate(&mut driver, 0, 0, -1);
    assert!(matches!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Blocked { .. }
    ));
    // A fast busy machine cannot map a bound beyond u64 machine time.
    let (machine, _) = Timers::new(&[], true);
    let mut driver = self::driver(vec![machine], u64::MAX - 1, 1);
    set_rate(&mut driver, 0, 0, 100_000_000);
    assert!(driver.step(at(0)).is_ok());
    assert!(matches!(
        driver.step(at(u64::MAX - 1)),
        Err(Error::ClockOverflow { id: MachineId(0) })
    ));
}
