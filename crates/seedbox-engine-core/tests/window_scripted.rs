//! Scripted machines through the serial and parallel executors. Every
//! scenario must give identical machine state, network effects and outcome
//! for any worker count and any host completion order.

use seedbox_engine_core::window::{
    run_parallel, run_serial, Assignment, Delivery, Error, EventKey, Machine, NoHook, Outcome,
    Output, Params, Report, Router, Time, World, NEVER,
};
use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Msg {
    from: usize,
    to: usize,
    tag: u64,
    hop: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
    /// Compute for this long, in quanta.
    Busy(Time),
    Send {
        to: usize,
        tag: u64,
    },
    /// Idle for this long.
    Sleep(Time),
    /// Idle until this many messages have been received in total.
    Await(u64),
    Exit,
    /// Fail the run (worker failure tests).
    Fail,
    /// Panic inside the machine (worker failure tests).
    Panic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Script {
    id: usize,
    time: Time,
    /// Busy progress is made in steps of at most this, so horizon stops can
    /// land early, as counted stops at cheap points do.
    quantum: Time,
    steps: VecDeque<Step>,
    sleep_until: Option<Time>,
    /// Reply to each message after this long, until a message's hop count
    /// reaches the limit.
    echo: Option<(Time, u32)>,
    replies: BTreeMap<(Time, u64), Msg>,
    reply_seq: u64,
    received: u64,
    /// (receiver time, event key, message) for each delivery.
    trace: Vec<(Time, EventKey, Msg)>,
    exited: Option<Time>,
    /// Host time spent per run: perturbs completion order only.
    host_delay: Duration,
    /// Wake only when about to act, as a halted guest does, instead of
    /// jumping to the assignment's wake time first. Such a machine may stop
    /// short of the slice end when idle.
    lazy: bool,
}

impl Script {
    fn new(id: usize, quantum: Time, steps: Vec<Step>) -> Self {
        Self {
            id,
            time: 0,
            quantum,
            steps: steps.into(),
            sleep_until: None,
            echo: None,
            replies: BTreeMap::new(),
            reply_seq: 0,
            received: 0,
            trace: Vec::new(),
            exited: None,
            host_delay: Duration::ZERO,
            lazy: false,
        }
    }

    fn echo(mut self, delay: Time, hops: u32) -> Self {
        self.echo = Some((delay, hops));
        self
    }

    /// Run zero-time steps. Returns the time the script can next act by
    /// itself, and whether it is computing.
    fn settle(
        &mut self,
        outputs: &mut Vec<Output<Msg>>,
        limit: usize,
    ) -> Result<(Time, bool), String> {
        while let Some((&(at, _), _)) = self.replies.first_key_value() {
            if at > self.time {
                break;
            }
            let (_, m) = self.replies.pop_first().unwrap();
            self.emit(outputs, limit, m)?;
        }
        let reply = self.replies.keys().next().map_or(NEVER, |k| k.0);
        loop {
            match self.steps.front().cloned() {
                None => return Ok((reply, false)),
                Some(Step::Busy(0)) => {
                    self.steps.pop_front();
                }
                Some(Step::Busy(_)) => return Ok((self.time, true)),
                Some(Step::Send { to, tag }) => {
                    self.steps.pop_front();
                    let m = Msg {
                        from: self.id,
                        to,
                        tag,
                        hop: 0,
                    };
                    self.emit(outputs, limit, m)?;
                }
                Some(Step::Sleep(d)) => {
                    let until = *self.sleep_until.get_or_insert(self.time + d);
                    if self.time < until {
                        return Ok((until.min(reply), false));
                    }
                    self.sleep_until = None;
                    self.steps.pop_front();
                }
                Some(Step::Await(n)) => {
                    if self.received < n {
                        return Ok((reply, false));
                    }
                    self.steps.pop_front();
                }
                Some(Step::Exit) => {
                    self.exited = Some(self.time);
                    return Ok((NEVER, false));
                }
                Some(Step::Fail) => {
                    return Err(format!("machine {} failed at {}", self.id, self.time))
                }
                Some(Step::Panic) => panic!("machine {} panicked at {}", self.id, self.time),
            }
        }
    }

    fn emit(&mut self, outputs: &mut Vec<Output<Msg>>, limit: usize, m: Msg) -> Result<(), String> {
        if outputs.len() == limit {
            return Err(format!(
                "machine {}: more than {limit} outputs in one round",
                self.id
            ));
        }
        outputs.push(Output {
            sent_at: self.time,
            payload: m,
        });
        Ok(())
    }
}

impl Machine<Msg> for Script {
    type Context = ();

    fn run(&mut self, _: &mut (), a: &Assignment<Msg>) -> Result<Report<Msg>, String> {
        std::thread::sleep(self.host_delay);
        // Idle until the first possible action, as a halted guest is woken.
        if !self.lazy {
            self.time = self.time.max(a.wake);
        }
        let mut outputs = Vec::new();
        let mut i = 0;
        while self.exited.is_none() && self.time < a.slice_end() {
            if self.lazy {
                let event = a.incoming.get(i).map_or(NEVER, |e| e.key.at);
                let wake = self.next_action().min(event);
                if wake >= a.slice_end() {
                    break;
                }
                self.time = self.time.max(wake);
            }
            while let Some(e) = a
                .incoming
                .get(i)
                .filter(|e| e.key.at <= self.time + a.delivery_early)
            {
                self.trace.push((self.time, e.key, e.payload.clone()));
                self.received += 1;
                if let Some((delay, hops)) = self.echo {
                    if e.payload.hop < hops {
                        let m = Msg {
                            from: self.id,
                            to: e.payload.from,
                            tag: e.payload.tag,
                            hop: e.payload.hop + 1,
                        };
                        self.replies.insert((self.time + delay, self.reply_seq), m);
                        self.reply_seq += 1;
                    }
                }
                i += 1;
            }
            let (own, busy) = self.settle(&mut outputs, a.max_outputs)?;
            if self.exited.is_some() {
                break;
            }
            let event = a.incoming.get(i).map_or(NEVER, |e| e.key.at);
            let target = a.stop.min(event).min(own.max(self.time + 1));
            if busy {
                let Some(Step::Busy(left)) = self.steps.front_mut() else {
                    unreachable!()
                };
                let step = self.quantum.min(*left).min(target - self.time);
                *left -= step;
                self.time += step;
            } else {
                self.time = target;
            }
        }
        let next = if self.exited.is_some() {
            NEVER
        } else {
            self.next_action()
        };
        Ok(Report {
            time: self.time,
            next,
            consumed: i,
            outputs,
            exited: self.exited,
            suspended: false,
        })
    }
}

impl Script {
    /// The next own action without running anything.
    fn next_action(&self) -> Time {
        let reply = self.replies.keys().next().map_or(NEVER, |k| k.0);
        let own = match self.steps.front() {
            None => NEVER,
            Some(Step::Sleep(d)) => self.sleep_until.unwrap_or(self.time + d),
            Some(Step::Await(n)) if self.received < *n => NEVER,
            Some(_) => self.time,
        };
        own.min(reply).max(self.time)
    }
}

/// Seeded link policy: latency plus jitter, loss, FIFO per directed link.
struct Links {
    latency: Time,
    jitter: Time,
    loss_ppm: u64,
    rng: Rng,
    last: BTreeMap<(usize, usize), Time>,
    dropped: Vec<(Time, Msg)>,
}

impl Links {
    fn new(seed: u64, latency: Time, jitter: Time, loss_ppm: u64) -> Self {
        Self {
            latency,
            jitter,
            loss_ppm,
            rng: Rng(seed | 1),
            last: BTreeMap::new(),
            dropped: Vec::new(),
        }
    }
}

impl Router<Msg> for Links {
    fn route(&mut self, from: usize, sent_at: Time, m: Msg, out: &mut Vec<Delivery<Msg>>) {
        assert_eq!(from, m.from);
        if self.rng.below(1_000_000) < self.loss_ppm {
            self.dropped.push((sent_at, m));
            return;
        }
        let mut at = sent_at + self.latency + self.rng.below(self.jitter + 1);
        let last = self.last.entry((from, m.to)).or_insert(0);
        at = at.max(*last);
        *last = at;
        out.push(Delivery {
            to: m.to,
            at,
            payload: m,
        });
    }
}

/// Delivers below the latency bound: the negative control.
struct Short(Time);

impl Router<Msg> for Short {
    fn route(&mut self, _: usize, sent_at: Time, m: Msg, out: &mut Vec<Delivery<Msg>>) {
        out.push(Delivery {
            to: m.to,
            at: sent_at + self.0,
            payload: m,
        });
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

/// Everything a run decides, for comparison across executors.
#[derive(Debug, PartialEq, Eq)]
struct Witness {
    outcome: Outcome,
    machines: Vec<Script>,
    rounds: Vec<seedbox_engine_core::window::RoundRecord>,
    effects: u64,
    pending: Vec<(usize, EventKey, Msg)>,
    dropped: Vec<(Time, Msg)>,
}

/// None: serial. Some(w): w workers. `reverse`: higher machines finish first.
fn execute(
    params: &Params,
    scripts: &[Script],
    seed: u64,
    workers: Option<usize>,
    reverse: bool,
    loss_ppm: u64,
) -> Result<Witness, Error> {
    let mut world = World::new(scripts.len(), params.clone())?;
    let mut machines = scripts.to_vec();
    if reverse {
        let n = machines.len() as u64;
        for m in &mut machines {
            m.host_delay = Duration::from_micros((n - m.id as u64) * 300);
        }
    }
    let mut links = Links::new(seed, params.latency, 400, loss_ppm);
    let (outcome, _) = match workers {
        None => run_serial(&mut world, &mut machines, &mut (), &mut links, NoHook)?,
        Some(w) => run_parallel(&mut world, &mut machines, w, |_| Ok(()), &mut links, NoHook)?,
    };
    for m in &mut machines {
        m.host_delay = Duration::ZERO;
    }
    Ok(Witness {
        outcome,
        machines,
        rounds: world.rounds().to_vec(),
        effects: world.effects_digest(),
        pending: world
            .pending()
            .map(|(m, k, p)| (m, *k, p.clone()))
            .collect(),
        dropped: links.dropped,
    })
}

/// The gate: every delivery is at or before its time, and no more than
/// `early` before it. An effect discovered after its receiver had run past
/// it would show up here as a late delivery.
fn assert_never_late(w: &Witness, early: Time) {
    for m in &w.machines {
        for (t, key, msg) in &m.trace {
            assert!(
                *t <= key.at,
                "machine {} took {msg:?} due at {} late, at {t}",
                m.id,
                key.at
            );
            assert!(
                *t + early >= key.at,
                "machine {} took {msg:?} due at {} too early, at {t}",
                m.id,
                key.at
            );
        }
    }
}

fn assert_same_everywhere(
    params: &Params,
    scripts: &[Script],
    seed: u64,
    loss_ppm: u64,
) -> Witness {
    let reference = execute(params, scripts, seed, None, false, loss_ppm).unwrap();
    assert_never_late(&reference, params.early);
    for workers in [1, 2, 4] {
        for reverse in [false, true] {
            let w = execute(params, scripts, seed, Some(workers), reverse, loss_ppm).unwrap();
            assert_eq!(
                w, reference,
                "seed {seed}, {workers} workers, reversed {reverse}"
            );
        }
    }
    reference
}

fn params() -> Params {
    Params {
        latency: 1_000,
        early: 200,
        duration: 200_000,
        end_on: None,
        max_outputs: 64,
    }
}

#[test]
fn ping_pong_matches_across_worker_counts() {
    let a = Script::new(
        0,
        37,
        vec![
            Step::Busy(500),
            Step::Send { to: 1, tag: 1 },
            Step::Await(20),
            Step::Exit,
        ],
    )
    .echo(50, 40);
    let b = Script::new(1, 53, vec![Step::Busy(2_000), Step::Await(1_000)]).echo(120, 40);
    let w = assert_same_everywhere(
        &Params {
            end_on: Some(0),
            ..params()
        },
        &[a, b],
        7,
        0,
    );
    assert!(
        matches!(w.outcome, Outcome::Exited { machine: 0, .. }),
        "{:?}",
        w.outcome
    );
    assert!(w.machines[0].received >= 20);
}

#[test]
fn simultaneous_sends_reach_a_receiver_in_machine_order() {
    // Machines 1 and 2 send at the same instant; with no jitter the frames
    // tie at machine 0 and are ordered by sender.
    let p = params();
    let scripts = vec![
        Script::new(0, 10, vec![Step::Await(2)]),
        Script::new(1, 10, vec![Step::Busy(300), Step::Send { to: 0, tag: 1 }]),
        Script::new(2, 10, vec![Step::Busy(300), Step::Send { to: 0, tag: 2 }]),
    ];
    for workers in [None, Some(3)] {
        let mut world = World::new(3, p.clone()).unwrap();
        let mut machines = scripts.clone();
        machines[1].host_delay = Duration::from_millis(5);
        let mut links = Links::new(1, p.latency, 0, 0);
        match workers {
            None => run_serial(&mut world, &mut machines, &mut (), &mut links, NoHook).unwrap(),
            Some(w) => {
                run_parallel(&mut world, &mut machines, w, |_| Ok(()), &mut links, NoHook).unwrap()
            }
        };
        let got: Vec<_> = machines[0]
            .trace
            .iter()
            .map(|(t, k, m)| (*t, k.at, m.from))
            .collect();
        assert_eq!(got, vec![(1_300, 1_300, 1), (1_300, 1_300, 2)]);
    }
}

#[test]
fn idle_guests_jump_time_in_few_rounds() {
    let scripts = vec![
        Script::new(
            0,
            10,
            vec![Step::Sleep(150_000), Step::Send { to: 1, tag: 1 }],
        ),
        Script::new(1, 10, vec![Step::Await(1), Step::Exit]),
    ];
    let w = assert_same_everywhere(
        &Params {
            end_on: Some(1),
            ..params()
        },
        &scripts,
        3,
        0,
    );
    assert!(w.rounds.len() <= 4, "{:?}", w.rounds);
    assert!(
        matches!(w.outcome, Outcome::Exited { machine: 1, exit_at, committed } if exit_at >= 151_000 && committed > exit_at)
    );
}

#[test]
fn a_busy_guest_beside_idle_ones() {
    let mut busy = vec![Step::Busy(150_000)];
    busy.extend((1..4).map(|to| Step::Send { to, tag: 9 }));
    let mut scripts = vec![Script::new(0, 97, busy)];
    for id in 1..4 {
        scripts.push(
            Script::new(
                id,
                10,
                vec![
                    Step::Await(1),
                    Step::Send {
                        to: 0,
                        tag: id as u64,
                    },
                ],
            )
            .echo(10, 1),
        );
    }
    let w = assert_same_everywhere(&params(), &scripts, 11, 0);
    // Each idle guest echoes once and sends once; then nothing is left.
    assert!(
        matches!(w.outcome, Outcome::Quiescent { committed } if committed > 150_000),
        "{:?}",
        w.outcome
    );
    let received: Vec<_> = w.machines.iter().map(|m| m.received).collect();
    assert_eq!(received, vec![6, 1, 1, 1]);
}

#[test]
fn exact_boundaries_stop_every_machine_exactly() {
    let p = params();
    let scripts: Vec<_> = (0..3)
        .map(|id| {
            Script::new(
                id,
                71,
                vec![
                    Step::Busy(30_000),
                    Step::Send {
                        to: (id + 1) % 3,
                        tag: 0,
                    },
                    Step::Busy(30_000),
                ],
            )
            .echo(5, 3)
        })
        .collect();
    for workers in [None, Some(2)] {
        let mut world = World::new(3, p.clone()).unwrap();
        world.add_boundary(12_345).unwrap();
        world.add_boundary(40_000).unwrap();
        let mut machines = scripts.clone();
        let mut links = Links::new(5, p.latency, 300, 0);
        // Run to the first boundary only: duration is a boundary too.
        let mut short = World::new(
            3,
            Params {
                duration: 12_345,
                ..p.clone()
            },
        )
        .unwrap();
        let mut m2 = scripts.clone();
        run_serial(
            &mut short,
            &mut m2,
            &mut (),
            &mut Links::new(5, p.latency, 300, 0),
            NoHook,
        )
        .unwrap();
        assert!(
            m2.iter().all(|m| m.time == 12_345),
            "{:?}",
            m2.iter().map(|m| m.time).collect::<Vec<_>>()
        );
        match workers {
            None => run_serial(&mut world, &mut machines, &mut (), &mut links, NoHook).unwrap(),
            Some(w) => {
                run_parallel(&mut world, &mut machines, w, |_| Ok(()), &mut links, NoHook).unwrap()
            }
        };
        for b in [12_345, 40_000] {
            assert!(
                world.rounds().iter().any(|r| r.end == b && r.exact),
                "no exact round ends at {b}"
            );
            assert!(
                !world.rounds().iter().any(|r| r.start < b && r.end > b),
                "a round spans {b}"
            );
        }
        for m in &machines {
            for (t, k, _) in &m.trace {
                for b in [12_345, 40_000] {
                    assert!(
                        !(k.at >= b && *t < b),
                        "event at {} delivered early across {b} at {t}",
                        k.at
                    );
                }
            }
        }
    }
}

#[test]
fn exit_during_a_round_commits_the_round() {
    let scripts = vec![
        Script::new(0, 13, vec![Step::Busy(5_123), Step::Exit]),
        Script::new(1, 29, vec![Step::Busy(100_000)]),
    ];
    let w = assert_same_everywhere(
        &Params {
            end_on: Some(0),
            ..params()
        },
        &scripts,
        1,
        20_000,
    );
    let Outcome::Exited {
        exit_at, committed, ..
    } = w.outcome
    else {
        panic!("{:?}", w.outcome)
    };
    assert_eq!(exit_at, 5_123);
    assert_eq!(committed, w.rounds.last().unwrap().end);
    assert!(committed > exit_at);
    assert!(w.machines[1].time + params().early >= committed && w.machines[1].time <= committed);
}

#[test]
fn negative_control_a_link_below_the_bound_is_detected() {
    let p = params();
    let scripts = vec![
        Script::new(0, 10, vec![Step::Busy(100), Step::Send { to: 1, tag: 1 }]),
        Script::new(1, 10, vec![Step::Busy(5_000)]),
    ];
    for workers in [None, Some(2)] {
        let mut world = World::new(2, p.clone()).unwrap();
        let mut machines = scripts.clone();
        // Latency less early, minus one: due inside the round that sent it.
        let mut short = Short(p.latency - p.early - 1);
        let e = match workers {
            None => run_serial(&mut world, &mut machines, &mut (), &mut short, NoHook),
            Some(w) => run_parallel(&mut world, &mut machines, w, |_| Ok(()), &mut short, NoHook),
        }
        .unwrap_err();
        assert!(matches!(e, Error::Causality { from: 0, to: 1, .. }), "{e}");
        assert_eq!(world.rounds().len(), 0);
    }
}

#[test]
fn output_limit_fails_explicitly() {
    let sends = (0..10).map(|tag| Step::Send { to: 1, tag }).collect();
    let scripts = vec![Script::new(0, 10, sends), Script::new(1, 10, vec![])];
    let p = Params {
        max_outputs: 4,
        ..params()
    };
    for workers in [None, Some(2)] {
        let e = execute(&p, &scripts, 1, workers, false, 0).unwrap_err();
        assert!(
            matches!(&e, Error::Machine { machine: 0, round: 0, message } if message.contains("more than 4")),
            "{e}"
        );
    }
}

#[test]
fn a_failing_machine_aborts_without_committing_and_the_lowest_failure_wins() {
    let scripts = vec![
        Script::new(0, 10, vec![Step::Busy(3_000)]),
        Script::new(1, 10, vec![Step::Busy(1_500), Step::Fail]),
        Script::new(2, 10, vec![Step::Busy(1_500), Step::Fail]),
        Script::new(3, 10, vec![Step::Busy(9_000)]),
    ];
    for workers in [None, Some(1), Some(2), Some(4)] {
        for reverse in [false, true] {
            let mut world = World::new(4, params()).unwrap();
            let mut machines = scripts.clone();
            if reverse {
                machines[2].host_delay = Duration::ZERO;
                machines[1].host_delay = Duration::from_millis(3);
            }
            let mut links = Links::new(1, 1_000, 0, 0);
            let e = match workers {
                None => run_serial(&mut world, &mut machines, &mut (), &mut links, NoHook),
                Some(w) => {
                    run_parallel(&mut world, &mut machines, w, |_| Ok(()), &mut links, NoHook)
                }
            }
            .unwrap_err();
            assert!(
                matches!(e, Error::Machine { machine: 1, .. }),
                "{workers:?}: {e}"
            );
            let failed_round = match e {
                Error::Machine { round, .. } => round,
                _ => unreachable!(),
            };
            assert_eq!(
                world.rounds().len() as u64,
                failed_round,
                "the failed round was committed"
            );
        }
    }
}

/// A machine that panics on a pool thread fails its round like an error: the
/// round is not committed, the lowest failing machine is reported whichever
/// finishes first, and the pool shuts down instead of waiting for it.
#[test]
fn a_panicking_machine_fails_its_round_on_every_pool_size() {
    for (panicking, failing, reported) in [(2, None, 2), (2, Some(1), 1), (1, Some(2), 1)] {
        let mut scripts = vec![
            Script::new(0, 10, vec![Step::Busy(3_000)]),
            Script::new(1, 10, vec![Step::Busy(1_500)]),
            Script::new(2, 10, vec![Step::Busy(1_500)]),
            Script::new(3, 10, vec![Step::Busy(9_000)]),
        ];
        scripts[panicking].steps.push_back(Step::Panic);
        if let Some(failing) = failing {
            scripts[failing].steps.push_back(Step::Fail);
        }
        for workers in [1, 2, 4, 8] {
            for reverse in [false, true] {
                let mut world = World::new(4, params()).unwrap();
                let mut machines = scripts.clone();
                if reverse {
                    machines[1].host_delay = Duration::from_millis(3);
                }
                let mut links = Links::new(1, 1_000, 0, 0);
                let e = run_parallel(
                    &mut world,
                    &mut machines,
                    workers,
                    |_| Ok(()),
                    &mut links,
                    NoHook,
                )
                .unwrap_err();
                let label = format!("panic {panicking}, fail {failing:?}, {workers} workers");
                let Error::Machine {
                    machine,
                    round,
                    message,
                } = e
                else {
                    panic!("{label}: {e}")
                };
                assert_eq!(machine, reported, "{label}: {message}");
                if machine == panicking {
                    assert!(message.contains("panicked"), "{label}: {message}");
                }
                assert_eq!(world.rounds().len() as u64, round, "{label}");
            }
        }
    }
}

#[test]
fn a_worker_that_cannot_start_fails_admission() {
    let mut world = World::new(2, params()).unwrap();
    let mut machines = vec![Script::new(0, 10, vec![]), Script::new(1, 10, vec![])];
    let e = run_parallel(
        &mut world,
        &mut machines,
        2,
        |w| {
            if w == 1 {
                Err("no core".into())
            } else {
                Ok(())
            }
        },
        &mut Links::new(1, 1_000, 0, 0),
        NoHook,
    )
    .unwrap_err();
    assert_eq!(
        e,
        Error::Worker {
            worker: 1,
            message: "no core".into()
        }
    );
    assert_eq!(world.rounds().len(), 0);
}

#[test]
fn randomized_worlds_match_and_are_never_late() {
    for seed in 1..=40u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let n = 2 + rng.below(4) as usize;
        let mut scripts = Vec::new();
        for id in 0..n {
            let mut steps = Vec::new();
            for _ in 0..rng.below(30) {
                steps.push(match rng.below(5) {
                    0 => Step::Busy(rng.below(3_000)),
                    1 | 2 => Step::Send {
                        to: (id + 1 + rng.below(n as u64 - 1) as usize) % n,
                        tag: rng.next(),
                    },
                    3 => Step::Sleep(rng.below(5_000)),
                    _ => Step::Await(rng.below(6)),
                });
            }
            let mut s = Script::new(id, 1 + rng.below(150), steps);
            s.lazy = rng.below(2) == 0;
            if rng.below(2) == 0 {
                s = s.echo(rng.below(800), rng.below(6) as u32);
            }
            scripts.push(s);
        }
        let early = rng.below(1_000);
        let p = Params {
            latency: 1_000,
            early,
            duration: 60_000 + rng.below(60_000),
            end_on: None,
            max_outputs: 256,
        };
        let reference = execute(&p, &scripts, seed, None, false, 20_000).unwrap();
        assert_never_late(&reference, p.early);
        // 8 exceeds every world here, so some threads find no assignment.
        for workers in [1, 2, 3, 8] {
            let w = execute(&p, &scripts, seed, Some(workers), seed % 2 == 0, 20_000).unwrap();
            assert_eq!(w, reference, "seed {seed}, {workers} workers");
        }
    }
}
