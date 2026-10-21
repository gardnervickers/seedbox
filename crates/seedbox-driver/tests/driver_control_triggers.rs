//! Public automatic-service ordering with timed outputs and actual inbox ACKs.
use seedbox_driver::driver::{
    Action, Driver, EventKind, MachineId, MachineResponse, StepOutcome, Target,
};
use seedbox_machine::{
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{
        ApplicationCommand, CommandResult, Output, Request, Response, SimulationControlId, Stream,
    },
    rng::RngStream,
};
use std::{collections::VecDeque, num::NonZeroU32, time::Duration};
mod support {
    pub mod driver_definition;
    pub mod input_key;
}
use input_key::auto_key;
use support::driver_definition;
use support::input_key;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn call(time: u64, id: u32, operation: serde_json::Value) -> Output {
    let mut payload = operation;
    payload["version"] = 1.into();
    payload["id"] = id.into();
    Output {
        at: at(time),
        payload: Response::SimulationControl {
            id: SimulationControlId {
                session: NonZeroU32::new(1).unwrap(),
                request: NonZeroU32::new(id).unwrap(),
            },
            payload: serde_json::to_vec(&payload).unwrap(),
        },
    }
}
fn arm() -> serde_json::Value {
    serde_json::json!({"op":"arm_trigger", "source":{"machine":"source"}, "marker":b"needle", "match_timeout_ns":20,
        "fault":{"action":"stop", "target":{"machine":"peer"}}})
}
#[derive(Clone)]
struct Fixture {
    events: VecDeque<Output>,
    ready: bool,
    crashed: bool,
    reached: u64,
    stopped: bool,
    pending: Option<(NonZeroU32, ApplicationCommand, u64)>,
    commands: Vec<(NonZeroU32, u64)>,
    acknowledgments: Vec<(NonZeroU32, u64)>,
    replies: Vec<(u32, u64, serde_json::Value)>,
}
impl Machine for Fixture {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        c: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        let now = c.clock.now();
        self.reached = self.reached.max(now);
        while let Some(input) = c.inbox.take() {
            match input.payload {
                Request::Crash => {
                    self.crashed = true;
                    self.pending = None;
                    self.events.clear();
                }
                Request::Reboot => {
                    self.crashed = false;
                    self.ready = false;
                    self.stopped = false;
                    self.pending = None;
                    self.events.clear();
                }
                Request::Application { id, command } if !self.crashed => {
                    assert!(self.pending.is_none(), "overlapping fixture commands");
                    assert!(matches!(
                        command,
                        ApplicationCommand::Stop | ApplicationCommand::Start
                    ));
                    self.commands.push((id, now));
                    // Acceptance is not completion: three units of timer work
                    // must elapse before this machine publishes the exact ACK.
                    self.pending = Some((id, command, now + 3));
                }
                Request::SimulationReply { id, payload } if !self.crashed => {
                    self.replies.push((
                        id.request.get(),
                        now,
                        serde_json::from_slice(&payload).unwrap(),
                    ));
                }
                _ => {}
            }
        }
        if self.crashed {
            return Ok(StopReason::Exited {
                reached: at(self.reached),
                reason: ExitReason::Crashed,
            });
        }
        if !self.ready && upto.as_nanos() > now {
            self.ready = true;
            c.outbox
                .emit(Output {
                    at: at(now),
                    payload: Response::Ready,
                })
                .unwrap();
        }
        if let Some((id, command, due)) = self.pending {
            if due <= now {
                self.pending = None;
                self.stopped = command == ApplicationCommand::Stop;
                self.acknowledgments.push((id, now));
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
        }
        // Scripted output requires positive execution; zero-budget command
        // admission and replies cannot manufacture new program observations.
        if upto.as_nanos() > now {
            while self.events.front().is_some_and(|e| e.at <= upto) {
                let event = self.events.pop_front().unwrap();
                assert!(event.at.as_nanos() >= now);
                self.reached = self.reached.max(event.at.as_nanos());
                c.outbox.emit(event).unwrap();
            }
        }
        if let Some((_, _, due)) = self.pending {
            return Ok(StopReason::Blocked {
                reached: at(self.reached),
                deadline: Some(at(due)),
            });
        }
        // Startup and remaining scripted work are runnable. Later idle calls
        // retain their actual position rather than pretending to execute.
        if !self.events.is_empty() || upto.as_nanos() <= 10 {
            self.reached = upto.as_nanos();
            Ok(StopReason::BoundaryReached)
        } else {
            Ok(StopReason::Blocked {
                reached: at(self.reached),
                deadline: None,
            })
        }
    }
    fn snapshot(&mut self) -> Result<Self, Self::Error> {
        Ok(self.clone())
    }
}
struct Rng;
impl RngStream for Rng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(0);
    }
}
struct World {
    driver: Driver<Fixture>,
    caller: MachineId,
    source: MachineId,
    peer: MachineId,
    trace: Vec<MachineResponse>,
}
impl World {
    fn new(workers: usize, caller: Vec<Output>, source: Vec<Output>) -> Self {
        let mut driver = Driver::new(
            at(0),
            Duration::from_nanos(10),
            workers,
            Rng,
            || Ok(()),
            driver_definition::definition("oracle"),
            ["caller", "source", "peer"]
                .map(driver_definition::definition)
                .into(),
            Vec::new(),
            seedbox_driver::driver::NetworkConfig::default(),
            move |definition| {
                Ok(Fixture {
                    events: match definition.name.as_str() {
                        "caller" => caller.clone().into(),
                        "source" => source.clone().into(),
                        _ => VecDeque::new(),
                    },
                    ready: false,
                    crashed: false,
                    reached: 0,
                    stopped: false,
                    pending: None,
                    commands: Vec::new(),
                    acknowledgments: Vec::new(),
                    replies: Vec::new(),
                })
            },
        )
        .unwrap();
        let ids: Vec<_> = driver
            .with_barrier(|b| {
                let ids: Vec<_> = b.machines().map(|(id, _)| id).collect();
                b.schedule(at(0), auto_key(), Action::Pause(Target::Machine(ids[0])))?;
                Ok(ids)
            })
            .unwrap();
        Self {
            driver,
            caller: ids[1],
            source: ids[2],
            peer: ids[3],
            trace: Vec::new(),
        }
    }
    fn step(&mut self, upto: u64) -> StepOutcome {
        let outcome = self.driver.step(at(upto)).unwrap();
        self.trace.extend(
            self.driver
                .drain_observations()
                .events
                .into_iter()
                .filter_map(|event| match event.kind {
                    EventKind::Output(output) => Some(output),
                    _ => None,
                })
                .collect::<Vec<_>>(),
        );
        outcome
    }
    fn now(&mut self) -> u64 {
        self.driver
            .with_barrier(|b| Ok(b.playback().position.at.as_nanos()))
            .unwrap()
    }
    fn advance(&mut self, upto: u64) {
        for _ in 0..100 {
            if self.now() == upto {
                return;
            }
            let before = self.now();
            let result = self.step(upto);
            assert!(matches!(
                result,
                StepOutcome::BarrierReached { .. } | StepOutcome::Blocked { .. }
            ));
            assert!(self.now() > before && self.now() <= upto, "{result:?}");
        }
        panic!("failed to reach {upto}");
    }
    fn reply(&mut self, id: u32) -> (u64, serde_json::Value) {
        self.driver
            .with_barrier(|b| {
                let matches: Vec<_> = b
                    .machine(self.caller)?
                    .replies
                    .iter()
                    .filter(|r| r.0 == id)
                    .collect();
                assert_eq!(matches.len(), 1);
                Ok((matches[0].1, matches[0].2.clone()))
            })
            .unwrap()
    }
    fn peer(&mut self) -> Fixture {
        self.driver
            .with_barrier(|b| Ok(b.machine(self.peer)?.clone()))
            .unwrap()
    }
}

