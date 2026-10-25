use super::fixture::auto_key;
use super::*;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
};

fn at(value: u64) -> Timestamp {
    Timestamp::from_nanos(value)
}

#[derive(Clone, Copy)]
enum Op {
    Work(u64),
    Sleep(u64),
    Park,
    Exit,
    Fail,
    BadStop,
}

struct TestMachine {
    name: usize,
    reached: u64,
    executed: u64,
    sleeping: Option<Option<u64>>,
    fail_run_at: Option<u64>,
    input: Arc<AtomicBool>,
    ops: VecDeque<Op>,
    boot: VecDeque<Op>,
    lifecycle: Vec<(&'static str, u64)>,
    fail_reboot: bool,
    log: Arc<Mutex<Vec<(usize, u64, u64)>>>,
    crashed: bool,
    pending_reboots: u64,
    pending_at: Option<u64>,
    fail_crash: bool,
}

impl Machine for TestMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();

    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while let Some(input) = context.inbox.front() {
            match input.payload {
                crate::pv::Request::Crash => {
                    context.inbox.take();
                    self.crash()?;
                    return Ok(StopReason::Exited {
                        reached: Timestamp::from_nanos(context.clock.now()),
                        reason: ExitReason::Crashed,
                    });
                }
                crate::pv::Request::Reboot => {
                    context.inbox.take();
                    self.reboot(Timestamp::from_nanos(context.clock.now()))?;
                }
                crate::pv::Request::Shutdown => {
                    context.inbox.take();
                    self.shutdown()?;
                }
                crate::pv::Request::Application { .. }
                | crate::pv::Request::StepRealtime { .. }
                | crate::pv::Request::SimulationReply { .. }
                | crate::pv::Request::ReceiveFrame { .. } => break,
                _ => return Err(std::io::Error::other("unexpected fixture request")),
            }
        }

        assert!(
            !self.crashed || self.pending_reboots > 0,
            "ran a crashed machine"
        );
        let now = context.clock.now();
        if self.pending_reboots > 0 {
            self.reached = now.max(self.reached).max(self.pending_at.unwrap());
            self.sleeping = None;
            self.ops = self.boot.clone();
            self.crashed = false;
            self.pending_reboots = 0;
            self.pending_at = None;
        }
        let end = upto.as_nanos();
        self.reached = self.reached.max(now);
        if self.fail_run_at == Some(now) {
            return Err(std::io::Error::other("fixture adoption failure"));
        }
        self.log.lock().unwrap().push((self.name, now, end));
        if let Some(deadline) = self.sleeping {
            let input = self.input.swap(false, Ordering::Relaxed);
            if !deadline.is_some_and(|at| at <= now) && !input {
                return Ok(StopReason::Blocked {
                    reached: at(self.reached),
                    deadline: deadline.map(at),
                });
            }
            self.reached = self.reached.max(now);
            self.sleeping = None;
        }
        // A zero-length call can service a timer, but does no program work.
        while self.reached < end {
            match self.ops.pop_front().expect("fixture exhausted") {
                Op::Work(work) => {
                    let done = work.min(end - self.reached);
                    self.reached += done;
                    self.executed += done;
                    if done < work {
                        self.ops.push_front(Op::Work(work - done));
                    }
                }
                Op::Sleep(delay) => {
                    let deadline = self.reached + delay;
                    self.sleeping = Some(Some(deadline));
                    return Ok(StopReason::Blocked {
                        reached: at(self.reached),
                        deadline: Some(at(deadline)),
                    });
                }
                Op::Park => {
                    self.sleeping = Some(None);
                    return Ok(StopReason::Blocked {
                        reached: at(self.reached),
                        deadline: None,
                    });
                }
                Op::Exit => {
                    return Ok(StopReason::Exited {
                        reached: at(self.reached),
                        reason: ExitReason::Shutdown,
                    });
                }
                Op::Fail => return Err(std::io::Error::other("fixture execution failure")),
                Op::BadStop => {
                    return Ok(StopReason::Blocked {
                        reached: at(end + 1),
                        deadline: None,
                    });
                }
            }
        }
        Ok(StopReason::BoundaryReached)
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn machine(name: usize, ops: &[Op]) -> TestMachine {
    let reached = 0;
    TestMachine {
        name,
        reached,
        executed: 0,
        sleeping: None,
        fail_run_at: None,
        input: Arc::new(AtomicBool::new(false)),
        ops: ops.iter().copied().collect(),
        boot: ops.iter().copied().collect(),
        lifecycle: Vec::new(),
        fail_reboot: false,
        log: Arc::new(Mutex::new(Vec::new())),
        crashed: false,
        pending_reboots: 0,
        pending_at: None,
        fail_crash: false,
    }
}

fn build(start: u64, programs: &[&[Op]]) -> Driver<TestMachine> {
    super::fixture::driver(
        at(start),
        std::time::Duration::from_nanos(1000),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            Ok(programs
                .iter()
                .enumerate()
                .map(|(index, ops)| machine(index, ops))
                .collect())
        },
    )
    .unwrap()
}

