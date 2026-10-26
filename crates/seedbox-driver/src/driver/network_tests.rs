//! Driver routing, causal rounds and exact frame delivery with scripted machines.
use super::fixture::{auto_key, definition};
use super::*;
use crate::definition::{MacAddress, NetworkInterface};
use std::sync::Mutex;

fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
const MS: u64 = 1_000_000;

fn mac(n: u8) -> [u8; 6] {
    [2, 0, 0, 0, 0, n]
}
fn frame(to: [u8; 6], tag: u8) -> Vec<u8> {
    let mut bytes = to.to_vec();
    bytes.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0x88, 0xb5, tag]);
    bytes
}

#[derive(Default)]
struct Log {
    /// (machine, start, upto) of every run, in machine time.
    runs: Vec<(usize, u64, u64)>,
    /// (machine, offered machine time, tag) of every accepted frame.
    received: Vec<(usize, u64, u8)>,
    /// (machine, offered machine time, bytes) of every accepted frame.
    frames: Vec<(usize, u64, Vec<u8>)>,
    /// (machine, offered machine time, payload) of every control reply.
    replies: Vec<(usize, u64, Vec<u8>)>,
}

/// Emits scripted outputs at machine times, optionally replying to every
/// frame one nanosecond later (`echo`) or in the accepting run (`answer`).
struct Net {
    id: usize,
    sends: VecDeque<(u64, Response)>,
    busy: bool,
    echo: Option<[u8; 6]>,
    answer: Option<[u8; 6]>,
    exited: bool,
    reached: u64,
    log: Arc<Mutex<Log>>,
}

impl Machine for Net {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();

    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let start = context.clock.now();
        self.reached = self.reached.max(start);
        let mut answers = Vec::new();
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Crash => self.exited = true,
                Request::Reboot => self.exited = false,
                Request::ReceiveFrame { bytes } if !self.exited => {
                    assert_eq!(input.at.as_nanos(), start, "offered at adoption");
                    let tag = bytes[14];
                    let mut log = self.log.lock().unwrap();
                    log.received.push((self.id, start, tag));
                    log.frames.push((self.id, start, bytes.clone()));
                    drop(log);
                    if let Some(to) = self.echo.filter(|_| tag < 60) {
                        let at = start + 1;
                        let index = self.sends.partition_point(|(t, _)| *t <= at);
                        let bytes = frame(to, tag.wrapping_add(1));
                        self.sends
                            .insert(index, (at, Response::TransmitFrame { bytes }));
                    }
                    if let Some(to) = self.answer {
                        answers.push(frame(to, tag.wrapping_add(100)));
                    }
                }
                Request::SimulationReply { payload, .. } => {
                    self.log
                        .lock()
                        .unwrap()
                        .replies
                        .push((self.id, start, payload));
                }
                _ => {}
            }
        }
        if self.exited {
            return Ok(StopReason::Exited {
                reached: at(self.reached),
                reason: ExitReason::Crashed,
            });
        }
        let end = upto.as_nanos();
        self.log.lock().unwrap().runs.push((self.id, start, end));
        // An answer transmits at the accepting run's start, even with zero budget.
        for bytes in answers {
            context
                .outbox
                .emit(crate::pv::Output {
                    at: at(start),
                    payload: Response::TransmitFrame { bytes },
                })
                .unwrap();
        }
        while self.sends.front().is_some_and(|(t, _)| *t <= end) {
            let (t, payload) = self.sends.pop_front().unwrap();
            let t = t.max(start);
            self.reached = self.reached.max(t);
            context
                .outbox
                .emit(crate::pv::Output { at: at(t), payload })
                .unwrap();
        }
        if self.busy {
            self.reached = end;
            return Ok(StopReason::BoundaryReached);
        }
        Ok(StopReason::Blocked {
            reached: at(self.reached),
            deadline: self.sends.front().map(|(t, _)| at(*t)),
        })
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

type World = (Driver<Net>, Arc<Mutex<Log>>);

struct Spec {
    nic: Option<u8>,
    sends: Vec<(u64, Response)>,
    busy: bool,
    echo: Option<[u8; 6]>,
    answer: Option<[u8; 6]>,
}
fn spec(nic: Option<u8>) -> Spec {
    Spec {
        nic,
        sends: Vec::new(),
        busy: false,
        echo: None,
        answer: None,
    }
}
impl Spec {
    fn send(self, at: u64, bytes: Vec<u8>) -> Self {
        self.emit(at, Response::TransmitFrame { bytes })
    }
    fn emit(mut self, at: u64, payload: Response) -> Self {
        self.sends.push((at, payload));
        self
    }
    fn answer(mut self, to: [u8; 6]) -> Self {
        self.answer = Some(to);
        self
    }
    fn busy(mut self) -> Self {
        self.busy = true;
        self
    }
    fn echo(mut self, to: [u8; 6]) -> Self {
        self.echo = Some(to);
        self
    }
}