#[test]
fn split_union_marker_admits_stop_at_discovery_commit_and_acknowledges_once() {
    let mut reference = None;
    for workers in [1, 2] {
        let mut request = arm();
        request["match_timeout_ns"] = 100.into();
        let mut world = World::new(
            workers,
            vec![call(1, 1, request)],
            vec![
                Output {
                    at: at(12),
                    payload: Response::BootConsole {
                        bytes: b"ne".to_vec(),
                    },
                },
                Output {
                    at: at(22),
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: b"ed".to_vec(),
                    },
                },
                Output {
                    at: at(23),
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stderr,
                        bytes: b"le".to_vec(),
                    },
                },
                Output {
                    at: at(42),
                    payload: Response::BootConsole {
                        bytes: b"needle".to_vec(),
                    },
                },
            ],
        );
        world.advance(10);
        let (received, reply) = world.reply(1);
        assert_eq!(received, 10);
        assert_eq!(reply["ok"], true);
        assert_eq!(reply["result"]["trigger_id"], 1);
        world.advance(20);
        assert!(world.peer().commands.is_empty(), "partial marker fired");
        world.advance(30);
        let peer = world.peer();
        assert_eq!(peer.commands.len(), 1);
        let command = peer.commands[0].0;
        assert_eq!(
            peer.commands[0].1, 30,
            "must not backdate admission to byte time 23"
        );
        assert!(!peer.stopped);
        assert!(peer.acknowledgments.is_empty());
        world.advance(33);
        let peer = world.peer();
        assert!(peer.stopped);
        assert_eq!(peer.acknowledgments, [(command, 33)]);
        assert!(world.trace.iter().any(|event| event.machine == world.peer && event.at == at(33)
            && matches!(event.payload, Response::CommandAcknowledged { id, result: CommandResult::Completed } if id == command)));
        // The last byte is the only remaining work: a fired trigger must not
        // keep the driver runnable toward its obsolete timeout at 110.
        world.advance(40);
        assert_eq!(world.step(50), StepOutcome::Blocked { at: at(42) });
        assert_eq!(
            world.peer().commands,
            [(command, 30)],
            "repeated marker fired again"
        );
        assert_eq!(world.peer().acknowledgments, [(command, 33)]);
        assert_eq!(world.step(200), StepOutcome::Blocked { at: at(42) });
        assert!(world.trace.iter().any(|event| event.machine == world.source
            && event.at == at(12)
            && matches!(&event.payload, Response::BootConsole { bytes } if bytes == b"ne")));
        if let Some(reference) = &reference {
            assert_eq!(&world.trace, reference);
        } else {
            reference = Some(world.trace);
        }
    }
}