fn ids(driver: &mut Driver<TestMachine>) -> Vec<MachineId> {
    driver
        .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))
        .unwrap()
}

fn schedule(
    driver: &mut Driver<TestMachine>,
    time: u64,
    action: Action,
) -> Result<(), Error<std::io::Error>> {
    driver.with_barrier(|barrier| barrier.schedule(at(time), auto_key(), action))
}

#[test]
fn initial_set_and_barrier_addition_share_the_current_time() {
    let mut driver = build(7, &[&[Op::Work(100), Op::Exit]]);
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    let first = ids(&mut driver)[0];
    let added = driver
        .with_barrier(|barrier| {
            let added = barrier.add(|| Ok(machine(1, &[Op::Work(2), Op::Exit])))?;
            assert_ne!(first, added);
            assert_eq!(
                barrier.machines().map(|(id, _)| id).collect::<Vec<_>>(),
                [first, added]
            );
            barrier.snapshot(first)?;
            Ok(added)
        })
        .unwrap();
    assert_eq!(
        driver.step(at(15)).unwrap(),
        StepOutcome::BarrierReached { at: at(15) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(added)?,
                StopReason::Exited {
                    reached: at(12),
                    reason: ExitReason::Shutdown
                }
            );
            assert_eq!(barrier.machine(first)?.reached, 15);
            Ok(())
        })
        .unwrap();
}