fn networked(index: usize, nic: Option<u8>) -> MachineDefinition {
    let mut definition = definition(index);
    definition.network = nic.map(|n| {
        NetworkInterface::new(
            MacAddress::new(mac(n)).unwrap(),
            std::net::Ipv4Addr::new(10, 0, 0, n),
            24,
        )
        .unwrap()
    });
    definition
}

fn world(
    start: u64,
    quantum: u64,
    workers: usize,
    config: NetworkConfig,
    specs: Vec<Spec>,
    spawnable: Vec<Spec>,
) -> Result<World, Error<std::io::Error>> {
    let log = Arc::new(Mutex::new(Log::default()));
    let spawnable_len = spawnable.len();
    let definitions: Vec<_> = specs
        .iter()
        .chain(&spawnable)
        .enumerate()
        .map(|(index, spec)| networked(index, spec.nic))
        .collect();
    let mut machines: BTreeMap<String, Net> = specs
        .into_iter()
        .chain(spawnable)
        .enumerate()
        .map(|(index, spec)| {
            (
                definitions[index].name.clone(),
                Net {
                    id: index,
                    sends: spec.sends.into(),
                    busy: spec.busy,
                    echo: spec.echo,
                    answer: spec.answer,
                    exited: false,
                    reached: 0,
                    log: log.clone(),
                },
            )
        })
        .collect();
    let initial = definitions.len() - spawnable_len;
    let mut definitions = definitions.into_iter();
    let oracle = definitions.next().unwrap();
    let others: Vec<_> = definitions.by_ref().take(initial - 1).collect();
    let spawnable: Vec<_> = definitions.collect();
    let driver = Driver::new(
        at(start),
        Duration::from_nanos(quantum),
        workers,
        crate::rng::SeededRng::new(5),
        || Ok(()),
        oracle,
        others,
        spawnable,
        config,
        move |definition| {
            Ok(machines
                .remove(&definition.name)
                .expect("one machine per name"))
        },
    )?;
    Ok((driver, log))
}

fn simple(quantum: u64, config: NetworkConfig, specs: Vec<Spec>) -> World {
    world(0, quantum, 1, config, specs, Vec::new()).unwrap()
}

/// Step to `upto`, returning each barrier reached on the way.
fn barriers(driver: &mut Driver<Net>, upto: u64) -> Vec<StepOutcome> {
    let mut seen = Vec::new();
    loop {
        let outcome = driver.step(at(upto)).unwrap();
        seen.push(outcome.clone());
        match outcome {
            StepOutcome::BarrierReached { at: reached } if reached.as_nanos() < upto => {}
            _ => return seen,
        }
    }
}
fn schedule(driver: &mut Driver<Net>, when: u64, action: Action) {
    driver
        .with_barrier(|barrier| barrier.schedule(at(when), auto_key(), action))
        .unwrap();
}
fn received(log: &Arc<Mutex<Log>>) -> Vec<(usize, u64, u8)> {
    log.lock().unwrap().received.clone()
}

#[test]
fn blocked_networked_machines_jump_to_their_next_event() {
    let (mut driver, log) = simple(
        100_000 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1)).send(10_000 * MS, frame(mac(2), 7)),
            spec(Some(2)),
        ],
    );
    // Only the initial runnable round is bounded by latency; blocked machines
    // impose no bound, so the next barrier is the 10 s send deadline.
    assert_eq!(
        barriers(&mut driver, 20_000 * MS),
        [
            StepOutcome::BarrierReached { at: at(MS) },
            StepOutcome::BarrierReached {
                at: at(10_000 * MS)
            },
            StepOutcome::Blocked {
                at: at(10_000 * MS + MS)
            },
        ]
    );
    assert_eq!(received(&log), [(1, 10_001 * MS, 7)]);
    assert!(log
        .lock()
        .unwrap()
        .runs
        .iter()
        .all(|&(_, start, _)| start == 0 || start >= 10_000 * MS));
}

#[test]
fn runnable_attached_machines_bound_rounds_by_latency_but_unattached_do_not() {
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![spec(Some(1)).busy(), spec(Some(2))],
    );
    barriers(&mut driver, 5 * MS);
    let runs: Vec<_> = log
        .lock()
        .unwrap()
        .runs
        .iter()
        .filter(|run| run.0 == 0)
        .map(|run| (run.1, run.2))
        .collect();
    assert_eq!(
        runs,
        (0..5).map(|n| (n * MS, (n + 1) * MS)).collect::<Vec<_>>()
    );

    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![spec(None).busy(), spec(Some(2))],
    );
    barriers(&mut driver, 5 * MS);
    // Only the attached machine's initial runnable round is bounded.
    let runs: Vec<_> = log
        .lock()
        .unwrap()
        .runs
        .iter()
        .filter(|r| r.0 == 0)
        .copied()
        .collect();
    assert_eq!(runs, [(0, 0, MS), (0, MS, 5 * MS)]);
}

