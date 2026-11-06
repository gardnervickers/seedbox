//! Breakpoint hits suspend a machine until the oracle resumes or crashes it,
//! or the run ends; the Driver owns the hit queue and the declared set.
use super::*;
use crate::pv::{Output, SimulationControlId};
use serde_json::{json, Value};

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
fn hit(name: &str) -> Response {
    Response::Break { name: name.into() }
}

/// A busy machine that emits scripted outputs at their times and holds at a
/// declared breakpoint the way a PV machine does.
#[derive(Clone, Default)]
struct Guest {
    script: VecDeque<(u64, Response)>,
    declared: Vec<String>,
    held: bool,
    crashed: bool,
    replies: Vec<Value>,
    /// Block request holds asked of this machine, in order.
    holds: Vec<(crate::pv::DiskOp, u64)>,
    /// Every run: (now, upto, held on entry).
    runs: Vec<(u64, u64, bool)>,
    resumed: Vec<u64>,
    /// The time of every accepted frame.
    frames: Vec<u64>,
}
impl Machine for Guest {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = c.clock.now();
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => {
                    self.crashed = true;
                    self.held = false;
                }
                Request::DeclareBreaks { names } => self.declared = names,
                Request::BreakOnBlockRequest { op, nth } => self.holds.push((op, nth)),
                Request::ResumeBreak => {
                    self.held = false;
                    self.resumed.push(now);
                }
                Request::SimulationReply { payload, .. } => {
                    self.replies.push(serde_json::from_slice(&payload).unwrap())
                }
                Request::ReceiveFrame { .. } => self.frames.push(now),
                _ => {}
            }
        }
        self.runs.push((now, upto.as_nanos(), self.held));
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: at(now),
                reason: ExitReason::Crashed,
            });
        }
        if self.held {
            return Ok(StopReason::Blocked {
                reached: at(now),
                deadline: None,
            });
        }
        while self
            .script
            .front()
            .is_some_and(|(t, _)| *t <= upto.as_nanos())
        {
            let (t, payload) = self.script.pop_front().unwrap();
            // An output scripted while held is written once the machine runs.
            let t = t.max(now);
            let suspends = match &payload {
                Response::Break { name } => Some(self.declared.contains(name)),
                &Response::BlockRequestHeld { op, nth } => Some(self.holds.contains(&(op, nth))),
                _ => None,
            };
            if let Some(suspends) = suspends {
                if !suspends {
                    continue;
                }
                self.held = true;
                c.outbox.emit(Output { at: at(t), payload }).unwrap();
                return Ok(StopReason::Blocked {
                    reached: at(t),
                    deadline: None,
                });
            }
            c.outbox.emit(Output { at: at(t), payload }).unwrap();
        }
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}

fn build(oracle: Vec<(u64, Response)>, worker: Vec<(u64, Response)>) -> Driver<Guest> {
    build_with(oracle, worker, false)
}