#[test]
fn failed_construction_preserves_existing_machines_and_id_order() {
    let failed = Driver::<TestMachine>::new(
        at(0),
        std::time::Duration::from_nanos(1000),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        super::fixture::definition(0),
        Vec::new(),
        Vec::new(),
        crate::driver::NetworkConfig::default(),
        |_| Err(std::io::Error::other("construction failed")),
    );
    assert!(matches!(failed, Err(Error::Construction(_))));
    let mut driver = build(0, &[&[Op::Exit]]);
    driver
        .with_barrier(|barrier| {
            let initial = barrier.machines().map(|(id, _)| id).collect::<Vec<_>>();
            assert!(matches!(
                barrier.add(|| Err(std::io::Error::other("construction failed"))),
                Err(Error::Construction(_))
            ));
            assert_eq!(
                barrier.machines().map(|(id, _)| id).collect::<Vec<_>>(),
                initial
            );
            let added = barrier.add(|| Ok(machine(1, &[Op::Exit])))?;
            assert_eq!(added, MachineId(1));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::Finished {
            at: at(0),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
fn blocked_oracle_does_not_poll() {
    let mut driver = build(7, &[&[Op::Park, Op::Work(2), Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(7) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Blocked { at: at(7) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
}

#[test]
fn idle_jump_respects_limit_and_delivers_timer_at_exact_boundary() {
    let mut driver = build(0, &[&[Op::Work(2), Op::Sleep(20), Op::Work(1), Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert_eq!(
        driver.step(at(5)).unwrap(),
        StepOutcome::BarrierReached { at: at(5) }
    );
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    assert_eq!(
        driver.step(at(50)).unwrap(),
        StepOutcome::BarrierReached { at: at(22) }
    );
    let calls = driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
            Ok(barrier.machine(id)?.log.lock().unwrap().len())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(22)).unwrap(),
        StepOutcome::BarrierReached { at: at(22) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), calls);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(50)).unwrap(),
        StepOutcome::Finished {
            at: at(23),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
fn new_earlier_timer_catches_up_without_rewinding_another_machine() {
    let mut driver = build(
        0,
        &[
            &[Op::Work(100), Op::Exit],
            &[Op::Work(1), Op::Sleep(2), Op::Work(1), Op::Exit],
        ],
    );
    let ids = ids(&mut driver);
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(ids[0])?.reached, 10);
            assert_eq!(
                barrier.stopped(ids[1])?,
                StopReason::Exited {
                    reached: at(4),
                    reason: ExitReason::Shutdown
                }
            );
            assert_eq!(*barrier.machine(ids[0])?.log.lock().unwrap(), [(0, 0, 10)]);
            assert_eq!(
                *barrier.machine(ids[1])?.log.lock().unwrap(),
                [(1, 0, 10), (1, 3, 10)]
            );
            Ok(())
        })
        .unwrap();
}

#[test]
fn actions_bound_execution_and_crash_precedes_a_same_time_timer() {
    let mut driver = build(0, &[&[Op::Work(1), Op::Sleep(4), Op::Fail]]);
    let id = ids(&mut driver)[0];
    driver
        .with_barrier(|barrier| {
            barrier.schedule(at(5), auto_key(), Action::Crash(Target::Machine(id)))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(5),
            reason: ExitReason::Crashed
        }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(
                barrier.stopped(id)?,
                StopReason::Exited {
                    reached: at(5),
                    reason: ExitReason::Crashed
                }
            );
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 1);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(5),
            reason: ExitReason::Crashed
        }
    );
}

#[test]
fn future_action_keeps_an_indefinitely_blocked_simulation_alive() {
    let mut driver = build(0, &[&[Op::Park]]);
    let id = ids(&mut driver)[0];
    schedule(&mut driver, 50, Action::Crash(Target::Machine(id))).unwrap();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(50),
            reason: ExitReason::Crashed
        }
    );
}

#[test]
fn execution_and_invalid_stop_failures_prevent_all_further_dispatch() {
    for op in [Op::Fail, Op::BadStop] {
        let mut driver = build(0, &[&[op], &[Op::Fail]]);
        let log = driver
            .with_barrier(|barrier| Ok(barrier.machines().nth(1).unwrap().1.log.clone()))
            .unwrap();
        assert!(driver.step(at(10)).is_err());
        assert!(matches!(driver.step(at(10)), Err(Error::Failed)));
        assert!(matches!(
            driver.with_barrier::<()>(|_| panic!("entered a failed simulation")),
            Err(Error::Failed)
        ));
        assert_eq!(log.lock().unwrap().len(), 1); // Already submitted work is joined.
    }
}

#[test]
fn crash_failure_and_backwards_step_prevent_retry() {
    // Any pool, even one larger than the world, fails the crash the same way,
    // and a retry is refused before any machine runs again.
    for workers in [1, 2, 8] {
        let peer = Arc::new(Mutex::new(Vec::new()));
        let log = peer.clone();
        let mut driver = super::fixture::driver(
            at(3),
            std::time::Duration::from_nanos(1000),
            workers,
            crate::rng::SeededRng::new(0),
            || Ok(()),
            move || {
                let mut guest = machine(0, &[Op::Fail]);
                guest.fail_crash = true;
                let mut peer = machine(1, &[Op::Park]);
                peer.log = log;
                Ok(vec![guest, peer])
            },
        )
        .unwrap();
        let id = ids(&mut driver)[0];
        schedule(&mut driver, 3, Action::Crash(Target::Machine(id))).unwrap();
        assert!(
            matches!(
                driver.step(at(3)),
                Err(Error::Machine {
                    id: MachineId(0),
                    ..
                })
            ),
            "{workers} workers"
        );
        let runs = peer.lock().unwrap().len();
        assert!(matches!(driver.step(at(4)), Err(Error::Failed)));
        assert!(matches!(driver.step(at(3)), Err(Error::Failed)));
        assert_eq!(peer.lock().unwrap().len(), runs, "{workers} workers");
    }
    let mut driver = build(3, &[&[Op::Park]]);
    assert!(matches!(driver.step(at(2)), Err(Error::PastTime { .. })));
    assert!(matches!(driver.step(at(3)), Err(Error::Failed)));
}

#[test]
fn invalid_schedule_does_not_mutate_the_queue_or_poison_the_driver() {
    let mut driver = build(3, &[&[Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert!(matches!(
        schedule(&mut driver, 2, Action::Crash(Target::Machine(id))),
        Err(Error::PastTime { .. })
    ));
    assert!(matches!(
        schedule(
            &mut driver,
            3,
            Action::Pause(Target::Machine(MachineId(99)))
        ),
        Err(Error::UnknownMachine(_))
    ));
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished {
            at: at(3),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
fn quantum_boundaries_survive_short_steps_actions_and_additions() {
    let mut driver = super::fixture::driver(
        at(7),
        Duration::from_nanos(10),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || Ok(vec![machine(0, &[Op::Work(100), Op::Exit])]),
    )
    .unwrap();
    assert_eq!(
        driver.step(at(12)).unwrap(),
        StepOutcome::BarrierReached { at: at(12) }
    );
    driver
        .with_barrier(|barrier| {
            let id = barrier.add(|| Ok(machine(1, &[Op::Park])))?;
            barrier.schedule(at(15), auto_key(), Action::Crash(Target::Machine(id)))
        })
        .unwrap();
    for end in [15, 17, 27, 37] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
        assert_eq!(
            driver.step(at(end)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
    }
}

#[test]
fn idle_time_does_not_skip_quantum_boundaries() {
    let mut driver = super::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || Ok(vec![machine(0, &[Op::Sleep(25), Op::Exit])]),
    )
    .unwrap();
    for end in [10, 20, 25] {
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::BarrierReached { at: at(end) }
        );
    }
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(25),
            reason: ExitReason::Shutdown
        }
    );
}

#[test]
fn invalid_configuration_never_calls_factories_and_overflow_never_runs_work() {
    for (start, quantum, workers) in [
        (0, Duration::ZERO, 1),
        (0, Duration::from_secs(u64::MAX), 1),
        (0, Duration::from_nanos(1), 0),
        (u64::MAX, Duration::from_nanos(1), 1),
    ] {
        assert!(Driver::<TestMachine>::new(
            at(start),
            quantum,
            workers,
            crate::rng::SeededRng::new(0),
            || panic!("invalid configuration created a context"),
            super::fixture::definition(0),
            Vec::new(),
            Vec::new(),
            crate::driver::NetworkConfig::default(),
            |_| panic!("invalid configuration created machines")
        )
        .is_err());
    }
    let mut driver = super::fixture::driver(
        at(u64::MAX - 3),
        Duration::from_nanos(2),
        1,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || Ok(vec![machine(0, &[Op::Work(100)])]),
    )
    .unwrap();
    assert_eq!(
        driver.step(at(u64::MAX)).unwrap(),
        StepOutcome::BarrierReached {
            at: at(u64::MAX - 1)
        }
    );
    let log = driver
        .with_barrier(|barrier| Ok(barrier.machine(MachineId(0))?.log.clone()))
        .unwrap();
    assert!(matches!(
        driver.step(at(u64::MAX)),
        Err(Error::TimeOverflow)
    ));
    assert_eq!(log.lock().unwrap().len(), 1);
}

#[test]
fn worker_count_preserves_barriers_machine_progress_and_action_order() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = super::fixture::driver(
            at(0),
            Duration::from_nanos(7),
            workers,
            crate::rng::SeededRng::new(0),
            || Ok(()),
            || {
                Ok(vec![
                    machine(0, &[Op::Work(2), Op::Sleep(15), Op::Work(3), Op::Exit]),
                    machine(1, &[Op::Sleep(4), Op::Work(1), Op::Sleep(8), Op::Exit]),
                    machine(2, &[Op::Sleep(7), Op::Fail]),
                ])
            },
        )
        .unwrap();
        schedule(&mut driver, 7, Action::Pause(Target::Machine(MachineId(2)))).unwrap();
        schedule(&mut driver, 7, Action::Crash(Target::Machine(MachineId(2)))).unwrap();
        let mut outcomes = Vec::new();
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome.clone());
            if matches!(outcome, StepOutcome::Finished { .. }) {
                break;
            }
            assert!(outcomes.len() < 20);
        }
        let machines = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(id, machine)| {
                        (
                            barrier.stopped(id).unwrap(),
                            machine.log.lock().unwrap().clone(),
                        )
                    })
                    .collect::<Vec<_>>())
            })
            .unwrap();
        // Both inputs' effects apply before the same-time timer: the paused
        // machine crashes without ever running its failing work.
        assert_eq!(machines[2].1, [(2, 0, 7)]);
        let observed = (outcomes, machines);
        if let Some(reference) = &reference {
            assert_eq!(&observed, reference);
        } else {
            reference = Some(observed);
        }
    }
}

