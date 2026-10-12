use super::*;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
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
    clock: Arc<dyn Clock + Send + Sync>,
    reached: u64,
    executed: u64,
    sleeping: Option<Option<u64>>,
    fail_run_at: Option<u64>,
    retain_blocked_position: bool,
    input: Arc<AtomicBool>,
    ops: VecDeque<Op>,
    log: Arc<Mutex<Vec<(usize, u64, u64)>>>,
    crashed: bool,
    fail_crash: bool,
}

impl Machine for TestMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = ();

    fn run(&mut self, _: &mut (), upto: Timestamp) -> Result<StopReason, Self::Error> {
        assert!(!self.crashed, "ran a crashed machine");
        let now = self.clock.now();
        let end = upto.as_nanos();
        let previous = self.reached;
        self.reached = self.reached.max(now);
        if self.fail_run_at == Some(now) {
            return Err(std::io::Error::other("fixture adoption failure"));
        }
        self.log.lock().unwrap().push((self.name, now, end));
        if let Some(deadline) = self.sleeping {
            let input = self.input.swap(false, Ordering::Relaxed);
            if !deadline.is_some_and(|at| at <= now) && !input {
                // An unchanged blocked report may retain the position at which
                // blocking began; adoption produces no CPU work or new effects.
                if self.retain_blocked_position {
                    self.reached = previous;
                }
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
                    })
                }
                Op::Fail => return Err(std::io::Error::other("fixture execution failure")),
                Op::BadStop => {
                    return Ok(StopReason::Blocked {
                        reached: at(end + 1),
                        deadline: None,
                    })
                }
            }
        }
        Ok(StopReason::BoundaryReached)
    }

    fn crash(&mut self) -> Result<(), Self::Error> {
        assert!(!self.crashed, "crashed twice");
        if self.fail_crash {
            return Err(std::io::Error::other("fixture crash failure"));
        }
        self.crashed = true;
        Ok(())
    }

    fn reboot(&mut self, _: Timestamp) -> Result<(), Self::Error> {
        Err(std::io::Error::other("fixture reboot is unsupported"))
    }

    fn snapshot(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

fn machine(clock: Arc<dyn Clock + Send + Sync>, name: usize, ops: &[Op]) -> TestMachine {
    let reached = clock.now();
    TestMachine {
        name,
        clock,
        reached,
        executed: 0,
        sleeping: None,
        fail_run_at: None,
        retain_blocked_position: false,
        input: Arc::new(AtomicBool::new(false)),
        ops: ops.iter().copied().collect(),
        log: Arc::new(Mutex::new(Vec::new())),
        crashed: false,
        fail_crash: false,
    }
}

fn build(start: u64, programs: &[&[Op]]) -> Driver<TestMachine> {
    Driver::new(
        at(start),
        std::time::Duration::from_nanos(1000),
        2,
        || Ok(()),
        |clock| {
            Ok(programs
                .iter()
                .enumerate()
                .map(|(index, ops)| machine(clock.clone(), index, ops))
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
    driver.with_barrier(|barrier| barrier.schedule(at(time), action))
}

#[test]
fn an_ineffective_wakeup_does_not_rewind_shared_time() {
    let mut driver = Driver::new(
        at(3),
        std::time::Duration::from_nanos(1000),
        2,
        || Ok(()),
        |clock| {
            let mut guest = machine(clock, 0, &[Op::Park]);
            guest.retain_blocked_position = true;
            Ok(vec![guest])
        },
    )
    .unwrap();
    let id = ids(&mut driver)[0];
    schedule(&mut driver, 10, Action::Wake(id)).unwrap();
    assert_eq!(
        driver.step(at(20)).unwrap(),
        StepOutcome::Blocked { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.reached, 3);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(20)).unwrap(),
        StepOutcome::Blocked { at: at(10) }
    );
    driver
        .with_barrier(|barrier| {
            assert_eq!(barrier.machine(id)?.log.lock().unwrap().len(), 2);
            Ok(())
        })
        .unwrap();
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
            let added = barrier.add(|clock| {
                assert_eq!(clock.now(), 10);
                Ok(machine(clock, 1, &[Op::Work(2), Op::Exit]))
            })?;
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
        || Ok(()),
        |_| Err(std::io::Error::other("construction failed")),
    );
    assert!(matches!(failed, Err(Error::Construction(_))));
    let mut driver = build(0, &[&[Op::Exit]]);
    driver
        .with_barrier(|barrier| {
            let initial = barrier.machines().map(|(id, _)| id).collect::<Vec<_>>();
            assert!(matches!(
                barrier.add(|_| Err(std::io::Error::other("construction failed"))),
                Err(Error::Construction(_))
            ));
            assert_eq!(
                barrier.machines().map(|(id, _)| id).collect::<Vec<_>>(),
                initial
            );
            let added = barrier.add(|clock| Ok(machine(clock, 1, &[Op::Exit])))?;
            assert_eq!(added, MachineId(1));
            Ok(())
        })
        .unwrap();
    assert_eq!(
        driver.step(at(1)).unwrap(),
        StepOutcome::Finished { at: at(0) }
    );
}

#[test]
fn empty_and_blocked_are_distinct_and_do_not_poll() {
    let mut driver = build(7, &[]);
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(7) }
    );
    let id = driver
        .with_barrier(|barrier| {
            barrier.add(|clock| Ok(machine(clock, 0, &[Op::Park, Op::Work(2), Op::Exit])))
        })
        .unwrap();
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
            barrier.machine(id)?.input.store(true, Ordering::Relaxed);
            barrier.schedule(at(7), Action::Wake(id))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(7)).unwrap(),
        StepOutcome::BarrierReached { at: at(7) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(9) }
    );
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
        StepOutcome::Finished { at: at(23) }
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
            barrier.schedule(at(5), Action::Crash(id))?;
            barrier.schedule(at(5), Action::Wake(id))
        })
        .unwrap();
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(5) }
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
        StepOutcome::Finished { at: at(5) }
    );
}

