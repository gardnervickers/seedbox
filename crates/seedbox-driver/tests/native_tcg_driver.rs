#![cfg(unix)]
use fixture::raw_program::{self, counting_loop, RBX, STUB};
use native_fixture::auto_key;
use seedbox_driver::driver::{Driver, MachineId, StepOutcome, Target};
use seedbox_driver::test_support::native as native_fixture;
use seedbox_machine::test_support::{SeededRng, SimulationClock};
use seedbox_machine::{
    clock::Clock,
    machine::{ExitReason, Machine, RunContext, StopReason, Timestamp},
    rng::RngStream,
};
use seedbox_machine_tcg::machine::tcg::{
    test_support as fixture, Error, Snapshot, TcgMachine as RawMachine,
};
use seedbox_test_time::HangGuard;
use std::{fs, path::Path, sync::Arc, time::Duration};
type TcgMachine = Environment<RawMachine>;
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
#[derive(Clone)]
enum Boot {
    Program(Arc<[u8]>),
    Linux {
        kernel: Arc<[u8]>,
        initramfs: Arc<[u8]>,
        firmware: Arc<[u8]>,
        command_line: String,
        epoch_seconds: u32,
    },
}
struct Manifest {
    boot: Boot,
    memory_mib: u32,
    disk: Option<seedbox_machine::definition::DiskImage>,
    network: Option<[u8; 6]>,
}
impl<R: RngStream> Environment<RawMachine, R> {
    fn configured(
        qemu: &Path,
        manifest: Manifest,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, Error> {
        assert!(
            manifest.disk.is_none() && manifest.network.is_none(),
            "these retained Driver fixtures attach no devices"
        );
        let machine = match manifest.boot {
            Boot::Program(bytes) => fixture::program(qemu, bytes.to_vec(), manifest.memory_mib)?,
            Boot::Linux {
                kernel,
                initramfs,
                firmware,
                command_line,
                epoch_seconds,
            } => fixture::linux(
                qemu,
                kernel.to_vec(),
                initramfs.to_vec(),
                firmware.to_vec(),
                command_line,
                epoch_seconds,
                manifest.memory_mib,
            )?,
        };
        Ok(Self(
            seedbox_machine::test_support::test_environment::Environment::configured(
                machine, rng, clock,
            ),
        ))
    }
}
impl Environment<RawMachine> {
    fn from_snapshot(
        qemu: &Path,
        snapshot: &Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, Error> {
        Ok(Self(
            seedbox_machine::test_support::test_environment::Environment::configured(
                RawMachine::from_snapshot(qemu, snapshot)?,
                SeededRng::new(0),
                clock,
            ),
        ))
    }
}
fn manifest(image: Vec<u8>) -> Manifest {
    Manifest {
        boot: Boot::Program(image.into()),
        memory_mib: 16,
        disk: None,
        network: None,
    }
}

const QEMU: &str = "qemu-system-x86_64";

/// The existing Linux idle test requires userspace boot within two simulated
/// seconds. tcg-init then sleeps for seedbox_sleep before poweroff; allow the
/// same two-second kernel budget for shutdown. These are guest-time bounds,
/// independent of the number of guests, workers or elapsed host time.
struct LinuxWorkloadProgress {
    now: Timestamp,
    deadline: Timestamp,
}
impl LinuxWorkloadProgress {
    fn new(longest_sleep_seconds: u64) -> Self {
        Self {
            now: at(0),
            deadline: at((2 + longest_sleep_seconds + 2) * 1_000_000_000),
        }
    }

    fn next_limit(&self, step_limit: u64) -> Result<Timestamp, &'static str> {
        if self.now >= self.deadline {
            return Err("Linux workload did not finish within boot + sleep + shutdown guest time");
        }
        Ok(at(self.now.as_nanos() + step_limit).min(self.deadline))
    }

    fn advanced(&mut self, reached: Timestamp) -> Result<(), &'static str> {
        if reached <= self.now {
            return Err("Linux workload Driver step made no simulated progress");
        }
        self.now = reached;
        Ok(())
    }
}

#[test]
fn linux_workload_stalls_fail_without_waiting_for_host_time() {
    let mut stalled = LinuxWorkloadProgress::new(3);
    assert_eq!(
        stalled.advanced(at(0)),
        Err("Linux workload Driver step made no simulated progress")
    );
    // A guest may keep executing while never reaching its expected shutdown.
    // Both real test step sizes must stop at the same derived guest deadline.
    for quantum in [1_000_000, 131_071] {
        let mut spinning = LinuxWorkloadProgress::new(3);
        while let Ok(limit) = spinning.next_limit(quantum) {
            spinning.advanced(limit).unwrap();
        }
        assert_eq!(spinning.now, at(7_000_000_000));
        assert_eq!(
            spinning.next_limit(quantum),
            Err("Linux workload did not finish within boot + sleep + shutdown guest time")
        );
    }
}