// The callback lets concurrency tests coordinate host execution without giving
// production machines any dependency on worker timing.
struct ProbeMachine(Box<dyn FnMut() -> Result<StopReason, std::io::Error> + Send>);
impl Machine for ProbeMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();
    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        _: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while let Some(input) = context.inbox.front() {
            match input.payload {
                crate::pv::Request::Crash => {
                    context.inbox.take();
                    self.crash()?;
                    return Ok(StopReason::Exited {
                        reached: Timestamp::from_nanos(context.clock.now()),
                        reason: ExitReason::Crashed,
                    });
                }
                crate::pv::Request::Reboot => {
                    context.inbox.take();
                    self.reboot(Timestamp::from_nanos(context.clock.now()))?;
                }
                crate::pv::Request::Shutdown => {
                    context.inbox.take();
                    self.shutdown()?;
                }
                crate::pv::Request::Application { .. }
                | crate::pv::Request::StepRealtime { .. }
                | crate::pv::Request::SimulationReply { .. }
                | crate::pv::Request::ReceiveFrame { .. } => break,
                _ => return Err(std::io::Error::other("unexpected fixture request")),
            }
        }

        let before = context.clock.now();
        let result = (self.0)();
        assert_eq!(context.clock.now(), before);
        result
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn calls_overlap_clock_stays_fixed_and_failure_joins_the_whole_batch() {
    use std::sync::Condvar;
    for panic_first in [false, true] {
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let peer_completed = Arc::new(AtomicBool::new(false));
        let completed = peer_completed.clone();
        let overlapped = Arc::new(AtomicBool::new(false));
        let observed_overlap = overlapped.clone();
        let mut driver = super::fixture::driver(
            at(0),
            Duration::from_nanos(10),
            2,
            crate::rng::SeededRng::new(0),
            || Ok(()),
            || {
                let waiter = gate.clone();
                let first = ProbeMachine(Box::new(move || {
                    let (lock, signal) = &*waiter;
                    let (ready, _) = signal
                        .wait_timeout_while(
                            lock.lock().unwrap(),
                            Duration::from_secs(30),
                            |ready| !*ready,
                        )
                        .unwrap();
                    assert!(*ready, "peer completion signal did not arrive");
                    observed_overlap.store(true, Ordering::Relaxed);
                    if panic_first {
                        panic!("fixture panic");
                    }
                    Err(std::io::Error::other("first machine failed"))
                }));
                let second = ProbeMachine(Box::new(move || {
                    let (lock, signal) = &*gate;
                    *lock.lock().unwrap() = true;
                    signal.notify_one();
                    completed.store(true, Ordering::Relaxed);
                    Err(std::io::Error::other("second machine failed"))
                }));
                Ok(vec![first, second])
            },
        )
        .unwrap();
        let error = driver.step(at(10)).unwrap_err();
        if panic_first {
            assert!(matches!(error, Error::Panicked { id: MachineId(0) }));
        } else {
            assert!(matches!(
                error,
                Error::Machine {
                    id: MachineId(0),
                    ..
                }
            ));
        }
        assert!(peer_completed.load(Ordering::Relaxed));
        assert!(overlapped.load(Ordering::Relaxed));
        assert!(matches!(driver.step(at(10)), Err(Error::Failed)));
        assert!(matches!(
            driver.with_barrier::<()>(|_| panic!("failed barrier")),
            Err(Error::Failed)
        ));
    }
}