#[test]
fn future_action_keeps_an_indefinitely_blocked_simulation_alive() {
    let mut driver = build(0, &[&[Op::Park]]);
    let id = ids(&mut driver)[0];
    schedule(&mut driver, 50, Action::Crash(id)).unwrap();
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::BarrierReached { at: at(10) }
    );
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(50) }
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
    let mut driver = Driver::new(
        at(3),
        std::time::Duration::from_nanos(1000),
        2,
        || Ok(()),
        |clock| {
            let mut guest = machine(clock, 0, &[Op::Fail]);
            guest.fail_crash = true;
            Ok(vec![guest])
        },
    )
    .unwrap();
    let id = ids(&mut driver)[0];
    schedule(&mut driver, 3, Action::Crash(id)).unwrap();
    assert!(matches!(driver.step(at(3)), Err(Error::Machine { .. })));
    assert!(matches!(driver.step(at(4)), Err(Error::Failed)));
    let mut driver = build(3, &[]);
    assert!(matches!(driver.step(at(2)), Err(Error::PastTime { .. })));
    assert!(matches!(driver.step(at(3)), Err(Error::Failed)));
}

#[test]
fn invalid_schedule_does_not_mutate_the_queue_or_poison_the_driver() {
    let mut driver = build(3, &[&[Op::Exit]]);
    let id = ids(&mut driver)[0];
    assert!(matches!(
        schedule(&mut driver, 2, Action::Crash(id)),
        Err(Error::PastTime { .. })
    ));
    assert!(matches!(
        schedule(&mut driver, 3, Action::Wake(MachineId(99))),
        Err(Error::UnknownMachine(_))
    ));
    assert_eq!(
        driver.step(at(10)).unwrap(),
        StepOutcome::Finished { at: at(3) }
    );
}