/// Both machines on a network with a 5 ns latency when `networked`.
fn build_with(
    oracle: Vec<(u64, Response)>,
    worker: Vec<(u64, Response)>,
    networked: bool,
) -> Driver<Guest> {
    let definition = |index: u8| {
        let mut definition = fixture::definition(index.into());
        if networked {
            definition.network = Some(
                crate::definition::NetworkInterface::new(
                    crate::definition::MacAddress::new([2, 0, 0, 0, 0, index + 1]).unwrap(),
                    std::net::Ipv4Addr::new(10, 0, 0, index + 1),
                    24,
                )
                .unwrap(),
            );
        }
        definition
    };
    let network = if networked {
        crate::driver::NetworkConfig::new(Duration::from_nanos(5), Duration::ZERO, 0).unwrap()
    } else {
        crate::driver::NetworkConfig::default()
    };
    let mut scripts = vec![oracle, worker].into_iter();
    let mut driver = Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        1,
        7,
        || Ok(()),
        definition(0),
        vec![definition(1)],
        vec![],
        network,
        move |_| {
            Ok(Guest {
                script: scripts.next().unwrap().into(),
                ..Default::default()
            })
        },
    )
    .unwrap();
    driver.set_engine(pv());
    driver
}
/// A PV engine, whose machines hold at breakpoints.
fn pv() -> faults::Engine {
    faults::Engine {
        backend: seedbox_spec::backend::CpuBackend::Pv,
        architecture: seedbox_spec::definition::Architecture::Aarch64,
    }
}
/// Step until `upto`, the oracle finishes or nothing can advance.
fn run_to(driver: &mut Driver<Guest>, upto: u64) {
    while driver.now() < at(upto) {
        match driver.step(at(upto)).unwrap() {
            StepOutcome::BarrierReached { .. } => {}
            _ => break,
        }
    }
}
fn guest(driver: &Driver<Guest>, id: usize) -> &Guest {
    driver.execution.machines[id].machine.as_ref().unwrap()
}
fn replies(driver: &Driver<Guest>) -> Vec<Value> {
    guest(driver, 0).replies.clone()
}
fn break_events(events: &[Event]) -> Vec<(u64, EventKind)> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::BreakHit { .. } | EventKind::BreakEnded { .. }
            )
        })
        .map(|e| (e.at.as_nanos(), e.kind.clone()))
        .collect()
}
const DECLARE: &str = r#""op":"declare_breaks","names":["leader"]"#;
const WAIT: &str = r#""op":"wait_break","timeout_ns":1000"#;

