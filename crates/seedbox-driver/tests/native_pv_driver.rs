#![cfg(seedbox_pv)]
use native_fixture::auto_key;
use seedbox_driver::driver::{Action, Driver, StepOutcome, Target};
use seedbox_driver::test_support::native as native_fixture;
use seedbox_machine::test_support::{SeededRng, SimulationClock};
use seedbox_machine::{
    clock::Clock,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    rng::RngStream,
};
use seedbox_machine_pv::machine::pv::{test_support as fixture, PVMachine as RawMachine, Snapshot};
use std::{fs, path::Path, sync::Arc};
type PVMachine = Environment<RawMachine>;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
struct Environment<M, R = SeededRng>(
    seedbox_machine::test_support::test_environment::Environment<M, R>,
);
impl<M, R> std::ops::Deref for Environment<M, R> {
    type Target = seedbox_machine::test_support::test_environment::Environment<M, R>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<M, R> std::ops::DerefMut for Environment<M, R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<M: Machine, R> Machine for Environment<M, R> {
    type Context = M::Context;
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        c: &mut RunContext<'_, Self::Context>,
        t: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        Machine::run(&mut self.0, c, t)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        self.0.snapshot()
    }
}
impl<R: RngStream> Environment<RawMachine, R> {
    fn configured(
        worker: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> std::io::Result<Self> {
        Ok(Self(
            seedbox_machine::test_support::test_environment::Environment::configured(
                fixture::boot(worker, kernel, initrd)?,
                rng,
                clock,
            ),
        ))
    }
}
impl Environment<RawMachine> {
    fn from_snapshot(
        worker: &Path,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self(
            seedbox_machine::test_support::test_environment::Environment::configured(
                RawMachine::from_snapshot(worker, snapshot)?,
                SeededRng::new(0),
                clock,
            ),
        ))
    }
}

impl<M: Machine<Context = ()>, R: RngStream> Environment<M, R> {}

/// Cold snapshots exercise context creation on both construction paths.
#[test]
fn concrete_pv_drivers_create_only_the_callers_context() {
    use seedbox_driver::{driver::Driver, test_support::definition};
    let executable = std::env::current_exe().unwrap();
    let worker = executable.clone();
    let create = move |_: &seedbox_machine::definition::MachineDefinition| {
        fixture::boot(&worker, fixture::timed_guest(), b"initrd".to_vec())
    };
    let owner = std::thread::current().id();
    let context = move || {
        assert_eq!(std::thread::current().id(), owner);
        Ok(())
    };
    let mut driver = Driver::new_seeded(
        at(0),
        std::time::Duration::from_millis(1),
        1,
        context,
        definition("oracle"),
        vec![definition("peer")],
        Vec::new(),
        seedbox_driver::driver::NetworkConfig::default(),
        create.clone(),
    )
    .unwrap();
    let saved = driver.checkpoint().unwrap();
    Driver::from_checkpoint(
        &saved,
        context,
        |snap| RawMachine::from_snapshot(&executable, snap).map_err(std::io::Error::other),
        create,
    )
    .unwrap();
}

/// Step toward `upto` until the run finishes, blocks or stops, or `upto` is
/// reached; returns the last outcome.
fn run_to(driver: &mut Driver<PVMachine>, upto: Timestamp) -> StepOutcome {
    loop {
        match driver.step(upto).unwrap() {
            StepOutcome::BarrierReached { at } if at < upto => {}
            outcome => return outcome,
        }
    }
}