#[test]
fn quantum_boundaries_survive_short_steps_actions_and_additions() {
    let mut driver = Driver::new(
        at(7),
        Duration::from_nanos(10),
        2,
        || Ok(()),
        |clock| Ok(vec![machine(clock, 0, &[Op::Work(100), Op::Exit])]),
    )
    .unwrap();
    assert_eq!(
        driver.step(at(12)).unwrap(),
        StepOutcome::BarrierReached { at: at(12) }
    );
    driver
        .with_barrier(|barrier| {
            let id = barrier.add(|clock| Ok(machine(clock, 1, &[Op::Park])))?;
            barrier.schedule(at(15), Action::Crash(id))
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
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        || Ok(()),
        |clock| Ok(vec![machine(clock, 0, &[Op::Sleep(25), Op::Exit])]),
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
        StepOutcome::Finished { at: at(25) }
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
            || panic!("invalid configuration created a context"),
            |_| panic!("invalid configuration created machines")
        )
        .is_err());
    }
    let mut driver = Driver::new(
        at(u64::MAX - 3),
        Duration::from_nanos(2),
        1,
        || Ok(()),
        |clock| Ok(vec![machine(clock, 0, &[Op::Work(100)])]),
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
        let mut driver = Driver::new(
            at(0),
            Duration::from_nanos(7),
            workers,
            || Ok(()),
            |clock| {
                Ok(vec![
                    machine(
                        clock.clone(),
                        0,
                        &[Op::Work(2), Op::Sleep(15), Op::Work(3), Op::Exit],
                    ),
                    machine(
                        clock.clone(),
                        1,
                        &[Op::Sleep(4), Op::Work(1), Op::Sleep(8), Op::Exit],
                    ),
                    machine(clock, 2, &[Op::Sleep(7), Op::Fail]),
                ])
            },
        )
        .unwrap();
        schedule(&mut driver, 7, Action::Wake(MachineId(2))).unwrap();
        schedule(&mut driver, 7, Action::Crash(MachineId(2))).unwrap();
        let mut outcomes = Vec::new();
        loop {
            let outcome = driver.step(at(100)).unwrap();
            outcomes.push(outcome);
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
        // The wake preceding the crash must run, even though both are due at 7.
        assert_eq!(machines[2].1, [(2, 0, 7), (2, 7, 7)]);
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
    fn run(&mut self, _: &mut (), _: Timestamp) -> Result<StopReason, Self::Error> {
        (self.0)()
    }
    fn crash(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
    fn reboot(&mut self, _: Timestamp) -> Result<(), Self::Error> {
        Err(std::io::Error::other("fixture reboot is unsupported"))
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
        let mut driver = Driver::new(
            at(0),
            Duration::from_nanos(10),
            2,
            || Ok(()),
            |clock| {
                let waiter = gate.clone();
                let first = ProbeMachine(Box::new(move || {
                    let (lock, signal) = &*waiter;
                    let (_ready, timeout) = signal
                        .wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(5), |ready| {
                            !*ready
                        })
                        .unwrap();
                    assert!(!timeout.timed_out(), "machine calls did not overlap");
                    observed_overlap.store(true, Ordering::Relaxed);
                    assert_eq!(clock.now(), 0);
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

#[test]
fn a_failed_step_waits_for_a_still_running_peer() {
    let (entered, running) = mpsc::channel();
    let (release, continue_run) = mpsc::channel();
    let (finished, returned) = mpsc::channel();
    let mut driver = Driver::new(
        at(0),
        Duration::from_nanos(10),
        2,
        || Ok(()),
        |_| {
            Ok(vec![
                ProbeMachine(Box::new(|| Err(std::io::Error::other("fixture failure")))),
                ProbeMachine(Box::new(move || {
                    entered.send(()).unwrap();
                    continue_run.recv_timeout(Duration::from_secs(5)).unwrap();
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
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        returned.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    assert!(matches!(
        returned.recv_timeout(Duration::from_secs(5)).unwrap(),
        Err(Error::Machine {
            id: MachineId(0),
            ..
        })
    ));
    driving.join().unwrap();
}

#[path = "control_tests.rs"]
mod control_tests;

#[path = "pause_tests.rs"]
mod pause_tests;