/// Arm the LAPIC timer to expire at program instruction 1003 (the count
/// store is instruction 4 and fires count + 1 later), `sti; hlt`, and after
/// the EOI handler (two instructions) increment EBX once.
fn timer_then_increment() -> Vec<u8> {
    raw_program::Asm::default()
        .lapic_timer(998)
        .sti()
        .hlt()
        .inc(raw_program::Reg::Ebx)
        .spin()
        .image()
}

fn clock(nanos: u64) -> Arc<SimulationClock> {
    Arc::new(SimulationClock::new(nanos))
}

fn at(nanos: u64) -> Timestamp {
    Timestamp::from_nanos(nanos)
}

fn machine_id(driver: &mut Driver<TcgMachine>) -> MachineId {
    driver
        .with_barrier(|barrier| Ok(barrier.machines().next().unwrap().0))
        .unwrap()
}

fn stopped(driver: &mut Driver<TcgMachine>, id: MachineId) -> StopReason {
    driver.with_barrier(|barrier| barrier.stopped(id)).unwrap()
}

fn console(driver: &mut Driver<TcgMachine>, id: MachineId) -> Vec<u8> {
    driver
        .with_barrier(|barrier| Ok(barrier.machine(id)?.console().unwrap()))
        .unwrap()
}

fn step(driver: &mut Driver<TcgMachine>, id: MachineId, now: &mut Timestamp, quantum: u64) {
    let upto = match stopped(driver, id) {
        StopReason::Blocked {
            deadline: Some(deadline),
            ..
        } => deadline,
        _ => at(now.as_nanos() + quantum),
    };
    *now = match driver.step(upto).unwrap() {
        StepOutcome::BarrierReached { at } | StepOutcome::Finished { at, .. } => at,
        StepOutcome::Blocked { .. } => panic!("guest blocked without a timer"),
        StepOutcome::Waiting { .. }
        | StepOutcome::BeforeFault { .. }
        | StepOutcome::Stopped { .. } => panic!("unexpected playback control in test"),
    };
}

fn registers(machine: &mut TcgMachine) -> [u64; 17] {
    fixture::registers(machine).unwrap()
}

impl<M: Machine<Context = ()>, R: RngStream> Environment<M, R> {
    fn run(&mut self, worker: &mut (), upto: Timestamp) -> Result<StopReason, M::Error> {
        self.0.run(worker, upto)
    }
}

