//! Public guest control calls with timed outputs and actual inbox ACKs.
use seedbox_driver::driver::{Action, Driver, MachineId, StepOutcome, Target};
use seedbox_driver::test_support::auto_key;
use seedbox_machine::{
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Output, Request, Response, SimulationControlId},
    rng::RngStream,
};
use std::{collections::VecDeque, num::NonZeroU32, time::Duration};
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
    peer: MachineId,
}
impl World {
    fn new(caller: Vec<Output>) -> Self {
        let mut driver = Driver::new(
            at(0),
            Duration::from_nanos(10),
            Rng,
            || Ok(()),
            seedbox_driver::test_support::definition("oracle"),
            ["caller", "peer"]
                .map(seedbox_driver::test_support::definition)
                .into(),
            Vec::new(),
            seedbox_driver::test_support::floor(Duration::from_nanos(10)),
            move |definition| {
                Ok(Fixture {
                    events: match definition.name.as_str() {
                        "caller" => caller.clone().into(),
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
            peer: ids[2],
        }
    }
    fn step(&mut self, upto: u64) -> StepOutcome {
        let outcome = self.driver.step(at(upto)).unwrap();
        self.driver.drain_observations();
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
fn positive_sleep_wakes_at_admission_plus_duration_even_if_caller_exits() {
    for crash in [false, true] {
        // Admitted one floor (10 ns) after it is made.
        let mut world = World::new(vec![call(
            0,
            1,
            serde_json::json!({"op":"sleep", "duration_ns":7}),
        )]);
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
            assert_eq!(reply["requested_at_ns"], 0);
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
    // Admitted one floor (10 ns) after it is made.
    let mut world = World::new(vec![call(
        0,
        1,
        serde_json::json!({"op":"stop", "target":{"machine":"peer"}}),
    )]);
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
    // The ACK is due at 13: the peer publishes it in its run from there. The
    // pending stop keeps that round going to its quantum end, and the reply
    // to the crashed caller, due one floor after the ACK, is dropped at 23.
    assert_eq!(world.step(100), StepOutcome::BarrierReached { at: at(20) });
    assert_eq!(world.step(100), StepOutcome::Blocked { at: at(23) });
    assert!(world.peer().stopped);
    assert_eq!(world.peer().acknowledgments, [(command, 13)]);
    assert!(world
        .driver
        .with_barrier(|b| Ok(b.machine(world.caller)?.replies.is_empty()))
        .unwrap());
    for _ in 0..3 {
        assert_eq!(world.step(100), StepOutcome::Blocked { at: at(23) });
    }
    assert_eq!(world.peer().commands, [(command, 10)]);
}