#[test]
fn delivery_is_exact_in_simulation_time_and_offered_in_receiver_time() {
    let config = NetworkConfig::new(Duration::from_millis(1), Duration::ZERO, 0).unwrap();
    let (mut driver, log) = simple(
        100 * MS,
        config,
        vec![spec(Some(1)).send(3 * MS, frame(mac(2), 1)), spec(Some(2))],
    );
    barriers(&mut driver, 10 * MS);
    assert_eq!(received(&log), [(1, 4 * MS, 1)]);

    // Sender +10%, receiver -10%: machine 3.3 ms is simulation 3 ms; arrival at
    // simulation 4 ms is offered at receiver machine time 3.6 ms.
    let (mut driver, log) = simple(
        100 * MS,
        config,
        vec![
            spec(Some(1)).send(3_300_000, frame(mac(2), 1)),
            spec(Some(2)),
        ],
    );
    driver
        .with_barrier(|b| {
            b.schedule_clock_rate(at(0), MachineId(0), ClockRate::new(100_000_000).unwrap())?;
            b.schedule_clock_rate(at(0), MachineId(1), ClockRate::new(-100_000_000).unwrap())
        })
        .unwrap();
    barriers(&mut driver, 10 * MS);
    assert_eq!(received(&log), [(1, 3_600_000, 1)]);
}

#[test]
fn timeline_items_at_an_arrival_precede_its_frames() {
    let sender = || spec(Some(1)).send(0, frame(mac(2), 1));
    // Crash at the arrival instant: the receiver's boot has exited, so it drops.
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![sender(), spec(Some(2))],
    );
    schedule(
        &mut driver,
        MS,
        Action::Crash(Target::Machine(MachineId(1))),
    );
    barriers(&mut driver, 5 * MS);
    assert!(received(&log).is_empty());

    // A Reboot applied at the arrival instant precedes it: the new live boot
    // receives the frame; the physical network has no stale-boot filter.
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![sender(), spec(Some(2))],
    );
    schedule(
        &mut driver,
        MS - 1,
        Action::Crash(Target::Machine(MachineId(1))),
    );
    schedule(
        &mut driver,
        MS,
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    barriers(&mut driver, 5 * MS);
    assert!(driver.drain_events().iter().any(|event| matches!(
        event.kind,
        EventKind::Transition { change: event::Transition::Rebooted { .. }, .. } if event.at == at(MS)
    )));
    assert_eq!(received(&log), [(1, MS, 1)]);

    // A crash one nanosecond later does not affect the earlier arrival.
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![sender(), spec(Some(2))],
    );
    schedule(
        &mut driver,
        MS + 1,
        Action::Crash(Target::Machine(MachineId(1))),
    );
    barriers(&mut driver, 5 * MS);
    assert_eq!(received(&log), [(1, MS, 1)]);
}

#[test]
fn frames_survive_sender_crash_and_reach_paused_receivers() {
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1)),
            spec(Some(2)).send(0, frame(mac(3), 1)),
            spec(Some(3)),
        ],
    );
    schedule(&mut driver, 1, Action::Crash(Target::Machine(MachineId(1))));
    schedule(&mut driver, 1, Action::Pause(Target::Machine(MachineId(2))));
    barriers(&mut driver, 5 * MS);
    // Offered to the paused receiver's NIC at arrival, by a zero-budget run.
    assert_eq!(received(&log), [(2, MS, 1)]);
    driver
        .with_barrier(|b| {
            assert!(b.paused(MachineId(2))?);
            Ok(())
        })
        .unwrap();
}

#[test]
fn simultaneous_sends_arrive_together_in_sender_order() {
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1)),
            spec(Some(2)).send(2 * MS, frame([0xff; 6], 2)),
            spec(Some(3)).send(2 * MS, frame(mac(1), 3)),
            spec(Some(4)).send(2 * MS, frame(mac(1), 4)),
        ],
    );
    barriers(&mut driver, 10 * MS);
    let mut got = received(&log);
    got.sort_by_key(|r| r.0);
    // Machine 0 takes all three in one admission, in sender order; the
    // broadcast reaches every other attached machine at the same arrival.
    assert_eq!(
        got,
        [
            (0, 3 * MS, 2),
            (0, 3 * MS, 3),
            (0, 3 * MS, 4),
            (2, 3 * MS, 2),
            (3, 3 * MS, 2)
        ]
    );
    let runs = log.lock().unwrap().runs.clone();
    assert_eq!(runs.iter().filter(|r| r.0 == 0 && r.1 == 3 * MS).count(), 1);
}

