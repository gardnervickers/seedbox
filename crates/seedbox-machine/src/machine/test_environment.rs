//! Caller-owned dependencies for direct backend contract tests. This harness is
//! separate from the captured machine; snapshots never save its RNG or clock.
use super::{Machine, RunContext, StopReason, Timestamp};
use crate::{
    clock::Clock,
    rng::{RngStream, SeededRng},
};
use std::{
    ops::{Deref, DerefMut},
    path::Path,
    sync::Arc,
};

pub(super) struct Environment<M, R = SeededRng> {
    machine: M,
    pub rng: R,
    clock: Arc<dyn Clock + Send + Sync>,
    inbox: std::collections::VecDeque<crate::pv::Input>,
    barrier: Timestamp,
}
impl<M, R> Deref for Environment<M, R> {
    type Target = M;
    fn deref(&self) -> &M {
        &self.machine
    }
}
impl<M, R> DerefMut for Environment<M, R> {
    fn deref_mut(&mut self) -> &mut M {
        &mut self.machine
    }
}
impl<M: Machine<Context = ()>, R: RngStream> Environment<M, R> {
    /// Admit a lifecycle request at the current barrier without executing instructions.
    pub fn request(&mut self, payload: crate::pv::Request) -> Result<StopReason, M::Error> {
        let at = self.barrier.max(Timestamp::from_nanos(self.clock.now()));
        let clock = crate::clock::SimulationClock::new(at.as_nanos());
        run_requests(&mut self.machine, &clock, &mut self.rng, [payload], at)
    }
    pub fn run(&mut self, worker: &mut (), upto: Timestamp) -> Result<StopReason, M::Error> {
        let result = self.machine.run(
            &mut RunContext {
                execution: Default::default(),
                worker,
                clock: self.clock.as_ref(),
                rng: &mut self.rng,
                inbox: crate::pv::Inbox::new(
                    &mut self.inbox,
                    Timestamp::from_nanos(self.clock.now()),
                ),
                outbox: crate::pv::Outbox::new(
                    &mut Vec::new(),
                    Timestamp::from_nanos(self.clock.now()),
                    upto,
                ),
            },
            upto,
        );
        if result.is_ok() {
            self.barrier = upto;
        }
        result
    }
}
impl<M: Machine, R> Machine for Environment<M, R> {
    type Context = M::Context;
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        context: &mut RunContext<'_, M::Context>,
        upto: Timestamp,
    ) -> Result<StopReason, M::Error> {
        self.machine.run(context, upto)
    }
    fn snapshot(&mut self) -> Result<M::Snapshot, M::Error> {
        self.machine.snapshot()
    }
}
#[cfg(unix)]
impl<R: RngStream> Environment<super::tcg::TcgMachine, R> {
    pub fn new(
        qemu: &Path,
        manifest: super::tcg::Manifest,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        let at = Timestamp::from_nanos(clock.now());
        let mut environment = Self::configured(qemu, manifest, rng, clock)?;
        environment.run(&mut (), at)?;
        Ok(environment)
    }
    pub fn configured(
        qemu: &Path,
        manifest: super::tcg::Manifest,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        Ok(Self {
            machine: super::tcg::TcgMachine::new_raw(qemu, manifest)?,
            rng,
            barrier: Timestamp::from_nanos(clock.now()),
            clock,
            inbox: Default::default(),
        })
    }
}
#[cfg(unix)]
impl Environment<super::tcg::TcgMachine> {
    pub fn from_snapshot(
        qemu: &Path,
        snapshot: &super::tcg::Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        Ok(Self {
            machine: super::tcg::TcgMachine::from_snapshot(qemu, snapshot)?,
            rng: SeededRng::new(0),
            barrier: Timestamp::from_nanos(clock.now()),
            clock,
            inbox: Default::default(),
        })
    }
}
#[cfg(seedbox_pv)]
impl<R: RngStream> Environment<super::pv::PVMachine, R> {
    pub fn new(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        let at = Timestamp::from_nanos(clock.now());
        let mut environment = Self::configured(executable, kernel, initrd, rng, clock)?;
        environment.run(&mut (), at)?;
        Ok(environment)
    }
    pub fn configured(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        Ok(Self {
            machine: super::pv::PVMachine::from_boot_inputs(executable, kernel, initrd)?,
            rng,
            barrier: Timestamp::from_nanos(clock.now()),
            clock,
            inbox: Default::default(),
        })
    }
}
#[cfg(seedbox_pv)]
impl Environment<super::pv::PVMachine> {
    pub fn from_snapshot(
        executable: &Path,
        snapshot: &super::pv::Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self {
            machine: super::pv::PVMachine::from_snapshot(executable, snapshot)?,
            rng: SeededRng::new(0),
            barrier: Timestamp::from_nanos(clock.now()),
            clock,
            inbox: Default::default(),
        })
    }
}