/// A machine that panics on a pool thread fails its round like an error, on
/// pools smaller than, equal to and larger than the world: every submitted call
/// finishes, the round is not committed, and the lowest failing machine is
/// reported whether it panicked or failed.
#[test]
fn a_panicking_machine_fails_its_round_on_every_pool_size() {
    for (panicking, failing, reported) in [(2, None, 2), (2, Some(1), 1), (1, Some(2), 1)] {
        for workers in [1, 2, 4, 8] {
            let ran = Arc::new(AtomicUsize::new(0));
            let calls = ran.clone();
            let mut driver = super::fixture::driver(
                at(0),
                Duration::from_nanos(10),
                workers,
                crate::rng::SeededRng::new(0),
                || Ok(()),
                || {
                    Ok((0..4)
                        .map(|index| {
                            let calls = calls.clone();
                            ProbeMachine(Box::new(move || {
                                calls.fetch_add(1, Ordering::SeqCst);
                                if index == panicking {
                                    panic!("fixture panic in machine {index}");
                                }
                                if Some(index) == failing {
                                    return Err(std::io::Error::other("fixture failure"));
                                }
                                Ok(StopReason::Blocked {
                                    reached: at(0),
                                    deadline: None,
                                })
                            }))
                        })
                        .collect())
                },
            )
            .unwrap();
            let before = driver.position();
            let error = driver.step(at(10)).unwrap_err();
            let label = format!("panic {panicking}, fail {failing:?}, {workers} workers");
            if reported == panicking {
                assert!(
                    matches!(error, Error::Panicked { id } if id == MachineId(reported)),
                    "{label}: {error}"
                );
            } else {
                assert!(
                    matches!(error, Error::Machine { id, .. } if id == MachineId(reported)),
                    "{label}: {error}"
                );
            }
            assert_eq!(ran.load(Ordering::SeqCst), 4, "{label}");
            assert_eq!(driver.position(), before, "{label}");
            assert!(matches!(driver.step(at(10)), Err(Error::Failed)), "{label}");
            assert_eq!(ran.load(Ordering::SeqCst), 4, "{label}");
        }
    }
}