#[test]
fn spawned_machines_join_the_network_and_host_additions_stay_unattached() {
    let (mut driver, log) = world(
        0,
        10 * MS,
        1,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(5 * MS, frame(mac(2), 9))],
        vec![spec(Some(2))],
    )
    .unwrap();
    schedule(
        &mut driver,
        MS,
        Action::Spawn {
            name: "fixture-1".into(),
        },
    );
    barriers(&mut driver, 10 * MS);
    assert_eq!(received(&log), [(1, 6 * MS, 9)]);

    let (mut driver, _) = simple(10 * MS, NetworkConfig::default(), vec![spec(Some(1))]);
    let log = Arc::new(Mutex::new(Log::default()));
    let added = driver
        .with_barrier(|b| {
            b.add(|| {
                Ok(Net {
                    id: 9,
                    sends: VecDeque::from([(
                        0,
                        Response::TransmitFrame {
                            bytes: frame(mac(1), 1),
                        },
                    )]),
                    busy: false,
                    echo: None,
                    answer: None,
                    exited: false,
                    reached: 0,
                    log,
                })
            })
        })
        .unwrap();
    driver.drain_events();
    assert!(matches!(driver.step(at(MS)), Err(Error::InvalidFrame { id }) if id == added));
    assert!(driver
        .drain_observations()
        .events
        .iter()
        .all(|event| !matches!(&event.kind, EventKind::Output(_))));
}

#[test]
fn the_nameserver_answers_current_members_regardless_of_partitions() {
    use super::fixture::owned::{dns_query, dns_reply};
    let query = |id| dns_query(mac(1), [10, 0, 0, 1], "fixture-2", id);
    let (mut driver, log) = world(
        0,
        10 * MS,
        1,
        NetworkConfig::default(),
        vec![
            spec(Some(1))
                .send(MS, query(1))
                .send(5 * MS, query(2))
                .send(5 * MS, dns_query(mac(1), [10, 0, 0, 1], "FIXTURE-1", 3)),
            spec(Some(2)),
        ],
        vec![spec(Some(3))],
    )
    .unwrap();
    schedule(&mut driver, 0, partition(&[0], &[1]));
    schedule(
        &mut driver,
        3 * MS,
        Action::Spawn {
            name: "fixture-2".into(),
        },
    );
    barriers(&mut driver, 10 * MS);
    let replies: Vec<_> = log
        .lock()
        .unwrap()
        .frames
        .iter()
        .map(|(machine, at, bytes)| (*machine, *at, dns_reply(bytes)))
        .collect();
    assert_eq!(
        replies,
        [
            // Not yet spawned: NXDOMAIN after one network latency.
            (0, 2 * MS, (1, 3, None)),
            (0, 6 * MS, (2, 0, Some([10, 0, 0, 3]))),
            // Partitioned from fixture-1, yet its name still resolves.
            (0, 6 * MS, (3, 0, Some([10, 0, 0, 2]))),
        ]
    );
    assert!(driver
        .execution
        .network
        .counts()
        .keys()
        .all(|pair| pair.from != pair.to));
}

#[test]
fn invalid_frames_fail_before_any_output_publishes() {
    for bytes in [vec![0xff; 13], {
        let mut long = frame(mac(2), 0);
        long.resize(crate::pv::MAX_FRAME_BYTES + 1, 0);
        long
    }] {
        let (mut driver, _) = simple(
            10 * MS,
            NetworkConfig::default(),
            vec![spec(Some(1)).send(0, bytes), spec(Some(2))],
        );
        driver.drain_events();
        assert!(matches!(
            driver.step(at(MS)),
            Err(Error::InvalidFrame { id: MachineId(0) })
        ));
        assert!(driver.drain_events().is_empty());
    }
    // A maximum-length frame is valid.
    let mut long = frame(mac(2), 0);
    long.resize(crate::pv::MAX_FRAME_BYTES, 0);
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(0, long), spec(Some(2))],
    );
    barriers(&mut driver, 5 * MS);
    assert_eq!(received(&log), [(1, MS, 0)]);
}

#[test]
fn duplicate_macs_are_rejected_before_the_factory_runs() {
    let mut definitions = (0..3).map(|i| networked(i, Some(1 + i as u8 % 2)));
    let oracle = definitions.next().unwrap();
    let rest: Vec<_> = definitions.collect();
    let result = Driver::<Net>::new(
        at(0),
        Duration::from_nanos(10),
        1,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        oracle,
        vec![rest[0].clone()],
        vec![rest[1].clone()],
        NetworkConfig::default(),
        |_| panic!("factory ran"),
    );
    assert!(matches!(result, Err(Error::DuplicateMac(mac)) if mac.octets() == [2, 0, 0, 0, 0, 1]));
}