/// Drive a raw backend with explicit caller-owned queued inputs.
pub(super) fn run_requests<M: Machine<Context = ()>>(
    machine: &mut M,
    clock: &dyn Clock,
    rng: &mut dyn RngStream,
    requests: impl IntoIterator<Item = crate::pv::Request>,
    upto: Timestamp,
) -> Result<StopReason, M::Error> {
    let at = Timestamp::from_nanos(clock.now());
    let mut inbox = requests
        .into_iter()
        .map(|payload| crate::pv::Input { at, payload })
        .collect();
    machine.run(
        &mut RunContext {
            execution: Default::default(),
            worker: &mut (),
            clock,
            rng,
            inbox: crate::pv::Inbox::new(&mut inbox, at),
            outbox: crate::pv::Outbox::new(&mut Vec::new(), at, upto),
        },
        upto,
    )
}

/// Shared real-backend lifecycle admission regression; callers supply a native machine.
pub(super) fn check_lifecycle_fifo<M: Machine<Context = ()>>(mut machine: M)
where
    M::Error: std::fmt::Debug,
{
    use super::ExitReason;
    use crate::pv::{ApplicationCommand, Request};
    struct CountingRng(usize);
    impl RngStream for CountingRng {
        fn fill_bytes(&mut self, bytes: &mut [u8]) {
            self.0 += bytes.len();
            bytes.fill(0);
        }
    }
    let mut rng = CountingRng(0);
    let clock = crate::clock::SimulationClock::new(0);
    let at = Timestamp::from_nanos;
    assert_eq!(
        run_requests(&mut machine, &clock, &mut rng, [Request::Crash], at(5)).unwrap(),
        StopReason::Exited {
            reached: at(0),
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(rng.0, 0, "crash before first boot cancels initialization");
    machine.snapshot().unwrap();
    clock.set(10);
    assert_eq!(
        run_requests(&mut machine, &clock, &mut rng, [], at(10)).unwrap(),
        StopReason::Exited {
            reached: at(10),
            reason: ExitReason::Crashed
        }
    );
    clock.set(0); // A lagging clock still permits the already-reached zero boundary.
    assert_eq!(
        run_requests(&mut machine, &clock, &mut rng, [], at(10)).unwrap(),
        StopReason::Exited {
            reached: at(10),
            reason: ExitReason::Crashed
        }
    );
    assert!(run_requests(&mut machine, &clock, &mut rng, [], at(11)).is_err());
    assert_eq!(rng.0, 0);
    clock.set(10);
    let command = || Request::Application {
        id: std::num::NonZeroU32::new(77).unwrap(),
        command: ApplicationCommand::Pause,
    };
    // The ended-boot app request is consumed and cannot reserve its ID in the
    // new boot or obstruct the following Reboot in the same standalone inbox.
    assert_eq!(
        run_requests(
            &mut machine,
            &clock,
            &mut rng,
            [command(), Request::Reboot, command(), Request::Shutdown],
            at(10)
        )
        .unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(rng.0, 8);
    machine.snapshot().unwrap();
    assert_eq!(
        run_requests(
            &mut machine,
            &clock,
            &mut rng,
            [Request::Reboot, Request::Crash],
            at(15)
        )
        .unwrap(),
        StopReason::Exited {
            reached: at(10),
            reason: ExitReason::Crashed
        }
    );
    assert_eq!(
        rng.0, 8,
        "cancelled deferred reboot executes no initialization"
    );
}