#[test]
fn a_failed_step_waits_for_a_still_running_peer() {
    let (entered, running) = mpsc::channel();
    let (release, continue_run) = mpsc::channel();
    let (finished, returned) = mpsc::channel();
    let mut driver = super::fixture::driver(
        at(0),
        Duration::from_nanos(10),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            Ok(vec![
                ProbeMachine(Box::new(|| Err(std::io::Error::other("fixture failure")))),
                ProbeMachine(Box::new(move || {
                    entered.send(()).unwrap();
                    seedbox_test_time::HangGuard::new("driver peer entry/completion on continue_run; retain the existing loaded-host deadlock budget, never timed absence", Duration::from_secs(30)).recv(&continue_run).unwrap();
                    Ok(StopReason::BoundaryReached)
                })),
            ])
        },
    )
    .unwrap();
    let driving = std::thread::spawn(move || {
        let result = driver.step(at(10));
        finished.send(result).unwrap();
    });
    seedbox_test_time::HangGuard::new("driver peer entry/completion on running; retain the existing loaded-host deadlock budget, never timed absence", Duration::from_secs(30)).recv(&running).unwrap();
    assert!(matches!(
        returned.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    assert!(matches!(
        seedbox_test_time::HangGuard::new("driver peer entry/completion on returned; retain the existing loaded-host deadlock budget, never timed absence", Duration::from_secs(30)).recv(&returned).unwrap(),
        Err(Error::Machine {
            id: MachineId(0),
            ..
        })
    ));
    driving.join().unwrap();
}

mod control_tests;

mod pause_tests;

mod reboot_tests;

struct RandomMachine {
    bytes: Vec<[u8; 9]>,
    reached: u64,
}
impl Machine for RandomMachine {
    // Thread-affine host bookkeeping must not carry simulation entropy.
    type Context = std::rc::Rc<std::cell::Cell<usize>>;
    type Error = std::io::Error;
    type Snapshot = Vec<[u8; 9]>;
    fn run(
        &mut self,
        context: &mut RunContext<'_, Self::Context>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while let Some(input) = context.inbox.front() {
            match input.payload {
                crate::pv::Request::Crash => {
                    context.inbox.take();
                    self.crash()?;
                    return Ok(StopReason::Exited {
                        reached: Timestamp::from_nanos(context.clock.now()),
                        reason: ExitReason::Crashed,
                    });
                }
                crate::pv::Request::Reboot => {
                    context.inbox.take();
                    self.reboot(Timestamp::from_nanos(context.clock.now()))?;
                }
                crate::pv::Request::Shutdown => {
                    context.inbox.take();
                    self.shutdown()?;
                }
                crate::pv::Request::Application { .. }
                | crate::pv::Request::StepRealtime { .. }
                | crate::pv::Request::SimulationReply { .. }
                | crate::pv::Request::ReceiveFrame { .. } => break,
                _ => return Err(std::io::Error::other("unexpected fixture request")),
            }
        }

        context.worker.set(context.worker.get() + 1);
        self.reached = self.reached.max(context.clock.now());
        if self.reached < upto.as_nanos() {
            let mut bytes = [0; 9];
            context.rng.fill_bytes(&mut bytes);
            self.bytes.push(bytes);
            self.reached = upto.as_nanos();
            std::thread::yield_now();
        }
        Ok(StopReason::BoundaryReached)
    }

    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        Ok(self.bytes.clone())
    }
}