#[test]
fn declared_hit_suspends_until_the_oracle_resumes_it() {
    let mut driver = build(
        vec![
            (1, call(1, DECLARE)),
            (2, call(2, WAIT)),
            (50, call(3, r#""op":"resume_break","break_id":1"#)),
        ],
        vec![
            (25, hit("leader")),
            (26, hit("follower")),
            (70, hit("leader")),
        ],
    );
    run_to(&mut driver, 100);
    let r = replies(&driver);
    assert_eq!(r[0]["result"], json!({}));
    assert_eq!(
        r[1]["result"],
        json!({"break_id":1,"source":"program","machine":"fixture-1","boot":0,"name":"leader","at_ns":25})
    );
    assert_eq!(r[2]["result"], json!({}));
    let worker = guest(&driver, 1);
    assert_eq!(worker.declared, ["leader"]);
    // Held from the hit until the resume: only zero-budget runs, if any.
    assert!(worker
        .runs
        .iter()
        .filter(|(_, _, held)| *held)
        .all(|(now, upto, _)| now == upto));
    assert_eq!(worker.resumed, [50]);
    // Multi-shot: the second write of the name suspends it again.
    assert!(worker.held);
    let events = driver.drain_events();
    assert_eq!(
        break_events(&events),
        [
            (
                25,
                EventKind::BreakHit {
                    break_id: 1,
                    machine: MachineId(1),
                    boot: 0,
                    name: "leader".into()
                }
            ),
            (
                50,
                EventKind::BreakEnded {
                    break_id: 1,
                    reason: BreakEnd::Resumed
                }
            ),
            (
                70,
                EventKind::BreakHit {
                    break_id: 2,
                    machine: MachineId(1),
                    boot: 0,
                    name: "leader".into()
                }
            ),
        ]
    );
    // No checkpoint while a machine is suspended.
    assert!(matches!(
        driver.checkpoint(),
        Err(Error::InvalidCheckpoint(_))
    ));
}

#[test]
fn a_suspended_receiver_admits_frames_at_arrival_without_a_budget() {
    let frame = Response::TransmitFrame {
        bytes: vec![2, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 1, 0x88, 0xb5, 9],
    };
    let mut driver = build_with(
        vec![
            (1, call(1, DECLARE)),
            (30, frame),
            (50, call(2, r#""op":"resume_break","break_id":1"#)),
        ],
        vec![(25, hit("leader"))],
        true,
    );
    run_to(&mut driver, 100);
    let worker = guest(&driver, 1);
    assert_eq!(worker.frames, [35]);
    assert!(worker
        .runs
        .iter()
        .filter(|(_, _, held)| *held)
        .all(|(now, upto, _)| now == upto));
    assert_eq!(worker.resumed, [50]);
}

#[test]
fn a_suspended_machine_only_crashes_and_its_hit_ends() {
    let mut driver = build(
        vec![
            (1, call(1, DECLARE)),
            (
                40,
                call(2, r#""op":"stop","target":{"machine":"fixture-1"}"#),
            ),
            (
                41,
                call(3, r#""op":"pause","target":{"machine":"fixture-1"}"#),
            ),
            (
                42,
                call(4, r#""op":"crash","target":{"machine":"fixture-1"}"#),
            ),
            (60, call(5, r#""op":"resume_break","break_id":1"#)),
            (61, call(6, r#""op":"resume_break","break_id":9"#)),
        ],
        vec![(25, hit("leader"))],
    );
    run_to(&mut driver, 100);
    let r = replies(&driver);
    for refused in [&r[1], &r[2]] {
        assert_eq!(refused["error"]["code"], "invalid_state");
    }
    assert_eq!(r[3]["ok"], true);
    assert_eq!(r[4]["error"]["message"], "breakpoint hit has already ended");
    assert_eq!(r[5]["error"]["message"], "unknown break_id");
    assert!(guest(&driver, 1).resumed.is_empty());
    let events = driver.drain_events();
    assert!(break_events(&events).iter().any(|(_, kind)| *kind
        == EventKind::BreakEnded {
            break_id: 1,
            reason: BreakEnd::Crashed
        }));
}

#[test]
fn a_waiter_times_out_and_a_later_hit_is_queued_for_the_next_wait() {
    let mut driver = build(
        vec![
            (1, call(1, DECLARE)),
            (2, call(2, r#""op":"wait_break","timeout_ns":10"#)),
            (40, call(3, WAIT)),
        ],
        vec![(25, hit("leader"))],
    );
    run_to(&mut driver, 100);
    let r = replies(&driver);
    assert_eq!(r[1]["result"], json!({}));
    // Admitted at the commit barrier after the call, then 10 ns.
    assert_eq!(r[1]["replied_at_ns"], 20);
    assert_eq!(r[2]["result"]["break_id"], 1);
    assert_eq!(r[2]["replied_at_ns"], 40);
}

#[test]
fn oracle_exit_ends_hits_without_resuming() {
    let mut driver = build(
        vec![
            (1, call(1, DECLARE)),
            (40, Response::ApplicationOutcome { code: 0 }),
        ],
        vec![(25, hit("leader"))],
    );
    run_to(&mut driver, 100);
    assert!(guest(&driver, 1).held);
    let events = driver.drain_events();
    assert!(matches!(
        break_events(&events).last(),
        Some((
            _,
            EventKind::BreakEnded {
                break_id: 1,
                reason: BreakEnd::RunEnded
            }
        ))
    ));
}

#[test]
fn undeclared_names_and_engines_without_breakpoints_never_suspend() {
    let mut driver = build(
        vec![(1, call(1, r#""op":"declare_breaks","names":["Bad Name"]"#))],
        vec![(25, hit("leader"))],
    );
    run_to(&mut driver, 100);
    assert_eq!(replies(&driver)[0]["error"]["code"], "invalid_request");
    assert!(!guest(&driver, 1).held);

    let mut scripts = vec![vec![(1, call(1, DECLARE))], vec![]].into_iter();
    let mut driver = Driver::new_seeded(
        at(0),
        Duration::from_nanos(10),
        1,
        7,
        || Ok(()),
        fixture::definition(0),
        vec![fixture::definition(1)],
        vec![],
        crate::driver::NetworkConfig::default(),
        move |_| {
            Ok(Guest {
                script: scripts.next().unwrap().into(),
                ..Default::default()
            })
        },
    )
    .unwrap();
    run_to(&mut driver, 20);
    assert_eq!(replies(&driver)[0]["error"]["code"], "unsupported");
    assert!(guest(&driver, 1).declared.is_empty());
}

#[test]
fn declarations_reach_machines_and_survive_a_checkpoint() {
    let mut driver = build(vec![(1, call(1, DECLARE))], vec![]);
    run_to(&mut driver, 20);
    let checkpoint = driver.checkpoint().unwrap();
    let restored = Driver::<Guest>::from_checkpoint(
        &checkpoint,
        1,
        || Ok(()),
        |saved| Ok(saved.clone()),
        |_| unreachable!("no spawns"),
    );
    let restored = restored.map_err(|e| e.to_string()).unwrap();
    assert_eq!(guest(&restored, 1).declared, ["leader"]);
    assert_eq!(
        restored.execution.control.service.breaks.declared(),
        ["leader"]
    );
}

#[test]
fn a_block_request_hit_suspends_until_resumed() {
    use crate::pv::DiskOp;
    let mut driver = build(
        vec![
            (
                1,
                call(
                    1,
                    r#""op":"break_on_block_request","target":{"machine":"fixture-1"},"request":"flush","nth":3"#,
                ),
            ),
            (2, call(2, WAIT)),
            (50, call(3, r#""op":"resume_break","break_id":1"#)),
            (
                51,
                call(
                    4,
                    r#""op":"break_on_block_request","target":{"machine":"fixture-1"},"request":"flush","nth":0"#,
                ),
            ),
        ],
        vec![(
            25,
            Response::BlockRequestHeld {
                op: DiskOp::Flush,
                nth: 3,
            },
        )],
    );
    run_to(&mut driver, 100);
    let r = replies(&driver);
    assert_eq!(r[0]["result"], json!({}));
    assert_eq!(
        r[1]["result"],
        json!({"break_id":1,"source":"block_request","machine":"fixture-1","boot":0,"name":"block_request","request":"flush","nth":3,"at_ns":25})
    );
    assert_eq!(r[2]["result"], json!({}));
    assert_eq!(r[3]["error"]["code"], "invalid_request");
    let worker = guest(&driver, 1);
    assert_eq!(worker.holds, [(DiskOp::Flush, 3)]);
    assert!(worker
        .runs
        .iter()
        .filter(|(_, _, held)| *held)
        .all(|(now, upto, _)| now == upto));
    assert_eq!(worker.resumed, [50]);
    assert!(!worker.held);
    let events = driver.drain_events();
    assert_eq!(
        break_events(&events),
        [
            (
                25,
                EventKind::BreakHit {
                    break_id: 1,
                    machine: MachineId(1),
                    boot: 0,
                    name: "block_request".into()
                }
            ),
            (
                50,
                EventKind::BreakEnded {
                    break_id: 1,
                    reason: BreakEnd::Resumed
                }
            ),
        ]
    );
}

#[test]
fn block_request_breakpoints_need_an_engine_with_breakpoints() {
    // Told no engine, or told TCG, which has no guest breakpoint device.
    let tcg = faults::Engine {
        backend: seedbox_spec::backend::CpuBackend::Tcg,
        architecture: seedbox_spec::definition::Architecture::X86_64,
    };
    for engine in [None, Some(tcg)] {
        let mut scripts = vec![
        vec![(
            1,
            call(
                1,
                r#""op":"break_on_block_request","target":{"machine":"fixture-1"},"request":"any","nth":1"#,
            ),
        )],
        vec![],
    ]
    .into_iter();
        let mut driver = Driver::new_seeded(
            at(0),
            Duration::from_nanos(10),
            1,
            7,
            || Ok(()),
            fixture::definition(0),
            vec![fixture::definition(1)],
            vec![],
            crate::driver::NetworkConfig::default(),
            move |_| {
                Ok(Guest {
                    script: scripts.next().unwrap().into(),
                    ..Default::default()
                })
            },
        )
        .unwrap();
        if let Some(engine) = engine {
            driver.set_engine(engine);
        }
        run_to(&mut driver, 20);
        assert_eq!(replies(&driver)[0]["error"]["code"], "unsupported");
        assert!(guest(&driver, 1).holds.is_empty());
    }
}

#[test]
fn a_block_request_hit_reached_on_resume_suspends_at_once() {
    use crate::pv::DiskOp;
    let mut driver = build(
        vec![
            (
                1,
                call(
                    1,
                    r#""op":"break_on_block_request","target":{"machine":"fixture-1"},"request":"flush","nth":3"#,
                ),
            ),
            (
                1,
                call(
                    2,
                    r#""op":"break_on_block_request","target":{"machine":"fixture-1"},"request":"write","nth":1"#,
                ),
            ),
            (2, call(3, WAIT)),
            (50, call(4, r#""op":"resume_break","break_id":1"#)),
            (51, call(5, WAIT)),
            (80, call(6, r#""op":"resume_break","break_id":2"#)),
        ],
        // The resumed FLUSH lets the WRITE behind it reach the second hold
        // before the guest runs again.
        vec![
            (
                25,
                Response::BlockRequestHeld {
                    op: DiskOp::Flush,
                    nth: 3,
                },
            ),
            (
                26,
                Response::BlockRequestHeld {
                    op: DiskOp::Write,
                    nth: 1,
                },
            ),
        ],
    );
    run_to(&mut driver, 60);
    let r = replies(&driver);
    assert_eq!(r[3]["result"], json!({}));
    assert_eq!(
        r[4]["result"],
        json!({"break_id":2,"source":"block_request","machine":"fixture-1","boot":0,"name":"block_request","request":"write","nth":1,"at_ns":50})
    );
    // Suspended again at the release: still no checkpoint.
    assert!(matches!(
        driver.checkpoint(),
        Err(Error::InvalidCheckpoint(_))
    ));
    run_to(&mut driver, 100);
    assert_eq!(guest(&driver, 1).resumed, [50, 80]);
    assert!(!guest(&driver, 1).held);
    let events = driver.drain_events();
    assert_eq!(
        break_events(&events),
        [
            (
                25,
                EventKind::BreakHit {
                    break_id: 1,
                    machine: MachineId(1),
                    boot: 0,
                    name: "block_request".into()
                }
            ),
            (
                50,
                EventKind::BreakEnded {
                    break_id: 1,
                    reason: BreakEnd::Resumed
                }
            ),
            (
                50,
                EventKind::BreakHit {
                    break_id: 2,
                    machine: MachineId(1),
                    boot: 0,
                    name: "block_request".into()
                }
            ),
            (
                80,
                EventKind::BreakEnded {
                    break_id: 2,
                    reason: BreakEnd::Resumed
                }
            ),
        ]
    );
    driver.checkpoint().unwrap();
}

#[test]
fn a_resume_admitted_behind_a_crash_of_its_machine_does_nothing() {
    // A nemesis crashes the machine while a handler resumes its hit, both
    // admitted at one barrier: the crash runs first and ends the boot.
    let mut driver = build(
        vec![
            (1, call(1, DECLARE)),
            (
                40,
                call(2, r#""op":"crash","target":{"machine":"fixture-1"}"#),
            ),
            (40, call(3, r#""op":"resume_break","break_id":1"#)),
        ],
        vec![(25, hit("leader"))],
    );
    run_to(&mut driver, 100);
    let r = replies(&driver);
    assert_eq!(r[1]["ok"], true);
    assert_eq!(r[2]["ok"], true);
    let worker = guest(&driver, 1);
    assert!(worker.crashed);
    assert!(worker.resumed.is_empty());
    let events = driver.drain_events();
    assert_eq!(
        break_events(&events)
            .into_iter()
            .filter(|(_, kind)| matches!(kind, EventKind::BreakEnded { .. }))
            .count(),
        1
    );
}