#[test]
fn unrepresentable_arrivals_fail_with_time_overflow_and_bounds_may_overflow() {
    let start = u64::MAX - MS / 2;
    // No send: the cap F + L overflows and the existing horizon applies.
    let (mut driver, _) = world(
        start,
        10,
        1,
        NetworkConfig::default(),
        vec![spec(Some(1)).busy(), spec(Some(2))],
        Vec::new(),
    )
    .unwrap();
    assert_eq!(
        driver.step(at(start + 10)).unwrap(),
        StepOutcome::BarrierReached { at: at(start + 10) }
    );
    let (mut driver, _) = world(
        start,
        10,
        1,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(start, frame(mac(2), 0)), spec(Some(2))],
        Vec::new(),
    )
    .unwrap();
    assert!(matches!(
        driver.step(at(start + 10)),
        Err(Error::TimeOverflow)
    ));
}

#[test]
fn worker_count_never_changes_network_results() {
    let run = |workers| {
        let config = NetworkConfig::new(
            Duration::from_micros(100),
            Duration::from_micros(50),
            100_000,
        )
        .unwrap();
        let (mut driver, log) = world(
            0,
            MS,
            workers,
            config,
            vec![
                spec(Some(1)).echo(mac(2)).send(0, frame(mac(2), 0)).busy(),
                spec(Some(2)).echo([0xff; 6]),
                spec(Some(3)).echo(mac(1)).send(10, frame([0xff; 6], 100)),
            ],
            Vec::new(),
        )
        .unwrap();
        named(&mut driver, 2 * MS, "cut", partition(&[0], &[1]));
        named(
            &mut driver,
            8 * MS,
            "heal",
            Action::Heal(HealTarget::Input(InputKey::new("cut").unwrap())),
        );
        barriers(&mut driver, 20 * MS);
        let events = driver.drain_events();
        let log = log.lock().unwrap();
        // Workers append concurrently; each machine's own order is the evidence.
        let mut received = log.received.clone();
        received.sort_by_key(|r| r.0);
        let mut runs = log.runs.clone();
        runs.sort_by_key(|r| r.0);
        (format!("{events:?}"), received, runs)
    };
    let one = run(1);
    assert!(one.1.len() > 50, "traffic exercised: {}", one.1.len());
    assert_eq!(one, run(4));
}

fn partition(left: &[usize], right: &[usize]) -> Action {
    let side = |ids: &[usize]| ids.iter().map(|&n| Target::Machine(MachineId(n))).collect();
    Action::Partition {
        left: side(left),
        right: side(right),
    }
}
fn named(driver: &mut Driver<Net>, when: u64, key: &str, action: Action) {
    driver
        .with_barrier(|b| b.schedule(at(when), InputKey::new(key).unwrap(), action))
        .unwrap();
}
fn settled(events: &[Event]) -> Vec<(FaultRecord, SettlementStatus)> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::FaultSettled { record, status, .. } => Some((record.clone(), *status)),
            _ => None,
        })
        .collect()
}

#[test]
fn partitions_cut_both_directions_between_sides_only_and_record_their_links() {
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1))
                .send(2 * MS, frame(mac(2), 1))
                .send(2 * MS, frame(mac(3), 2)),
            spec(Some(2)).send(2 * MS, frame(mac(1), 3)),
            spec(Some(3)).send(2 * MS, frame([0xff; 6], 4)),
        ],
    );
    named(&mut driver, MS, "cut", partition(&[1], &[0]));
    barriers(&mut driver, 10 * MS);
    let mut got = received(&log);
    got.sort();
    // 0<->1 is cut both ways; 0->2, 2->0 and 2->1 are not.
    assert_eq!(got, [(0, 3 * MS, 4), (1, 3 * MS, 4), (2, 3 * MS, 2)]);
    let events = driver.drain_events();
    let (record, status) = settled(&events).remove(0);
    assert_eq!(status, SettlementStatus::Applied);
    assert_eq!(record.action, FaultAction::Partition);
    assert_eq!(
        record.target,
        FaultTarget::Links {
            left: vec![MachineId(1)],
            right: vec![MachineId(0)]
        }
    );
    assert_eq!(record.handle, Some(FaultHandle("p1".into())));
}

