//! Caller-owned dependencies for direct backend contract tests. This harness is
//! separate from the captured machine; snapshots never save its RNG or clock.
use super::{Machine, RunContext, StopReason, Timestamp};
use crate::{
    clock::Clock,
    rng::{RngStream, SeededRng},
};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

pub struct Environment<M, R = SeededRng> {
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
    /// Attach caller-owned dependencies to an already configured, stopped machine.
    /// Construction performs no execution and saves no dependency in its snapshot.
    // Backend-local constructors wrap this generic harness: backend crates
    // cannot add inherent methods to this foreign type.
    pub fn configured(machine: M, rng: R, clock: Arc<dyn Clock + Send + Sync>) -> Self {
        Self {
            machine,
            rng,
            barrier: Timestamp::from_nanos(clock.now()),
            clock,
            inbox: Default::default(),
        }
    }

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
/// Drive a raw backend with explicit caller-owned queued inputs.
pub fn run_requests<M: Machine<Context = ()>>(
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
pub fn check_lifecycle_fifo<M: Machine<Context = ()>>(mut machine: M)
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

/// Shared queued-boot entropy regression; callers supply a native machine that
/// has not drawn entropy, its private restore and its private boot-seed reader.
// Restore and the boot seed are private to each backend, so closures pass
// them in; no public accessor exists only for tests.
pub fn check_queued_boot_entropy<M, F, S>(mut machine: M, restore: F, seed: S)
where
    M: Machine<Context = ()>,
    M::Error: std::fmt::Debug,
    F: FnOnce(&M::Snapshot) -> M,
    S: Fn(&M) -> u64,
{
    use super::ExitReason;
    use crate::pv::Request;
    #[derive(Clone)]
    struct Bytes(u8);
    impl RngStream for Bytes {
        fn fill_bytes(&mut self, bytes: &mut [u8]) {
            for byte in bytes {
                *byte = self.0;
                self.0 += 1;
            }
        }
    }
    let at = Timestamp::from_nanos;
    let clock = crate::clock::SimulationClock::new(0);
    let mut rng = Bytes(0);
    machine.snapshot().unwrap();
    assert_eq!(rng.0, 0, "snapshot must not draw caller entropy");
    run_requests(&mut machine, &clock, &mut rng, [], at(0)).unwrap();
    assert_eq!(rng.0, 8);
    let saved = machine.snapshot().unwrap();
    let mut branch_rng = rng.clone();
    run_requests(
        &mut machine,
        &clock,
        &mut rng,
        [Request::Reboot, Request::Reboot, Request::Shutdown],
        at(0),
    )
    .unwrap();
    assert_eq!(
        rng.0, 24,
        "each ordered reboot draws one seed, without instructions"
    );
    assert_eq!(
        seed(&machine),
        u64::from_le_bytes([16, 17, 18, 19, 20, 21, 22, 23])
    );
    let mut branch = restore(&saved);
    run_requests(
        &mut branch,
        &clock,
        &mut branch_rng,
        [Request::Reboot, Request::Reboot, Request::Shutdown],
        at(0),
    )
    .unwrap();
    assert_eq!(seed(&branch), seed(&machine));
    assert_eq!(branch_rng.0, rng.0);
    let before_crash = branch_rng.0;
    assert!(matches!(
        run_requests(
            &mut branch,
            &clock,
            &mut branch_rng,
            [Request::Crash],
            at(0)
        )
        .unwrap(),
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    assert_eq!(
        branch_rng.0, before_crash,
        "crash accepts at zero budget without a seed draw"
    );
    branch.snapshot().unwrap();
    run_requests(
        &mut branch,
        &clock,
        &mut branch_rng,
        [Request::Reboot],
        at(0),
    )
    .unwrap();
    assert_eq!(branch_rng.0, 32, "reboot revives an exited machine");
}

/// Shared deferred-startup regression; callers supply a machine whose backend
/// process path, `missing`, does not exist.
// The `Display` bound is all the body needs to match its error to the path
// the caller says is missing.
pub fn check_deferred_startup_failure<M>(mut machine: M, missing: &str)
where
    M: Machine<Context = ()>,
    M::Error: std::fmt::Display,
{
    let at = Timestamp::from_nanos;
    let clock = crate::clock::SimulationClock::new(0);
    let mut rng = SeededRng::new(9);
    let mut expected = rng.clone();
    use crate::rng::RngStream;
    expected.fill_bytes(&mut [0; 8]);
    assert!(machine.snapshot().is_ok());
    let error = machine
        .run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: crate::pv::Inbox::new(
                    &mut Default::default(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                ),
                outbox: crate::pv::Outbox::new(
                    &mut Vec::new(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                    at(0),
                ),
            },
            at(0),
        )
        .unwrap_err();
    assert!(error.to_string().contains(missing));
    assert!(machine.snapshot().is_err());
    assert!(machine
        .run(
            &mut RunContext {
                execution: Default::default(),
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                inbox: crate::pv::Inbox::new(
                    &mut Default::default(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock))
                ),
                outbox: crate::pv::Outbox::new(
                    &mut Vec::new(),
                    Timestamp::from_nanos(crate::clock::Clock::now(&clock)),
                    at(1)
                ),
            },
            at(1)
        )
        .is_err());
    let mut got = [0; 8];
    let mut want = [0; 8];
    rng.fill_bytes(&mut got);
    expected.fill_bytes(&mut want);
    assert_eq!(got, want);
}