mod machine {
    mod tcg {
        mod tests {
            use crate::*;
            #[test]
            fn driver_starts_at_its_clock_and_records_early_exit() {
                let mut driver = native_fixture::driver(
                    at(37),
                    std::time::Duration::from_millis(1),
                    2,
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(37)).as_nanos()));
                        let machine = Environment(
                seedbox_machine::test_support::test_environment::Environment::configured(
                    fixture::shutdown(Path::new(QEMU))?,
                    SeededRng::new(0),
                    clock,
                ),
            );
                        fixture::assert_unstarted(&machine);
                        Ok(vec![machine])
                    },
                )
                .unwrap();
                let id = machine_id(&mut driver);
                // The power-off store is the one instruction after the stub.
                let exited = at(37 + STUB + 1);
                assert_eq!(
                    driver.step(at(100)).unwrap(),
                    StepOutcome::Finished {
                        at: exited,
                        reason: ExitReason::Shutdown
                    }
                );
                assert_eq!(
                    stopped(&mut driver, id),
                    StopReason::Exited {
                        reached: exited,
                        reason: ExitReason::Shutdown,
                    }
                );
            }
            #[test]
            fn linux_idle_timer_wakes_complete_bounded_runs() {
                fn input(name: &str) -> std::path::PathBuf {
                    std::env::var_os(name)
                        .expect("run tests inside nix develop")
                        .into()
                }
                let assets = input("SEEDBOX_TCG_LINUX");
                let mut driver =
                    native_fixture::driver(
                        at(0),
                        std::time::Duration::from_millis(1),
                        2,
                        SeededRng::new(0),
                        || Ok(()),
                        || {
                            let clock: Arc<dyn Clock + Send + Sync> =
                                Arc::new(SimulationClock::new((at(0)).as_nanos()));
                            TcgMachine::configured(
                &input("SEEDBOX_TCG_QEMU"),
                Manifest {
                    memory_mib: 256,
                    disk: None,
                    network: None,
                    boot: Boot::Linux {
                        kernel: fs::read(assets.join("kernel")).unwrap().into(),
                        initramfs: fs::read(assets.join("initramfs")).unwrap().into(),
                        firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap().into(),
                        command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr".into(),
                        epoch_seconds: 0,
                    },
                },
                SeededRng::new(0),
                clock,
            )
            .map(|machine| vec![machine])
                        },
                    )
                    .unwrap();
                let index = machine_id(&mut driver);
                let mut now = at(0);
                // Cross the real Linux idle/timer transitions that previously
                // stranded seedbox-run in rr_wait_io_event. Every bounded step
                // must complete; no guest changes or inspection kick the CPU.
                let mut blocked_stops = 0;
                while now < at(2_000_000_000) {
                    let adopted = now;
                    step(&mut driver, index, &mut now, 1_000_000);
                    match stopped(&mut driver, index) {
                        StopReason::Blocked { reached, deadline } => {
                            blocked_stops += 1;
                            assert!(deadline.is_none_or(|next| next > reached));
                        }
                        StopReason::BoundaryReached => {}
                        reason => panic!("unexpected Linux idle stop: {reason:?}"),
                    }
                    assert!(now > adopted, "bounded Driver step must advance");
                }
                assert!(blocked_stops > 0, "exercise a real Linux idle stop");
                let output = String::from_utf8(console(&mut driver, index)).unwrap();
                assert!(
                    output.contains("seedbox: Linux userspace booted"),
                    "{output}"
                );
            }
            #[test]
            fn linux_boots_sleeps_and_shuts_down_with_the_machine_driver() {
                fn input(name: &str) -> std::path::PathBuf {
                    std::env::var_os(name)
                        .expect("run tests inside nix develop")
                        .into()
                }
                let assets = input("SEEDBOX_TCG_LINUX");
                let mut driver =
                    native_fixture::driver(
                        at(0),
                        std::time::Duration::from_millis(1),
                        2,
                        SeededRng::new(0),
                        || Ok(()),
                        || {
                            let clock: Arc<dyn Clock + Send + Sync> =
                                Arc::new(SimulationClock::new((at(0)).as_nanos()));
                            TcgMachine::configured(
                &input("SEEDBOX_TCG_QEMU"),
                Manifest {
                    memory_mib: 256,
                    disk: None,
                    network: None,
                    boot: Boot::Linux {
                        kernel: fs::read(assets.join("kernel")).unwrap().into(),
                        initramfs: fs::read(assets.join("initramfs")).unwrap().into(),
                        firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap().into(),
                        command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr".into(),
                        epoch_seconds: 0,
                    },
                },
                SeededRng::new(0),
                clock,
            )
            .map(|machine| vec![machine])
                        },
                    )
                    .unwrap();
                let index = machine_id(&mut driver);
                let mut now = at(0);
                let mut timer_wakeups = 0;
                let mut fork = None;
                let mut output_at_fork = 0;
                // examples/tcg-init's workload sleeps100 simulated seconds.
                // Allow5 simulated seconds for bounded kernel boot/shutdown work;
                // host scheduling/throughput is not a correctness criterion.
                const WORKLOAD_SLEEP_NS: u64 = 100_000_000_000;
                const KERNEL_WORK_NS: u64 = 5_000_000_000;
                let source_limit = at(WORKLOAD_SLEEP_NS + KERNEL_WORK_NS);
                fn advance_before(
                    driver: &mut Driver<TcgMachine>,
                    index: MachineId,
                    now: &mut Timestamp,
                    quantum: u64,
                    limit: Timestamp,
                ) {
                    assert!(
                        *now < limit,
                        "Linux exceeded guest-time limit {limit:?}: {}",
                        String::from_utf8_lossy(&console(driver, index))
                    );
                    let upto = match stopped(driver, index) {
                        StopReason::Blocked {
                            deadline: Some(deadline),
                            ..
                        } => deadline,
                        _ => at(now.as_nanos() + quantum),
                    }
                    .min(limit);
                    *now = match driver.step(upto).unwrap() {
                        StepOutcome::BarrierReached { at } | StepOutcome::Finished { at, .. } => at,
                        other => panic!("unexpected bounded Linux step: {other:?}"),
                    };
                }
                let exit_at = loop {
                    advance_before(&mut driver, index, &mut now, 1_000_000, source_limit);
                    match stopped(&mut driver, index) {
                        StopReason::BoundaryReached => {}
                        StopReason::Blocked {
                            deadline: Some(_), ..
                        } => {
                            timer_wakeups += 1;
                            let output = console(&mut driver, index);
                            if fork.is_none()
                                && String::from_utf8_lossy(&output)
                                    .contains("seedbox: Linux userspace booted")
                            {
                                output_at_fork = output.len();
                                fork = Some(
                                    driver
                                        .with_barrier(|barrier| barrier.snapshot(index))
                                        .unwrap(),
                                );
                            }
                        }
                        StopReason::Exited {
                            reason: ExitReason::Shutdown,
                            reached,
                        } => break reached,
                        reason => panic!(
                            "unexpected Linux stop: {reason:?}\n{}",
                            String::from_utf8_lossy(&console(&mut driver, index))
                        ),
                    }
                };
                let output = String::from_utf8(console(&mut driver, index)).unwrap();
                assert!(
                    output.contains("seedbox: Linux userspace booted"),
                    "{output}"
                );
                assert!(output.contains("seedbox: Linux timer woke"), "{output}");
                assert!(timer_wakeups > 0);
                drop(driver);

                // Restore after source destruction and drive with a different quantum.
                // The remaining console bytes and final simulation position must agree.
                let fork = fork.expect("Linux slept after its boot marker");
                let mut driver = native_fixture::driver(
                    fixture::snapshot_reached(&fork),
                    std::time::Duration::from_millis(1),
                    2,
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> = Arc::new(SimulationClock::new(
                            (fixture::snapshot_reached(&fork)).as_nanos(),
                        ));
                        TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &fork, clock)
                            .map(|machine| vec![machine])
                    },
                )
                .unwrap();
                let index = machine_id(&mut driver);
                let mut now = fixture::snapshot_reached(&fork);
                loop {
                    // The source run supplies an exact deterministic endpoint;
                    // the restored branch may not run beyond it under any host load.
                    advance_before(&mut driver, index, &mut now, 131_071, exit_at);
                    match stopped(&mut driver, index) {
                        StopReason::BoundaryReached => {}
                        StopReason::Blocked {
                            deadline: Some(_), ..
                        } => {}
                        StopReason::Exited {
                            reached,
                            reason: ExitReason::Shutdown,
                        } => {
                            assert_eq!(reached, exit_at);
                            break;
                        }
                        reason => panic!("unexpected Linux branch stop: {reason:?}"),
                    }
                }
                assert_eq!(
                    console(&mut driver, index),
                    &output.as_bytes()[output_at_fork..]
                );
            }
            #[test]
            fn driver_coordinates_linux_guests_without_exceeding_step_limits() {
                fn input(name: &str) -> std::path::PathBuf {
                    std::env::var_os(name)
                        .expect("run tests inside nix develop")
                        .into()
                }
                let assets = input("SEEDBOX_TCG_LINUX");
                let boot = Boot::Linux {
                    kernel: fs::read(assets.join("kernel")).unwrap().into(),
                    initramfs: fs::read(assets.join("initramfs")).unwrap().into(),
                    firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap().into(),
                    command_line: "console=ttyS0 earlycon=uart8250,io,0x3f8 rdinit=/init nokaslr"
                        .into(),
                    epoch_seconds: 0,
                };
                // Hang guard only: main measured ~91 host seconds for these
                // 12 boots unloaded. Four times that permits concurrent portable
                // checks; workload failure is determined by guest progress below.
                let guard = HangGuard::new(
                    "paired native Linux boot transport must return",
                    Duration::from_secs(4 * 91),
                );
                let mut reference = None;
                for (guests, step_limit, workers) in [
                    ([("slow", "2"), ("fast", "1")], 1_000_000, 1),
                    ([("slow", "2"), ("fast", "1")], 1_000_000, 2),
                    ([("slow", "2"), ("fast", "1")], 131_071, 4),
                    ([("fast", "1"), ("slow", "2")], 131_071, 4),
                ] {
                    let oracle = ("oracle", "3");
                    let longest_sleep_seconds = std::iter::once(&oracle)
                        .chain(guests.iter())
                        .map(|(_, sleep)| sleep.parse::<u64>().unwrap())
                        .max()
                        .unwrap();
                    let mut driver = native_fixture::driver(
                        at(0),
                        std::time::Duration::from_millis(1),
                        workers,
                        SeededRng::new(0),
                        || Ok(()),
                        || {
                            let clock: Arc<dyn Clock + Send + Sync> =
                                Arc::new(SimulationClock::new((at(0)).as_nanos()));
                            // The first machine is the oracle. Keep it alive beyond both
                            // observed guests so neither creation order cuts off the other.
                            std::iter::once(&oracle)
                                .chain(guests.iter())
                                .map(|(name, sleep)| {
                                    let mut boot = boot.clone();
                                    if let Boot::Linux { command_line, .. } = &mut boot {
                                        command_line.push_str(&format!(
                                            " seedbox_name={name} seedbox_sleep={sleep}"
                                        ));
                                    }
                                    TcgMachine::configured(
                                        &input("SEEDBOX_TCG_QEMU"),
                                        Manifest {
                                            boot,
                                            memory_mib: 256,
                                            disk: None,
                                            network: None,
                                        },
                                        SeededRng::new(0),
                                        clock.clone(),
                                    )
                                })
                                .collect()
                        },
                    )
                    .unwrap();
                    // Machines and their read-only clock can be transferred to workers.
                    fn assert_send<T: Send>() {}
                    assert_send::<Driver<TcgMachine>>();
                    let ids: Vec<_> = driver
                        .with_barrier(|barrier| {
                            Ok(barrier.machines().skip(1).map(|(id, _)| id).collect())
                        })
                        .unwrap();
                    // All three guests boot concurrently. Derive the deadline
                    // from their actual seedbox_sleep command-line arguments.
                    let mut progress = LinuxWorkloadProgress::new(longest_sleep_seconds);
                    let mut now = at(0);
                    let mut booted = [false; 2];
                    let mut woke = [false; 2];
                    let mut wake_order = Vec::new();
                    loop {
                        guard.remaining().unwrap_or_else(|error| {
                            panic!("paired Linux infrastructure hang at {now:?}: {error}")
                        });
                        let before = now;
                        let upto = progress.next_limit(step_limit).unwrap_or_else(|reason| {
                            panic!("{reason}: at {now:?}, guests={guests:?}, workers={workers}")
                        });
                        let outcome = driver.step(upto).unwrap();
                        now = match outcome {
                            StepOutcome::BarrierReached { at }
                            | StepOutcome::Finished { at, .. } => at,
                            StepOutcome::Blocked { .. } => {
                                panic!("Linux guests blocked without timers")
                            }
                            StepOutcome::Waiting { .. }
                            | StepOutcome::BeforeFault { .. }
                            | StepOutcome::Stopped { .. } => {
                                panic!("unexpected playback control in test")
                            }
                        };
                        assert!(now >= before && now <= upto);
                        progress.advanced(now).unwrap_or_else(|reason| {
                            panic!("{reason}: at {now:?}, guests={guests:?}, workers={workers}")
                        });
                        driver
                            .with_barrier(|barrier| {
                                for (index, &id) in ids.iter().enumerate() {
                                    let guest = barrier.machine(id)?;
                                    // Visible state never comes from beyond this barrier.
                                    fixture::assert_not_beyond(guest, now);
                                    let output =
                                        String::from_utf8(guest.console().unwrap()).unwrap();
                                    booted[index] |=
                                        output.contains("seedbox: Linux userspace booted");
                                    let (name, sleep) = guests[index];
                                    if !woke[index]
                                        && output.contains(&format!(
                                            "seedbox: Linux timer woke name={name} sleep={sleep}"
                                        ))
                                    {
                                        assert!(booted.iter().all(|&booted| booted));
                                        woke[index] = true;
                                        wake_order.push(guests[index].0);
                                    }
                                }
                                Ok(())
                            })
                            .unwrap();
                        if matches!(outcome, StepOutcome::Finished { .. }) {
                            break;
                        }
                    }
                    assert_eq!(wake_order, ["fast", "slow"]);
                    let mut exits: Vec<_> = driver
                        .with_barrier(|barrier| {
                            ids.iter()
                                .enumerate()
                                .map(|(index, &id)| {
                                    let StopReason::Exited {
                                        reason: ExitReason::Shutdown,
                                        reached,
                                    } = barrier.stopped(id)?
                                    else {
                                        panic!("guest did not shut down");
                                    };
                                    Ok((guests[index].0, reached))
                                })
                                .collect()
                        })
                        .unwrap();
                    exits.sort_by_key(|&(name, _)| name);
                    let gap = exits[1].1.as_nanos() - exits[0].1.as_nanos();
                    assert!(
                        (900_000_000..1_100_000_000).contains(&gap),
                        "exit gap: {gap} ns"
                    );
                    // Reversing creation order assigns the independent RNG streams to
                    // different inputs, so only unchanged input order has identical times.
                    if guests[0].0 == "slow" {
                        if let Some(reference) = &reference {
                            assert_eq!(&exits, reference);
                        } else {
                            reference = Some(exits);
                        }
                    }
                }
            }
            #[test]
            fn driver_playback_controls_preserve_real_machine_execution_and_pre_crash_state() {
                use seedbox_driver::driver::{Action, PlaybackRate};
                let code = counting_loop(); // Increment EBX; jump back one instruction.
                let mut reference = None;
                for workers in [1, 2, 4] {
                    let mut driver = native_fixture::driver(
                        at(0),
                        Duration::from_nanos(10),
                        workers,
                        SeededRng::new(0),
                        || Ok(()),
                        || {
                            let clock: Arc<dyn Clock + Send + Sync> =
                                Arc::new(SimulationClock::new((at(0)).as_nanos()));
                            TcgMachine::configured(
                                Path::new(QEMU),
                                manifest(code.clone()),
                                SeededRng::new(0),
                                clock,
                            )
                            .map(|guest| vec![guest])
                        },
                    )
                    .unwrap();
                    let id = machine_id(&mut driver);
                    driver
                        .with_barrier(|barrier| barrier.set_rate(PlaybackRate::Paused))
                        .unwrap();
                    assert!(
                        matches!(driver.step(at(100)).unwrap(), StepOutcome::Waiting { at: reached, .. } if reached == at(0))
                    );
                    driver
                        .with_barrier(|barrier| {
                            fixture::assert_reached(barrier.machine(id)?, at(0));
                            barrier.step_quantum()
                        })
                        .unwrap();
                    assert_eq!(
                        driver.step(at(3)).unwrap(),
                        StepOutcome::BarrierReached { at: at(3) }
                    );
                    assert_eq!(
                        driver.step(at(100)).unwrap(),
                        StepOutcome::BarrierReached { at: at(10) }
                    );
                    // The first quantum ran only stub instructions; admit a
                    // second one to reach eight loop instructions.
                    driver
                        .with_barrier(|barrier| barrier.step_quantum())
                        .unwrap();
                    assert_eq!(
                        driver.step(at(100)).unwrap(),
                        StepOutcome::BarrierReached { at: at(20) }
                    );
                    let saved = driver
                        .with_barrier(|barrier| {
                            assert!(!barrier.playback().stepping);
                            barrier.snapshot(id)
                        })
                        .unwrap();
                    let mut restored =
                        TcgMachine::from_snapshot(Path::new(QEMU), &saved, clock(20)).unwrap();
                    let registers = registers(&mut restored);
                    assert_eq!(registers[RBX], (20 - STUB) / 2);
                    if let Some(reference) = reference {
                        assert_eq!(registers, reference);
                    } else {
                        reference = Some(registers);
                    }
                    driver
                        .with_barrier(|barrier| {
                            barrier.schedule(
                                at(25),
                                auto_key(),
                                Action::Crash(Target::Machine(id)),
                            )?;
                            barrier.pause_before_next_fault(PlaybackRate::Unlimited)
                        })
                        .unwrap();
                    let held = driver.step(at(100)).unwrap();
                    assert!(matches!(&held, StepOutcome::BeforeFault { at: t, fault }
            if *t == at(25) && matches!(fault.target, seedbox_driver::driver::FaultTarget::Machine { machine, .. } if machine == id) && fault.action == seedbox_driver::driver::FaultAction::Crash));
                    for _ in 0..2 {
                        assert_eq!(driver.step(at(100)).unwrap(), held);
                    }
                    driver
                        .with_barrier(|barrier| {
                            fixture::assert_reached(barrier.machine(id)?, at(25));
                            fixture::assert_matches_ready(barrier.machine(id)?);
                            barrier.set_rate(PlaybackRate::Unlimited)
                        })
                        .unwrap();
                    assert_eq!(
                        driver.step(at(100)).unwrap(),
                        StepOutcome::Finished {
                            at: at(25),
                            // Crashing the oracle completes the simulation with its actual
                            // exit reason, not the old all-machines-exited sentinel.
                            reason: ExitReason::Crashed
                        }
                    );
                    assert_eq!(
                        stopped(&mut driver, id),
                        StopReason::Exited {
                            reached: at(25),
                            reason: ExitReason::Crashed
                        }
                    );
                }
            }
        }
        mod elapsed_tests {
            use crate::*;
            #[test]
            fn driver_resumes_a_runnable_guest_without_replaying_paused_instructions() {
                use seedbox_driver::driver::{Action, StepOutcome};
                let mut driver = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_nanos(10_000),
                    2,
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        (0..2)
                            .map(|_| {
                                TcgMachine::configured(
                                    Path::new(QEMU),
                                    manifest(counting_loop()),
                                    SeededRng::new(0),
                                    clock.clone(),
                                )
                            })
                            .collect()
                    },
                )
                .unwrap();
                let id = driver
                    .with_barrier(|barrier| {
                        let id = barrier.machines().next().unwrap().0;
                        barrier.schedule(
                            at(STUB + 3),
                            auto_key(),
                            Action::Pause(Target::Machine(id)),
                        )?;
                        barrier.schedule(
                            at(STUB + 20),
                            auto_key(),
                            Action::Resume(Target::Machine(id)),
                        )?;
                        Ok(id)
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(at(STUB + 3)).unwrap(),
                    StepOutcome::BarrierReached { at: at(STUB + 3) }
                );
                assert!(driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
                let paused = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
                assert_eq!(
                    driver.step(at(STUB + 20)).unwrap(),
                    StepOutcome::BarrierReached { at: at(STUB + 20) }
                );
                assert!(!driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
                let resumed = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
                fixture::assert_snapshot_reached(&resumed, at(STUB + 20));
                let clock = Arc::new(SimulationClock::new(STUB + 20));
                let mut paused_copy =
                    TcgMachine::from_snapshot(Path::new(QEMU), &paused, clock.clone()).unwrap();
                let mut resumed_copy =
                    TcgMachine::from_snapshot(Path::new(QEMU), &resumed, clock).unwrap();
                assert_eq!(registers(&mut paused_copy), registers(&mut resumed_copy));
                assert_eq!(registers(&mut resumed_copy)[RBX], 2);
                assert_eq!(
                    driver.step(at(STUB + 24)).unwrap(),
                    StepOutcome::BarrierReached { at: at(STUB + 24) }
                );
                let after = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
                let mut copy = TcgMachine::from_snapshot(
                    Path::new(QEMU),
                    &after,
                    Arc::new(SimulationClock::new(STUB + 24)),
                )
                .unwrap();
                assert_eq!(registers(&mut copy)[RBX], 4);
            }
            #[test]
            fn driver_resume_delivers_overdue_timer_before_post_resume_instructions() {
                use seedbox_driver::driver::{Action, StepOutcome};
                // First machine sleeps on the LAPIC deadline at program
                // instruction 1003, then increments EBX.
                let programs = [
                    timer_then_increment(),
                    // Second machine remains runnable during the pause.
                    raw_program::Asm::default().spin().image(),
                ];
                let mut driver = native_fixture::driver(
                    at(0),
                    std::time::Duration::from_nanos(10_000),
                    2,
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        programs
                            .iter()
                            .map(|code| {
                                TcgMachine::configured(
                                    Path::new(QEMU),
                                    manifest(code.clone()),
                                    SeededRng::new(0),
                                    clock.clone(),
                                )
                            })
                            .collect()
                    },
                )
                .unwrap();
                let id = driver
                    .with_barrier(|barrier| {
                        let id = barrier.machines().next().unwrap().0;
                        barrier.schedule(
                            at(STUB + 7),
                            auto_key(),
                            Action::Pause(Target::Machine(id)),
                        )?;
                        barrier.schedule(
                            at(STUB + 2000),
                            auto_key(),
                            Action::Resume(Target::Machine(id)),
                        )?;
                        Ok(id)
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(at(STUB + 7)).unwrap(),
                    StepOutcome::BarrierReached { at: at(STUB + 7) }
                );
                assert!(driver.with_barrier(|barrier| barrier.paused(id)).unwrap());
                let resumed_at = STUB + 2000;
                assert_eq!(
                    driver.step(at(resumed_at)).unwrap(),
                    StepOutcome::BarrierReached { at: at(resumed_at) }
                );
                let resumed = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
                fixture::assert_snapshot_reached(&resumed, at(resumed_at));
                fixture::assert_snapshot_ready(&resumed);
                let clock = Arc::new(SimulationClock::new(resumed_at));
                let mut copy =
                    TcgMachine::from_snapshot(Path::new(QEMU), &resumed, clock.clone()).unwrap();
                assert_eq!(registers(&mut copy)[RBX], 0);
                // The EOI handler's two instructions precede the increment.
                let woke = resumed_at + 3;
                assert_eq!(
                    driver.step(at(woke)).unwrap(),
                    StepOutcome::BarrierReached { at: at(woke) }
                );
                let after = driver.with_barrier(|barrier| barrier.snapshot(id)).unwrap();
                let mut copy = TcgMachine::from_snapshot(Path::new(QEMU), &after, clock).unwrap();
                assert_eq!(registers(&mut copy)[RBX], 1);
            }
        }
        mod lifecycle_tests {
            use crate::*;
            #[test]
            fn driver_reboots_live_and_crashed_guest_with_fresh_state_and_retained_actions() {
                for workers in [1, 2, 4] {
                    check_driver_reboot(workers);
                }
            }
            fn check_driver_reboot(workers: usize) {
                use seedbox_driver::driver::{Action, StepOutcome};
                use std::time::Duration;

                // Every time is one 10 ns quantum later than on a stubless
                // program, so the 12-instruction stub completes before the
                // first barrier and quantum boundaries keep their place.
                let t = |n: u64| at(10 + n);
                let mut driver = native_fixture::driver(
                    at(0),
                    Duration::from_nanos(10),
                    workers,
                    SeededRng::new(0),
                    || Ok(()),
                    || {
                        let clock: Arc<dyn Clock + Send + Sync> =
                            Arc::new(SimulationClock::new((at(0)).as_nanos()));
                        // The reboot subject must not be the oracle: an oracle crash
                        // permanently completes Driver, even with a future reboot queued.
                        (0..2)
                            .map(|_| {
                                TcgMachine::configured(
                                    Path::new(QEMU),
                                    manifest(counting_loop()), // Increment EBX; loop.
                                    SeededRng::new(0),
                                    clock.clone(),
                                )
                            })
                            .collect()
                    },
                )
                .unwrap();
                let (oracle, id) = driver
                    .with_barrier(|barrier| {
                        let mut machines = barrier.machines();
                        Ok((machines.next().unwrap().0, machines.next().unwrap().0))
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(at(10)).unwrap(),
                    StepOutcome::BarrierReached { at: at(10) }
                );
                assert_eq!(
                    driver.step(t(5)).unwrap(),
                    StepOutcome::BarrierReached { at: t(5) }
                );
                let before = driver
                    .with_barrier(|barrier| {
                        let saved = barrier.snapshot(id)?;
                        barrier.schedule(t(5), auto_key(), Action::Pause(Target::Machine(id)))?;
                        barrier.schedule(t(5), auto_key(), Action::Crash(Target::Machine(id)))?;
                        Ok(saved)
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(t(5)).unwrap(),
                    StepOutcome::BarrierReached { at: t(5) }
                );
                // Reboot requires an applied crash. A Reboot input queued with the Crash
                // would be admitted before the Crash effect applies, so it is scheduled at
                // the following barrier, still at the same simulation time.
                driver
                    .with_barrier(|barrier| {
                        barrier.schedule(t(5), auto_key(), Action::Reboot(Target::Machine(id)))
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(t(5)).unwrap(),
                    StepOutcome::BarrierReached { at: t(5) }
                );
                let after = driver
                    .with_barrier(|barrier| {
                        assert_eq!(barrier.machines().nth(1).unwrap().0, id);
                        assert!(!barrier.paused(id)?);
                        assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
                        fixture::assert_no_pending_boots(barrier.machine(id)?);
                        barrier.snapshot(id)
                    })
                    .unwrap();
                let mut old = TcgMachine::from_snapshot(
                    Path::new(QEMU),
                    &before,
                    Arc::new(SimulationClock::new(10 + 5)),
                )
                .unwrap();
                let mut fresh = TcgMachine::from_snapshot(
                    Path::new(QEMU),
                    &after,
                    Arc::new(SimulationClock::new(10 + 5)),
                )
                .unwrap();
                assert!(fixture::registers(&mut old).unwrap()[RBX] > 0);
                fresh.run(&mut (), t(5)).unwrap();
                assert_eq!(fixture::registers(&mut fresh).unwrap()[RBX], 0);

                driver
                    .with_barrier(|barrier| {
                        for (time, action) in [
                            (5, Action::Crash(Target::Machine(id))),
                            (8, Action::Reboot(Target::Machine(id))),
                            (9, Action::Resume(Target::Machine(id))),
                            (12, Action::Crash(Target::Machine(id))),
                            (12, Action::Crash(Target::Machine(oracle))),
                        ] {
                            barrier.schedule(t(time), auto_key(), action)?;
                        }
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(at(100)).unwrap(),
                    StepOutcome::BarrierReached { at: t(8) }
                );
                // A same-time pause would be admitted before the reboot effect applies
                // (and rejected for a crashed machine), so pause the new boot afterwards.
                driver
                    .with_barrier(|barrier| {
                        barrier.schedule(t(8), auto_key(), Action::Pause(Target::Machine(id)))
                    })
                    .unwrap();
                assert_eq!(
                    driver.step(t(8)).unwrap(),
                    StepOutcome::BarrierReached { at: t(8) }
                );
                driver
                    .with_barrier(|barrier| {
                        assert_eq!(barrier.machines().nth(1).unwrap().0, id);
                        assert!(barrier.paused(id)?);
                        assert_eq!(barrier.stopped(id)?, StopReason::BoundaryReached);
                        fixture::assert_no_pending_boots(barrier.machine(id)?);
                        fixture::assert_no_pending_at(barrier.machine(id)?);
                        fixture::assert_ready(barrier.machine(id)?);
                        Ok(())
                    })
                    .unwrap();
                for end in [9, 10] {
                    assert_eq!(
                        driver.step(at(100)).unwrap(),
                        StepOutcome::BarrierReached { at: t(end) }
                    );
                }
                assert_eq!(
                    driver.step(at(100)).unwrap(),
                    StepOutcome::Finished {
                        at: t(12),
                        reason: ExitReason::Crashed
                    }
                );
                driver
                    .with_barrier(|barrier| {
                        assert_eq!(
                            barrier.stopped(id)?,
                            StopReason::Exited {
                                reached: t(12),
                                reason: ExitReason::Crashed
                            }
                        );
                        Ok(())
                    })
                    .unwrap();
            }
        }
    }
}
