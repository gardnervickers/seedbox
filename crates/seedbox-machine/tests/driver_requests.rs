use seedbox_machine::{
    driver::{Action, Driver, Error, MachineId},
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    pv::{ApplicationCommand, CommandResult, Request, Response},
    rng::RngStream,
};
use std::{io, num::NonZeroU32, time::Duration};
#[derive(Clone, Default)]
struct Fixture {
    boot: u64,
    resets: usize,
    work: u64,
    exit: Option<ExitReason>,
    fail: bool,
}
impl Machine for Fixture {
    type Context = ();
    type Error = io::Error;
    type Snapshot = Self;
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, io::Error> {
        let now = Timestamp::from_nanos(context.clock.now());
        while let Some(input) = context.inbox.take() {
            match input.payload {
                Request::Reboot => {
                    self.boot += 1;
                    self.resets += 1;
                    self.exit = None;
                    let mut byte = [0];
                    context.rng.fill_bytes(&mut byte);
                }
                Request::Crash => {
                    if self.exit.is_none() {
                        self.exit = Some(ExitReason::Crashed);
                    }
                }
                Request::Shutdown => {}
                Request::Application { id, .. } if self.exit.is_none() => {
                    context
                        .outbox
                        .emit(seedbox_machine::pv::Output {
                            at: now,
                            payload: Response::CommandAcknowledged {
                                id,
                                result: CommandResult::Completed,
                            },
                        })
                        .unwrap();
                }
                _ => {}
            }
        }
        if self.fail {
            return Err(io::Error::other("fixture failure"));
        }
        if let Some(reason) = self.exit {
            return Ok(StopReason::Exited {
                reached: now,
                reason,
            });
        }
        self.work += upto.as_nanos() - now.as_nanos();
        Ok(StopReason::BoundaryReached)
    }
    fn snapshot(&mut self) -> Result<Self, io::Error> {
        Ok(self.clone())
    }
}
#[path = "support/driver_definition.rs"]
mod driver_definition;
struct Rng;
impl RngStream for Rng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        bytes.fill(7);
    }
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn make() -> (Driver<Fixture>, MachineId) {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        vec![driver_definition::definition("target")],
        |_| Ok(Fixture::default()),
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().nth(1).unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            let oracle = barrier.machines().next().unwrap().0;
            barrier.schedule(at(0), Action::Pause(oracle))
        })
        .unwrap();
    (driver, id)
}
fn app(id: u32) -> Request {
    Request::Application {
        id: NonZeroU32::new(id).unwrap(),
        command: ApplicationCommand::Start,
    }
}
#[test]
fn responses_keep_emitting_boot_across_reused_ids_and_drain_once() {
    let (mut driver, id) = make();
    driver
        .with_barrier(|barrier| {
            barrier.schedule_request(at(0), id, app(1))?;
            barrier.schedule_request(at(0), id, Request::Reboot)?;
            barrier.schedule_request(at(0), id, app(1))
        })
        .unwrap();
    driver.step(at(0)).unwrap();
    let outputs = driver
        .with_barrier(|barrier| Ok(barrier.drain_responses()))
        .unwrap();
    assert_eq!(
        outputs
            .iter()
            .map(|output| (output.machine, output.boot, output.at))
            .collect::<Vec<_>>(),
        [(id, 0, at(0)), (id, 1, at(0))]
    );
    assert!(outputs.iter().all(|output|matches!(output.payload,Response::CommandAcknowledged { id,result:CommandResult::Completed } if id.get()==1)));
    assert!(
        driver
            .with_barrier(|barrier| Ok(barrier.drain_responses()))
            .unwrap()
            .is_empty()
    );
}
#[test]
fn paused_and_exited_requests_share_action_order_and_reboots_are_distinct() {
    let (mut driver, id) = make();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), Action::Pause(id))?;
            barrier.schedule_request(at(0), id, app(1))?;
            barrier.schedule_request(at(0), id, Request::Crash)?;
            barrier.schedule_request(at(0), id, app(2))?;
            barrier.schedule(at(0), Action::Reboot(id))?;
            barrier.schedule_request(at(0), id, Request::Reboot)?;
            barrier.schedule_request(at(0), id, app(3))
        })
        .unwrap();
    driver.step(at(0)).unwrap();
    driver
        .with_barrier(|barrier| {
            assert!(!barrier.paused(id)?);
            assert_eq!(barrier.machine(id)?.resets, 2);
            assert_eq!(barrier.machine(id)?.work, 0);
            let outputs = barrier.drain_responses();
            assert_eq!(
                outputs.iter().map(|output| output.boot).collect::<Vec<_>>(),
                [0, 2]
            );
            Ok(())
        })
        .unwrap();
}
#[test]
fn action_and_request_lifecycle_have_equivalent_effects() {
    for request_mode in [false, true] {
        let (mut driver, id) = make();
        driver
            .with_barrier(|barrier| {
                for (request, action) in [
                    (Request::Crash, Action::Crash(id)),
                    (Request::Reboot, Action::Reboot(id)),
                    (Request::Shutdown, Action::Shutdown(id)),
                ] {
                    if request_mode {
                        barrier.schedule_request(at(0), id, request)?;
                    } else {
                        barrier.schedule(at(0), action)?;
                    }
                }
                Ok(())
            })
            .unwrap();
        driver.step(at(0)).unwrap();
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.machine(id)?.resets, 1);
                assert_eq!(barrier.machine(id)?.exit, None);
                assert_eq!(barrier.machine(id)?.work, 0);
                Ok(())
            })
            .unwrap();
    }
}
#[test]
fn request_schedule_uses_barrier_time_and_mutation_validation() {
    let (mut driver, id) = make();
    driver.step(at(3)).unwrap();
    driver
        .with_barrier(|barrier| {
            assert!(matches!(
                barrier.schedule_request(at(2), id, app(1)),
                Err(Error::PastTime { .. })
            ));
            barrier.stop();
            assert!(matches!(
                barrier.schedule_request(at(3), id, app(1)),
                Err(Error::Stopped)
            ));
            Ok(())
        })
        .unwrap();
}