#[test]
fn machine_rng_cursors_follow_identity_across_work_stealing_and_failed_addition() {
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = super::fixture::driver(
            at(0),
            Duration::from_nanos(10),
            workers,
            crate::rng::SeededRng::new(123),
            || Ok(std::rc::Rc::new(std::cell::Cell::new(0))),
            || {
                Ok((0..7)
                    .map(|_| RandomMachine {
                        bytes: Vec::new(),
                        reached: 0,
                    })
                    .collect())
            },
        )
        .unwrap();
        driver.step(at(10)).unwrap();
        driver
            .with_barrier(|barrier| {
                assert!(barrier
                    .add(|| Err(std::io::Error::other("fixture failed addition")))
                    .is_err());
                let id = barrier.add(|| {
                    Ok(RandomMachine {
                        bytes: Vec::new(),
                        reached: 0,
                    })
                })?;
                barrier.schedule(at(20), auto_key(), Action::Pause(Target::Machine(id)))?;
                barrier.schedule(at(40), auto_key(), Action::Resume(Target::Machine(id)))?;
                // Capturing machine state never borrows or advances its RNG.
                let first = barrier.machines().next().unwrap().0;
                barrier.snapshot(first)?;
                Ok(())
            })
            .unwrap();
        while driver.now() < at(60) {
            driver.step(at(60)).unwrap();
        }
        let histories = driver
            .with_barrier(|barrier| {
                Ok(barrier
                    .machines()
                    .map(|(_, m)| m.bytes.clone())
                    .collect::<Vec<_>>())
            })
            .unwrap();
        let mut global = crate::rng::SeededRng::new(123);
        use crate::rng::{RngStream, RngStreamExt};
        for history in &histories {
            let mut child = crate::rng::SeededRng::new(global.next_u64());
            for observed in history {
                let mut expected = [0; 9];
                child.fill_bytes(&mut expected);
                assert_eq!(*observed, expected);
            }
        }
        assert_ne!(histories[0], histories[1]);
        if let Some(expected) = &reference {
            assert_eq!(&histories, expected);
        } else {
            reference = Some(histories);
        }
    }
}