#[test]
fn partitions_cut_at_send_stack_and_heal_only_their_own_links() {
    let config =
        NetworkConfig::new(Duration::from_millis(1), Duration::from_micros(10), 1).unwrap();
    let (mut driver, log) = simple(
        20 * MS,
        config,
        vec![
            spec(Some(1))
                .send(0, frame(mac(2), 1))
                .send(2 * MS, frame(mac(2), 2))
                .send(5 * MS, frame(mac(2), 3))
                .send(7 * MS, frame(mac(2), 4)),
            spec(Some(2)),
        ],
    );
    // Sent at 0, in flight when the first partition applies at 500 us.
    named(&mut driver, MS / 2, "first", partition(&[0], &[1]));
    named(&mut driver, MS / 2, "second", partition(&[1], &[0]));
    named(
        &mut driver,
        4 * MS,
        "heal-first",
        Action::Heal(HealTarget::Input(InputKey::new("first").unwrap())),
    );
    named(
        &mut driver,
        6 * MS,
        "heal-second",
        Action::Heal(HealTarget::Handle(FaultHandle("p2".into()))),
    );
    barriers(&mut driver, 20 * MS);
    let tags: Vec<_> = received(&log).iter().map(|r| r.2).collect();
    // Frame 1 survives; 2 and 3 are cut while either partition is active.
    assert_eq!(tags, [1, 4]);
    let heals: Vec<_> = settled(&driver.drain_events())
        .into_iter()
        .filter(|(r, _)| r.action == FaultAction::Heal)
        .map(|(r, s)| (r.target, r.handle, s))
        .collect();
    let links = |l: usize, r: usize| FaultTarget::Links {
        left: vec![MachineId(l)],
        right: vec![MachineId(r)],
    };
    assert_eq!(
        heals,
        [
            (
                links(0, 1),
                Some(FaultHandle("p1".into())),
                SettlementStatus::Applied
            ),
            (
                links(1, 0),
                Some(FaultHandle("p2".into())),
                SettlementStatus::Applied
            ),
        ]
    );
}

#[test]
fn cut_candidates_draw_nothing_and_reserve_no_fifo_slot() {
    let config =
        NetworkConfig::new(Duration::from_millis(1), Duration::from_micros(500), 1).unwrap();
    let run = |cut: bool| {
        let mut specs = spec(Some(1));
        if cut {
            for n in 0..5 {
                specs = specs.send(MS + n, frame(mac(2), 9));
            }
        }
        let (mut driver, log) = simple(
            20 * MS,
            config,
            vec![specs.send(5 * MS, frame(mac(2), 1)), spec(Some(2))],
        );
        named(&mut driver, 0, "cut", partition(&[0], &[1]));
        named(
            &mut driver,
            3 * MS,
            "heal",
            Action::Heal(HealTarget::Input(InputKey::new("cut").unwrap())),
        );
        barriers(&mut driver, 20 * MS);
        (
            received(&log),
            driver
                .execution
                .network
                .pair_cursor(MachineId(0), MachineId(1)),
        )
    };
    // Cut frames neither advance the pair's streams nor its FIFO clock.
    assert_eq!(run(true), run(false));
    assert_eq!(run(true).0.len(), 1);
}

#[test]
fn partitions_outlive_member_crash_and_reboot() {
    let (mut driver, log) = simple(
        20 * MS,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(5 * MS, frame(mac(2), 1)), spec(Some(2))],
    );
    named(&mut driver, 0, "cut", partition(&[0], &[1]));
    schedule(
        &mut driver,
        MS,
        Action::Crash(Target::Machine(MachineId(1))),
    );
    schedule(
        &mut driver,
        2 * MS,
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    barriers(&mut driver, 20 * MS);
    assert!(received(&log).is_empty());
    let partitions: Vec<_> = settled(&driver.drain_events())
        .into_iter()
        .filter(|(r, _)| r.action == FaultAction::Partition)
        .map(|(_, s)| s)
        .collect();
    assert_eq!(partitions, [SettlementStatus::Applied]);
}

#[test]
fn partition_membership_is_rejected_before_allocation() {
    let (mut driver, _) = world(
        0,
        10 * MS,
        1,
        NetworkConfig::default(),
        vec![spec(Some(1)), spec(Some(2)), spec(None)],
        vec![spec(Some(4))],
    )
    .unwrap();
    let name = |n: &str| Target::Name(n.into());
    let cases = [
        (
            "empty",
            Action::Partition {
                left: vec![],
                right: vec![name("fixture-0")],
            },
            RejectionReason::InvalidRequest,
        ),
        (
            "overlap",
            partition(&[0, 1], &[1]),
            RejectionReason::InvalidRequest,
        ),
        (
            "repeat",
            partition(&[0, 0], &[1]),
            RejectionReason::InvalidRequest,
        ),
        (
            "bare",
            partition(&[0], &[2]),
            RejectionReason::InvalidRequest,
        ),
        (
            "unspawned",
            Action::Partition {
                left: vec![name("fixture-0")],
                right: vec![name("fixture-3")],
            },
            RejectionReason::TargetUnavailable,
        ),
    ];
    for (key, action, _) in &cases {
        named(&mut driver, 0, key, action.clone());
    }
    named(&mut driver, 0, "valid", partition(&[0], &[1]));
    // Schedule-time validation still refuses unknown members.
    assert!(matches!(
        driver.with_barrier(|b| b.schedule(
            at(0),
            InputKey::new("unknown").unwrap(),
            Action::Partition {
                left: vec![name("nobody")],
                right: vec![name("fixture-0")]
            }
        )),
        Err(Error::UnknownName(_))
    ));
    driver.step(at(MS)).unwrap();
    let events = driver.drain_events();
    let rejected: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::InputRejected { key, rejection } => {
                Some((key.as_str().to_owned(), rejection.reason))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        rejected,
        cases
            .iter()
            .map(|(key, _, reason)| (key.to_string(), *reason))
            .collect::<Vec<_>>()
    );
    // No rejected partition consumed a handle.
    assert_eq!(settled(&events)[0].0.handle, Some(FaultHandle("p1".into())));
}