#[test]
fn deadline_bytes_do_not_match_and_exact_deadline_cancel_precedes_expiry() {
    for cancel in [false, true] {
        let late_id = if cancel { 3 } else { 2 };
        let mut calls = vec![call(1, 1, arm())];
        if cancel {
            calls.push(call(
                30,
                2,
                serde_json::json!({"op":"cancel_trigger", "trigger_id":1}),
            ));
        }
        calls.push(call(
            40,
            late_id,
            serde_json::json!({"op":"cancel_trigger", "trigger_id":1}),
        ));
        let mut world = World::new(
            2,
            calls,
            vec![
                Output {
                    at: at(12),
                    payload: Response::BootConsole {
                        bytes: b"nee".to_vec(),
                    },
                },
                Output {
                    at: at(30),
                    payload: Response::ApplicationOutput {
                        stream: Stream::Stdout,
                        bytes: b"dle".to_vec(),
                    },
                },
            ],
        );
        world.advance(20);
        assert!(world.peer().commands.is_empty());
        world.advance(30);
        assert!(
            world.peer().commands.is_empty(),
            "bytes at the exact deadline cannot match"
        );
        if cancel {
            let (received, reply) = world.reply(2);
            assert_eq!(received, 30);
            assert_eq!(reply["ok"], true, "{reply}");
            assert_eq!(reply["admitted_at_ns"], 30);
        }
        world.advance(40);
        let (_, reply) = world.reply(late_id);
        assert_eq!(reply["ok"], false);
        assert_eq!(reply["error"]["code"], "invalid_state");
        assert!(world.peer().commands.is_empty());
        for _ in 0..3 {
            assert_eq!(world.step(100), StepOutcome::Blocked { at: at(40) });
        }
    }
}