#[test]
fn stale_adoption_reports_are_rejected_and_poison_before_another_dispatch() {
    // Only resume admits a zero-budget run of an unchanged program now that
    // host wakeups (Action::Wake) are removed.
    for resume in [true] {
        for exited in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = calls.clone();
            let mut driver = super::fixture::driver(
                at(0),
                Duration::from_nanos(10),
                1,
                crate::rng::SeededRng::new(0),
                || Ok(()),
                || {
                    Ok(vec![ProbeMachine(Box::new(move || {
                        let call = calls.fetch_add(1, Ordering::Relaxed);
                        if !resume && call == 0 {
                            return Ok(StopReason::Blocked {
                                reached: at(0),
                                deadline: None,
                            });
                        }
                        Ok(if exited {
                            StopReason::Exited {
                                reached: at(4),
                                reason: ExitReason::Shutdown,
                            }
                        } else {
                            StopReason::Blocked {
                                reached: at(4),
                                deadline: None,
                            }
                        })
                    }))])
                },
            )
            .unwrap();
            let id = driver
                .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
                .unwrap();
            driver
                .with_barrier(|barrier| {
                    if resume {
                        barrier.schedule(at(0), auto_key(), Action::Pause(Target::Machine(id)))?;
                    }
                    barrier.schedule(at(5), auto_key(), Action::Resume(Target::Machine(id)))
                })
                .unwrap();
            assert!(
                matches!(driver.step(at(100)), Err(Error::InvalidStop { id: rejected, .. }) if rejected == id),
                "resume={resume}, exited={exited}"
            );
            let count = observed.load(Ordering::Relaxed);
            assert_eq!(count, if resume { 1 } else { 2 });
            assert!(matches!(driver.step(at(100)), Err(Error::Failed)));
            assert_eq!(observed.load(Ordering::Relaxed), count);
            assert!(matches!(
                driver.with_barrier::<()>(|_| panic!("entered poisoned driver")),
                Err(Error::Failed)
            ));
        }
    }
}

impl TestMachine {
    fn crash(&mut self) -> Result<(), std::io::Error> {
        assert!(!self.crashed || self.pending_reboots > 0, "crashed twice");
        self.lifecycle.push(("crash", self.reached));
        if self.fail_crash {
            return Err(std::io::Error::other("fixture crash failure"));
        }
        self.crashed = true;
        self.pending_reboots = 0;
        self.pending_at = None;
        Ok(())
    }
    fn reboot(&mut self, at: Timestamp) -> Result<(), std::io::Error> {
        self.lifecycle.push(("reboot", at.as_nanos()));
        if self.fail_reboot {
            return Err(std::io::Error::other("fixture reboot failure"));
        }
        assert!(at.as_nanos() >= self.reached);
        self.pending_reboots += 1;
        self.pending_at = Some(at.as_nanos());
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), std::io::Error> {
        Err(std::io::Error::other("fixture shutdown is unsupported"))
    }
}

impl ProbeMachine {
    fn crash(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
    fn reboot(&mut self, _: Timestamp) -> Result<(), std::io::Error> {
        Err(std::io::Error::other("fixture reboot is unsupported"))
    }
    fn shutdown(&mut self) -> Result<(), std::io::Error> {
        Err(std::io::Error::other("fixture shutdown is unsupported"))
    }
}

impl RandomMachine {
    fn crash(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
    fn reboot(&mut self, at: Timestamp) -> Result<(), std::io::Error> {
        self.reached = at.as_nanos();
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), std::io::Error> {
        Ok(())
    }
}