#[test]
fn exhausted_frame_sequences_fail_with_position_overflow_before_publication() {
    let (mut driver, _) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(0, frame(mac(2), 0)), spec(Some(2))],
    );
    driver
        .execution
        .network
        .set_sequence(MachineId(0), u64::MAX);
    driver.drain_events();
    assert!(matches!(driver.step(at(MS)), Err(Error::PositionOverflow)));
    assert!(driver
        .drain_observations()
        .events
        .iter()
        .all(|event| !matches!(event.kind, EventKind::Output(_))));
    assert!(matches!(driver.step(at(MS)), Err(Error::Failed)));
}

fn control(request: u32, op: serde_json::Value) -> Response {
    let mut wire = op;
    wire["version"] = serde_json::json!(1);
    wire["id"] = serde_json::json!(request);
    Response::SimulationControl {
        id: crate::pv::SimulationControlId {
            session: NonZeroU32::new(1).unwrap(),
            request: NonZeroU32::new(request).unwrap(),
        },
        payload: serde_json::to_vec(&wire).unwrap(),
    }
}
fn members(names: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        names
            .iter()
            .map(|n| serde_json::json!({ "machine": n }))
            .collect(),
    )
}
fn replies(log: &Arc<Mutex<Log>>) -> Vec<serde_json::Value> {
    log.lock()
        .unwrap()
        .replies
        .iter()
        .map(|(_, _, payload)| serde_json::from_slice(payload).unwrap())
        .collect()
}

#[test]
fn guest_partition_calls_reply_with_handles_and_cut_links_through_the_driver() {
    let partition = serde_json::json!({
        "op": "partition",
        "left": members(&["fixture-2"]),
        "right": members(&["fixture-1"]),
    });
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1)).emit(MS, control(1, partition)),
            spec(Some(2)).send(5 * MS, frame(mac(3), 1)),
            spec(Some(3)).send(5 * MS, frame(mac(2), 2)),
        ],
    );
    barriers(&mut driver, 10 * MS);
    assert_eq!(
        replies(&log)[0]["result"],
        serde_json::json!({"handle":"p1"})
    );
    assert!(received(&log).is_empty());
    let (record, status) = settled(&driver.drain_events()).remove(0);
    assert_eq!(status, SettlementStatus::Applied);
    assert!(matches!(
        record.origin,
        FaultOrigin::Control {
            machine: MachineId(0),
            ..
        }
    ));
    // Published sides are numeric identities in ascending order.
    assert_eq!(
        record.target,
        FaultTarget::Links {
            left: vec![MachineId(2)],
            right: vec![MachineId(1)]
        }
    );
}