#[test]
fn positive_sleep_wakes_at_admission_plus_duration_even_if_caller_exits() {
    for crash in [false, true] {
        let mut world = World::new(
            2,
            vec![call(
                1,
                1,
                serde_json::json!({"op":"sleep", "duration_ns":7}),
            )],
            vec![],
        );
        world.advance(10);
        assert!(world
            .driver
            .with_barrier(|b| Ok(b.machine(world.caller)?.replies.is_empty()))
            .unwrap());
        if crash {
            world
                .driver
                .with_barrier(|b| {
                    b.schedule(
                        at(11),
                        auto_key(),
                        Action::Crash(Target::Machine(world.caller)),
                    )
                })
                .unwrap();
        }
        world.advance(16);
        assert!(world
            .driver
            .with_barrier(|b| Ok(b.machine(world.caller)?.replies.is_empty()))
            .unwrap());
        world.advance(17);
        if crash {
            assert!(world
                .driver
                .with_barrier(|b| Ok(b.machine(world.caller)?.replies.is_empty()))
                .unwrap());
            assert!(world
                .driver
                .with_barrier(|b| Ok(matches!(
                    b.stopped(world.caller)?,
                    StopReason::Exited {
                        reason: ExitReason::Crashed,
                        ..
                    }
                )))
                .unwrap());
        } else {
            let (received, reply) = world.reply(1);
            assert_eq!(received, 17);
            assert_eq!(reply["requested_at_ns"], 1);
            assert_eq!(reply["admitted_at_ns"], 10);
            assert_eq!(reply["effect_at_ns"], 17);
            assert_eq!(reply["replied_at_ns"], 17);
        }
        for _ in 0..3 {
            assert_eq!(world.step(100), StepOutcome::Blocked { at: at(17) });
        }
    }
}

#[test]
fn accepted_peer_effect_finishes_after_its_caller_exits() {
    let mut world = World::new(
        2,
        vec![call(
            1,
            1,
            serde_json::json!({"op":"stop", "target":{"machine":"peer"}}),
        )],
        vec![],
    );
    world.advance(10);
    let peer = world.peer();
    assert_eq!(peer.commands.len(), 1);
    let command = peer.commands[0].0;
    assert_eq!(peer.commands[0].1, 10);
    assert!(!peer.stopped);
    assert!(peer.acknowledgments.is_empty());
    world
        .driver
        .with_barrier(|b| {
            b.schedule(
                at(11),
                auto_key(),
                Action::Crash(Target::Machine(world.caller)),
            )
        })
        .unwrap();
    world.advance(11);
    world.advance(13);
    assert!(world.peer().stopped);
    assert_eq!(world.peer().acknowledgments, [(command, 13)]);
    assert!(world
        .driver
        .with_barrier(|b| Ok(b.machine(world.caller)?.replies.is_empty()))
        .unwrap());
    for _ in 0..3 {
        assert_eq!(world.step(100), StepOutcome::Blocked { at: at(13) });
    }
    assert_eq!(world.peer().commands, [(command, 10)]);
}

#[test]
fn cancel_before_deadline_removes_the_only_future_wakeup() {
    let mut world = World::new(
        2,
        vec![
            call(1, 1, arm()),
            call(
                20,
                2,
                serde_json::json!({"op":"cancel_trigger", "trigger_id":1}),
            ),
        ],
        vec![],
    );
    world.advance(20);
    let (received, reply) = world.reply(2);
    assert_eq!(received, 20);
    assert_eq!(reply["ok"], true);
    assert!(world.peer().commands.is_empty());
    for _ in 0..3 {
        assert_eq!(world.step(100), StepOutcome::Blocked { at: at(20) });
    }
}
