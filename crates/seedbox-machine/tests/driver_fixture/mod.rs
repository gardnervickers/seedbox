#![allow(dead_code, unused_imports)]
//! Test machines implemented solely through seedbox-machine's public contracts.

#[path = "../support/driver_definition.rs"]
mod driver_definition;
pub use driver_definition::definition;
#[path = "../support/input_key.rs"]
mod input_key;
pub use input_key::{auto_key, key};

pub use seedbox_machine::{
    clock::Clock,
    driver::{
        Action, Driver, Error, EventKind, FaultAction, FaultOrigin, InputKey, MachineId,
        PlaybackRate, PlaybackStatus, SettlementStatus, StepOutcome, Target,
    },
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    rng::RngStream,
};
pub use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSnapshot {
    pub reached: u64,
    pub executed: u64,
    pub sleeping: Option<Option<u64>>,
    pub ops: VecDeque<Op>,
    pub boot: VecDeque<Op>,
    pub crashed: bool,
    pub exited: bool,
    pub shutdown_requested: bool,
    pub shutdown_delivered: bool,
    pub shutdown_work_remaining: u64,
    pub pending_boots: u64,
    pub pending_at: Option<u64>,
}

pub fn at(value: u64) -> Timestamp {
    Timestamp::from_nanos(value)
}

#[derive(Clone)]
pub struct TestRng(pub u64);
impl RngStream for TestRng {
    fn fill_bytes(&mut self, bytes: &mut [u8]) {
        for byte in bytes {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1);
            *byte = (self.0 >> 56) as u8;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Work(u64),
    Sleep(u64),
    Park,
    Exit,
    Fail,
    BadStop,
}

#[derive(Clone)]
pub struct TestMachine {
    pub name: usize,
    pub reached: u64,
    pub executed: u64,
    pub sleeping: Option<Option<u64>>,
    pub fail_run_at: Option<u64>,
    pub retain_blocked_position: bool,
    pub input: Arc<AtomicBool>,
    pub ops: VecDeque<Op>,
    pub boot: VecDeque<Op>,
    pub lifecycle: Vec<&'static str>,
    pub fail_reboot: bool,
    pub fail_shutdown: bool,
    pub shutdown_requested: bool,
    pub shutdown_delivered: bool,
    pub shutdown_latches: u64,
    pub shutdown_deliveries: u64,
    pub shutdown_work_remaining: u64,
    pub pending_boots: u64,
    pub pending_at: Option<u64>,
    pub boot_times: Vec<u64>,
    pub exited: bool,
    pub log: Arc<Mutex<Vec<(usize, u64, u64)>>>,
    pub crashed: bool,
    pub fail_crash: bool,
}

impl Machine for TestMachine {
    type Context = ();
    type Error = std::io::Error;
    type Snapshot = TestSnapshot;

    fn run(
        &mut self,
        context: &mut RunContext<'_, ()>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        while let Some(input) = context.inbox.front() {
            match input.payload {
                seedbox_machine::pv::Request::Crash => {
                    context.inbox.take();
                    if (self.crashed || self.exited) && self.pending_boots == 0 {
                        return Ok(StopReason::Exited {
                            reached: at(context.clock.now()),
                            reason: if self.crashed {
                                ExitReason::Crashed
                            } else {
                                ExitReason::Shutdown
                            },
                        });
                    }
                    self.crash()?;
                    return Ok(StopReason::Exited {
                        reached: Timestamp::from_nanos(context.clock.now()),
                        reason: ExitReason::Crashed,
                    });
                }
                seedbox_machine::pv::Request::Reboot => {
                    context.inbox.take();
                    self.reboot(Timestamp::from_nanos(context.clock.now()))?;
                }
                seedbox_machine::pv::Request::Shutdown => {
                    context.inbox.take();
                    self.shutdown()?;
                }
                seedbox_machine::pv::Request::Application { .. } => break,
                _ => unreachable!(),
            }
        }