#[test]
fn triggered_partitions_arm_without_a_target_and_cut_after_firing() {
    let arm = |request, right: &[&str]| {
        control(
            request,
            serde_json::json!({
                "op": "arm_trigger",
                "source": { "machine": "fixture-1" },
                "marker": [7],
                "match_timeout_ns": 10 * MS,
                "fault": {
                    "action": "partition",
                    "left": members(&["fixture-1"]),
                    "right": members(right),
                },
            }),
        )
    };
    let (mut driver, log) = simple(
        20 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1))
                .emit(MS, arm(1, &["fixture-3"]))
                .emit(MS, arm(2, &["fixture-2"]))
                .emit(2 * MS, control(3, serde_json::json!({"op":"inspect"})))
                .emit(7 * MS, control(4, serde_json::json!({"op":"inspect"}))),
            spec(Some(2))
                .send(2 * MS, frame(mac(3), 1))
                .emit(
                    3 * MS,
                    Response::ApplicationOutput {
                        stream: crate::pv::Stream::Stdout,
                        bytes: vec![7],
                    },
                )
                .send(6 * MS, frame(mac(3), 2)),
            spec(Some(3)),
            spec(None),
        ],
    );
    barriers(&mut driver, 20 * MS);
    let replies = replies(&log);
    // An unattached member is refused without consuming a trigger ID.
    assert_eq!(replies[0]["error"]["code"], "invalid_request");
    assert_eq!(replies[1]["result"], serde_json::json!({"trigger_id":1}));
    // The frame before the match arrives; the one after the fired cut does not.
    assert_eq!(received(&log), [(2, 3 * MS, 1)]);
    let events = driver.drain_events();
    assert!(events.iter().any(|e| matches!(
        e.kind,
        EventKind::TriggerArmed {
            trigger: 1,
            source: MachineId(1),
            target: None,
            ..
        }
    )));
    let admitted = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FaultAdmitted(r) if r.origin == FaultOrigin::Trigger { trigger: 1 } => {
                Some(r.clone())
            }
            _ => None,
        })
        .expect("the fired trigger admits its partition");
    assert_eq!(admitted.action, FaultAction::Partition);
    assert_eq!(
        admitted.target,
        FaultTarget::Links {
            left: vec![MachineId(1)],
            right: vec![MachineId(2)]
        }
    );
    let armed = &replies[2]["result"]["triggers"][0];
    assert_eq!(armed["status"], "armed");
    assert!(armed.get("handle").is_none());
    assert!(replies[2]["result"]["active_handles"]
        .as_array()
        .unwrap()
        .is_empty());
    let fired = &replies[3]["result"]["triggers"][0];
    assert_eq!(fired["status"], "fired");
    assert_eq!(fired["fault_id"], admitted.fault.get());
    assert_eq!(
        fired["handle"],
        admitted.handle.as_ref().unwrap().to_string()
    );
    assert_eq!(
        replies[3]["result"]["active_handles"],
        serde_json::json!([fired["handle"]])
    );
}

#[test]
fn round_end_transmissions_route_before_a_same_time_partition_but_admission_answers_after() {
    const T: u64 = 2 * MS;
    let run = |cut: bool| {
        let (mut driver, log) = simple(
            10 * MS,
            NetworkConfig::default(),
            vec![
                spec(Some(1))
                    .busy()
                    .send(T, frame(mac(2), 1))
                    .answer(mac(2)),
                spec(Some(2)),
                spec(Some(3)).send(T - MS, frame(mac(1), 5)),
            ],
        );
        if cut {
            named(&mut driver, T, "cut", partition(&[0], &[1]));
        }
        barriers(&mut driver, 10 * MS);
        let mut got = received(&log);
        got.sort();
        got
    };
    // The answer is transmitted by the zero-budget admission run at T and
    // arrives a latency later, strictly after the earlier round's frame.
    assert_eq!(
        run(false),
        [(0, T, 5), (1, T + MS, 1), (1, T + MS + 1, 105)]
    );
    // A frame stamped T by the round ending at T routes at that round's commit,
    // before the partition's FIFO turn at T; the answer routes after it.
    assert_eq!(run(true), [(0, T, 5), (1, T + MS, 1)]);
}

#[test]
fn a_frame_arriving_at_a_timer_deadline_shares_one_admission_run() {
    let (mut driver, log) = simple(
        10 * MS,
        NetworkConfig::default(),
        vec![
            spec(Some(1)).send(2 * MS, frame(mac(2), 1)),
            spec(Some(2)).send(3 * MS, frame(mac(1), 2)),
        ],
    );
    barriers(&mut driver, 10 * MS);
    let log = log.lock().unwrap();
    // Frames precede timers at a barrier: the delivery run also serves the
    // due timer, so no separate wake run follows.
    let at_deadline = log
        .runs
        .iter()
        .filter(|r| (r.0, r.1) == (1, 3 * MS))
        .count();
    assert_eq!(at_deadline, 1);
    let mut got = log.received.clone();
    got.sort();
    assert_eq!(got, [(0, 4 * MS, 2), (1, 3 * MS, 1)]);
}

#[test]
fn down_members_can_be_partitioned_and_stay_cut_after_reboot() {
    let (mut driver, log) = simple(
        20 * MS,
        NetworkConfig::default(),
        vec![spec(Some(1)).send(5 * MS, frame(mac(2), 1)), spec(Some(2))],
    );
    schedule(
        &mut driver,
        MS,
        Action::Crash(Target::Machine(MachineId(1))),
    );
    named(&mut driver, 2 * MS, "cut", partition(&[0], &[1]));
    schedule(
        &mut driver,
        3 * MS,
        Action::Reboot(Target::Machine(MachineId(1))),
    );
    barriers(&mut driver, 20 * MS);
    assert!(received(&log).is_empty());
    let events = driver.drain_events();
    assert!(events.iter().any(|e| matches!(
        e.kind,
        EventKind::Transition {
            machine: MachineId(1),
            change: event::Transition::Rebooted { .. },
            ..
        }
    )));
    assert!(settled(&events)
        .iter()
        .any(|(r, s)| r.action == FaultAction::Partition && *s == SettlementStatus::Applied));
}