mod machine {
    mod pv {
        mod tests {
            use crate::*;
            // Invoke explicitly with SEEDBOX_PV_WORKER set to a signed seedbox-pv-worker binary.
            // Default portable tests do not open HVF/KVM.
            #[test]
            #[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
            fn native_driver_isolates_two_guests_and_restores_independent_snapshots() {
                let executable =
                    std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
                let executable = Path::new(&executable);
                let mut driver = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        (0..2)
                            .map(|_| {
                                PVMachine::configured(
                                    executable,
                                    fixture::timed_guest(),
                                    b"initrd".to_vec(),
                                    SeededRng::new(7),
                                    clock.clone(),
                                )
                            })
                            .collect()
                    },
                )
                .unwrap();
                assert_eq!(
                    driver.step(at(1)).unwrap(),
                    StepOutcome::BarrierReached { at: at(1) }
                );
                let snapshot = driver
                    .with_barrier(|barrier| {
                        let ids: Vec<_> = barrier.machines().map(|(id, _)| id).collect();
                        for id in &ids {
                            assert_eq!(
                                barrier.stopped(*id)?,
                                StopReason::Blocked {
                                    reached: at(0),
                                    deadline: Some(at(5))
                                }
                            );
                        }
                        let snapshot = barrier.snapshot(ids[0])?;
                        barrier.schedule(
                            at(3),
                            auto_key(),
                            Action::Crash(Target::Machine(ids[1])),
                        )?;
                        Ok(snapshot)
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(at(10)).unwrap(),
                    StepOutcome::BarrierReached { at: at(3) }
                );
                // A round does not end at a timer, so each driver asks for a
                // barrier at the guests' timer before the one that powers off.
                assert_eq!(
                    driver.step(at(5)).unwrap(),
                    StepOutcome::BarrierReached { at: at(5) }
                );
                // Powered off at 5, the run ends one floor (1 ms) later.
                assert_eq!(
                    run_to(&mut driver, at(2_000_000)),
                    StepOutcome::Finished {
                        at: at(1_000_005),
                        reason: ExitReason::Shutdown
                    }
                );
                drop(driver);
                // The checkpoint survives the workers and temporary boot/checkpoint files.
                let mut restored = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                            .map_err(std::io::Error::other)?])
                    },
                )
                .unwrap();
                assert_eq!(
                    restored.step(at(1)).unwrap(),
                    StepOutcome::BarrierReached { at: at(1) }
                );
                assert_eq!(
                    restored.step(at(5)).unwrap(),
                    StepOutcome::BarrierReached { at: at(5) }
                );
                // Powered off at 5, the run ends one floor (1 ms) later.
                assert_eq!(
                    run_to(&mut restored, at(2_000_000)),
                    StepOutcome::Finished {
                        at: at(1_000_005),
                        reason: ExitReason::Shutdown
                    }
                );
                let mut second = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                            .map_err(std::io::Error::other)?])
                    },
                )
                .unwrap();
                assert_eq!(
                    second.step(at(1)).unwrap(),
                    StepOutcome::BarrierReached { at: at(1) }
                );
                assert_eq!(
                    second.step(at(5)).unwrap(),
                    StepOutcome::BarrierReached { at: at(5) }
                );
                // Powered off at 5, the run ends one floor (1 ms) later.
                assert_eq!(
                    run_to(&mut second, at(2_000_000)),
                    StepOutcome::Finished {
                        at: at(1_000_005),
                        reason: ExitReason::Shutdown
                    }
                );
            }
            #[test]
            #[ignore = "requires a native PV hypervisor, SEEDBOX_PV_WORKER and SEEDBOX_PV_KERNEL"]
            fn native_linux_boot_and_program_poweroff_use_the_driver() {
                let executable =
                    std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
                let kernel_path =
                    std::env::var_os("SEEDBOX_PV_KERNEL").expect("set SEEDBOX_PV_KERNEL");
                let kernel = fs::read(kernel_path).unwrap();
                let mut driver = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        Ok(vec![PVMachine::configured(
                            Path::new(&executable),
                            kernel,
                            fixture::linux_initrd(),
                            SeededRng::new(7),
                            clock,
                        )?])
                    },
                )
                .unwrap();
                for tick in 1..=10_000 {
                    match driver.step(at(tick * 1_000_000)).unwrap() {
                        StepOutcome::Finished { .. } => {
                            driver
                                .with_barrier(|barrier| {
                                    let (_, machine) = barrier.machines().next().unwrap();
                                    fixture::assert_linux_pass_console(machine);
                                    Ok(())
                                })
                                .unwrap();
                            return;
                        }
                        StepOutcome::Blocked { .. } => panic!("Linux blocked without a timer"),
                        _ => {}
                    }
                }
                panic!("Linux failed to power off within the simulation bound");
            }
            #[test]
            #[ignore = "requires a native PV hypervisor and SEEDBOX_PV_WORKER"]
            fn native_horizon_is_accepted_before_following_poweroff() {
                let executable =
                    std::env::var_os("SEEDBOX_PV_WORKER").expect("set SEEDBOX_PV_WORKER");
                let executable = Path::new(&executable);
                let code = fixture::horizon_guest();
                let mut driver = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        Ok(vec![PVMachine::configured(
                            executable,
                            code,
                            b"initrd".to_vec(),
                            SeededRng::new(1),
                            clock,
                        )?])
                    },
                )
                .unwrap();
                assert_eq!(
                    driver.step(at(40)).unwrap(),
                    StepOutcome::BarrierReached { at: at(40) }
                );
                let snapshot = driver
                    .with_barrier(|barrier| {
                        let id = barrier.machines().next().unwrap().0;
                        assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
                        barrier.snapshot(id)
                    })
                    .unwrap();
                // Powered off at 40, the run ends one floor (1 ms) later.
                assert_eq!(
                    run_to(&mut driver, at(2_000_000)),
                    StepOutcome::Finished {
                        at: at(1_000_040),
                        reason: ExitReason::Shutdown
                    }
                );
                drop(driver);
                let mut branch = native_fixture::driver(
                    at(40),
                    std::time::Duration::from_millis(1),
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(40)).as_nanos()));
                        Ok(vec![PVMachine::from_snapshot(executable, &snapshot, clock)
                            .map_err(std::io::Error::other)?])
                    },
                )
                .unwrap();
                // Powered off at 40, the run ends one floor (1 ms) later.
                assert_eq!(
                    run_to(&mut branch, at(2_000_000)),
                    StepOutcome::Finished {
                        at: at(1_000_040),
                        reason: ExitReason::Shutdown
                    }
                );
            }
        }
    }
}