        let now = context.clock.now();
        let end = upto.as_nanos();
        let previous = self.reached;
        if self.pending_boots == 0 && (self.crashed || self.exited) {
            if upto.as_nanos() == now {
                return Ok(StopReason::Exited {
                    reached: at(now),
                    reason: if self.crashed {
                        ExitReason::Crashed
                    } else {
                        ExitReason::Shutdown
                    },
                });
            }
            return Err(std::io::Error::other("fixture ran an exited machine"));
        }
        if self.pending_boots != 0 {
            if self.pending_at.is_some_and(|at| end < at) {
                return Err(std::io::Error::other("fixture run precedes pending reboot"));
            }
            let boot_at = now
                .max(self.reached)
                .max(self.pending_at.unwrap_or(self.reached));
            for _ in 0..self.pending_boots {
                let mut seed = [0; 8];
                context.rng.fill_bytes(&mut seed);
                self.boot_times.push(boot_at);
                self.ops = self.boot.clone();
            }
            self.pending_boots = 0;
            self.pending_at = None;
            self.reached = boot_at;
            self.sleeping = None;
            self.crashed = false;
            self.exited = false;
            self.shutdown_delivered = false;
        }
        self.reached = self.reached.max(now);
        if self.fail_run_at == Some(now) {
            return Err(std::io::Error::other("fixture adoption failure"));
        }
        self.log.lock().unwrap().push((self.name, now, end));
        if self.shutdown_requested && !self.shutdown_delivered {
            self.shutdown_delivered = true;
            self.shutdown_deliveries += 1;
            self.sleeping = None;
        }
        if self.shutdown_delivered {
            let work = self.shutdown_work_remaining.min(end - self.reached);
            self.reached += work;
            self.executed += work;
            self.shutdown_work_remaining -= work;
            if self.shutdown_work_remaining == 0 {
                self.exited = true;
                return Ok(StopReason::Exited {
                    reached: at(self.reached),
                    reason: ExitReason::Shutdown,
                });
            }
            return Ok(StopReason::BoundaryReached);
        }
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
                    self.exited = true;
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