#[test]
fn request_validation_rejects_foreign_identity_and_held_barrier() {
    let (mut driver, id) = make();
    let (mut other, _) = make();
    let foreign = other
        .with_barrier(|barrier| barrier.add(|| Ok(Fixture::default())))
        .unwrap();
    driver.with_barrier(|barrier| { assert!(matches!(barrier.schedule_request(at(0),foreign,app(1)),Err(Error::UnknownMachine(found)) if found==foreign)); barrier.schedule_request(at(0),id,Request::Crash)?; barrier.pause_before_next_crash(seedbox_machine::driver::PlaybackRate::Unlimited) }).unwrap();
    assert!(matches!(
        driver.step(at(0)).unwrap(),
        seedbox_machine::driver::StepOutcome::BeforeAction { .. }
    ));
    driver
        .with_barrier(|barrier| {
            assert!(matches!(
                barrier.schedule_request(at(0), id, Request::Reboot),
                Err(Error::BeforeAction)
            ));
            assert!(barrier.drain_responses().is_empty());
            Ok(())
        })
        .unwrap();
}

#[test]
fn failed_run_poisons_before_any_response_can_be_observed() {
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        Rng,
        || Ok(()),
        driver_definition::definition("oracle"),
        Vec::new(),
        |_| {
            Ok(Fixture {
                fail: true,
                ..Fixture::default()
            })
        },
    )
    .unwrap();
    let id = driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap();
    driver
        .with_barrier(|barrier| barrier.schedule_request(at(0), id, app(1)))
        .unwrap();
    assert!(matches!(driver.step(at(0)), Err(Error::Machine { .. })));
    assert!(matches!(
        driver.with_barrier(|barrier| Ok(barrier.drain_responses())),
        Err(Error::Failed)
    ));
    assert!(matches!(driver.step(at(0)), Err(Error::Failed)));
}

#[test]
fn same_time_separate_commits_preserve_publication_sequence() {
    let (mut driver, first) = make();
    let second = driver
        .with_barrier(|barrier| barrier.add(|| Ok(Fixture::default())))
        .unwrap();
    driver
        .with_barrier(|barrier| {
            barrier.schedule_request(at(0), second, app(1))?;
            barrier.schedule_request(at(0), first, app(1))
        })
        .unwrap();
    driver.step(at(0)).unwrap();
    let responses = driver
        .with_barrier(|barrier| Ok(barrier.drain_responses()))
        .unwrap();
    assert_eq!(
        responses
            .iter()
            .map(|response| response.machine)
            .collect::<Vec<_>>(),
        [second, first]
    );
}

#[test]
fn normalized_requests_preserve_same_time_controls_and_held_crash_cursor() {
    use seedbox_machine::driver::{PlaybackRate, StepOutcome};
    let (mut driver, id) = make();
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(0), Action::Pause(id))?;
            barrier.schedule_request(at(0), id, app(1))?;
            barrier.schedule(at(0), Action::Crash(id))?;
            barrier.schedule_request(at(0), id, Request::Reboot)?;
            barrier.schedule_request(at(0), id, app(2))?;
            barrier.pause_before_next_crash(PlaybackRate::Unlimited)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(0)).unwrap(),
        StepOutcome::BeforeAction {
            at: at(0),
            action: Action::Crash(id)
        }
    );
    driver.with_barrier(|barrier| {
        assert!(barrier.paused(id)?);
        assert_eq!(barrier.machine(id)?.resets, 0);
        assert_eq!(barrier.machine(id)?.work, 0);
        let responses = barrier.drain_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].boot, 0);
        assert!(matches!(responses[0].payload, Response::CommandAcknowledged { id, .. } if id.get() == 1));
        barrier.set_rate(PlaybackRate::Unlimited)
    }).unwrap();
    driver.step(at(0)).unwrap();
    driver.with_barrier(|barrier| {
        assert!(!barrier.paused(id)?);
        assert_eq!(barrier.machine(id)?.resets, 1);
        assert_eq!(barrier.machine(id)?.work, 0);
        let responses = barrier.drain_responses();
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].boot, 1);
        assert!(matches!(responses[0].payload, Response::CommandAcknowledged { id, .. } if id.get() == 2));
        Ok(())
    }).unwrap();
}