    fn snapshot(&mut self) -> Result<TestSnapshot, Self::Error> {
        Ok(TestSnapshot {
            reached: self.reached,
            executed: self.executed,
            sleeping: self.sleeping,
            ops: self.ops.clone(),
            boot: self.boot.clone(),
            crashed: self.crashed,
            exited: self.exited,
            shutdown_requested: self.shutdown_requested,
            shutdown_delivered: self.shutdown_delivered,
            shutdown_work_remaining: self.shutdown_work_remaining,
            pending_boots: self.pending_boots,
            pending_at: self.pending_at,
        })
    }
}

pub fn machine(name: usize, ops: &[Op]) -> TestMachine {
    let reached = 0;
    TestMachine {
        name,
        reached,
        executed: 0,
        sleeping: None,
        fail_run_at: None,
        retain_blocked_position: false,
        input: Arc::new(AtomicBool::new(false)),
        ops: ops.iter().copied().collect(),
        boot: ops.iter().copied().collect(),
        lifecycle: Vec::new(),
        fail_reboot: false,
        fail_shutdown: false,
        shutdown_requested: false,
        shutdown_delivered: false,
        shutdown_latches: 0,
        shutdown_deliveries: 0,
        shutdown_work_remaining: 0,
        pending_boots: 1,
        pending_at: None,
        boot_times: Vec::new(),
        exited: false,
        log: Arc::new(Mutex::new(Vec::new())),
        crashed: false,
        fail_crash: false,
    }
}

// The first program is explicitly the oracle in these scheduler fixtures.
pub fn configured(workers: usize, machines: Vec<TestMachine>) -> Driver<TestMachine> {
    let definitions: Vec<_> = (0..machines.len())
        .map(|index| definition(index.to_string()))
        .collect();
    Driver::new(
        at(0),
        Duration::from_nanos(10),
        workers,
        TestRng(1),
        || Ok(()),
        definitions[0].clone(),
        definitions[1..].to_vec(),
        Vec::new(),
        move |definition| Ok(machines[definition.name.parse::<usize>().unwrap()].clone()),
    )
    .unwrap()
}
pub fn controlled(workers: usize, programs: &[&[Op]]) -> Driver<TestMachine> {
    configured(
        workers,
        programs
            .iter()
            .enumerate()
            .map(|(id, ops)| machine(id, ops))
            .collect(),
    )
}
// Lifecycle targets may exit and reboot while a separate parked oracle lives.
pub fn controlled_targets(workers: usize, programs: &[&[Op]]) -> Driver<TestMachine> {
    let mut machines = vec![machine(usize::MAX, &[Op::Park])];
    machines.extend(
        programs
            .iter()
            .enumerate()
            .map(|(id, ops)| machine(id, ops)),
    );
    let mut driver = configured(workers, machines);
    driver
        .with_barrier(|barrier| {
            let oracle = barrier.machines().next().unwrap().0;
            barrier.schedule(
                at(0),
                key("oracle-pause"),
                Action::Pause(Target::Machine(oracle)),
            )
        })
        .unwrap();
    // Apply the oracle pause before tests arm pre-fault holds; it executes
    // no machine and publishes its fault events.
    driver.step(at(0)).unwrap();
    driver
}
pub fn ids(driver: &mut Driver<TestMachine>) -> Vec<MachineId> {
    driver
        .with_barrier(|barrier| {
            Ok(barrier
                .machines()
                .filter(|(_, machine)| machine.name != usize::MAX)
                .map(|(id, _)| id)
                .collect())
        })
        .unwrap()
}
/// The time, action and target of a pre-fault hold.
pub fn held(outcome: &StepOutcome) -> Option<(Timestamp, FaultAction, MachineId)> {
    match outcome {
        StepOutcome::BeforeFault { at, fault } => Some((*at, fault.action, fault.target)),
        _ => None,
    }
}
pub fn status(driver: &mut Driver<TestMachine>) -> PlaybackStatus {
    driver
        .with_barrier(|barrier| Ok(barrier.playback()))
        .unwrap()
}

impl TestMachine {
    fn crash(&mut self) -> Result<(), std::io::Error> {
        self.lifecycle.push("crash");
        if self.fail_crash {
            return Err(std::io::Error::other("fixture crash failure"));
        }
        if (self.crashed || self.exited) && self.pending_boots == 0 {
            return Err(std::io::Error::other("fixture crashed an exited machine"));
        }
        self.crashed = true;
        self.exited = true;
        self.pending_boots = 0;
        self.pending_at = None;
        self.shutdown_requested = false;
        self.shutdown_delivered = false;
        Ok(())
    }
    fn reboot(&mut self, at: Timestamp) -> Result<(), std::io::Error> {
        self.lifecycle.push("reboot");
        if self.fail_reboot {
            return Err(std::io::Error::other("fixture reboot failure"));
        }
        if at.as_nanos() < self.reached
            || self
                .pending_at
                .is_some_and(|previous| at.as_nanos() < previous)
        {
            return Err(std::io::Error::other(
                "fixture reboot precedes machine position",
            ));
        }
        self.pending_boots = self
            .pending_boots
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("fixture pending reboot count exhausted"))?;
        self.pending_at = Some(at.as_nanos());
        self.shutdown_requested = false;
        self.shutdown_delivered = false;
        self.shutdown_work_remaining = 0;
        Ok(())
    }
    fn shutdown(&mut self) -> Result<(), std::io::Error> {
        if self.fail_shutdown {
            return Err(std::io::Error::other("fixture shutdown request failure"));
        }
        if (self.pending_boots > 0 || !self.exited) && !self.shutdown_requested {
            self.shutdown_requested = true;
            self.shutdown_latches += 1;
            self.shutdown_work_remaining = 2;
        }
        Ok(())
    }
}
