use super::*;
use crate::clock::Clock;
use crate::rng::SeededRng;
use std::sync::Arc;
type TcgMachine = crate::machine::test_environment::Environment<super::TcgMachine>;
use crate::driver::{Driver, MachineId, StepOutcome};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs,
    time::{Duration, Instant},
};

fn manifest(words: &[u32]) -> Manifest {
    Manifest {
        boot: Boot::Program(words.iter().flat_map(|word| word.to_le_bytes()).collect()),
        memory_mib: 16,
    }
}

struct TestClock(AtomicU64);

impl Clock for TestClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

fn clock(nanos: u64) -> Arc<TestClock> {
    Arc::new(TestClock(AtomicU64::new(nanos)))
}

fn start(words: &[u32]) -> TcgMachine {
    TcgMachine::new(
        Path::new("qemu-system-riscv64"),
        manifest(words),
        SeededRng::new(0),
        clock(0),
    )
    .expect("QEMU fixture startup; run tests inside nix develop")
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
        | StepOutcome::BeforeAction { .. }
        | StepOutcome::Stopped { .. } => panic!("unexpected playback control in test"),
    };
}

fn registers(machine: &mut TcgMachine) -> [u64; 33] {
    machine.process.as_mut().unwrap().registers().unwrap()
}

#[test]
fn execution_bounds_and_snapshot_branches_preserve_registers_and_ram() {
    // Set t0=RAM_BASE+0x1000; increment a0; store a0; repeat the last three instructions.
    let code = [
        0x0000_1297, // auipc t0, 1: t0 = PC + 0x1000, our scratch RAM address.
        0x0015_0513, // addi a0, a0, 1: increment the counter.
        0x00a2_b023, // sd a0, 0(t0): store the 64-bit counter in scratch RAM.
        0xff9f_f06f, // jal zero, -8: jump back two instructions without saving a link.
    ];
    let mut machine = start(&code);
    assert_eq!(
        machine.run(&mut (), at(8)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine)[10], 3);
    let snapshot = machine.snapshot().unwrap();
    let mut fresh = start(&code);
    fresh.run(&mut (), at(20)).unwrap();
    machine.run(&mut (), at(20)).unwrap();
    assert_eq!(registers(&mut machine), registers(&mut fresh));
    // Capture a second time after continuing, then destroy the source process
    // and all its temporary files before restoring the first snapshot twice.
    let _later = machine.snapshot().unwrap();
    drop(machine);
    let mut branch =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock(0)).unwrap();
    assert_eq!(registers(&mut branch)[10], 3);
    // Read eight bytes at RAM_BASE + 0x1000, the address computed by AUIPC above.
    let memory = branch
        .process
        .as_mut()
        .unwrap()
        .memory(0x8000_1000, 8)
        .unwrap();
    assert_eq!(memory, 2u64.to_le_bytes());
    branch.run(&mut (), at(20)).unwrap();
    assert_eq!(registers(&mut branch), registers(&mut fresh));
    let mut second =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock(0)).unwrap();
    second.run(&mut (), at(21)).unwrap();
    assert_ne!(registers(&mut second), registers(&mut branch));
}

// RISC-V virt's SiFive test finisher: writing 0x5555 to 0x100000 powers off.
const SHUTDOWN: [u32; 4] = [
    0x0010_02b7, // lui t0, 0x100: finisher MMIO address.
    0x0000_5337, // lui t1, 5: high part of the shutdown value.
    0x5553_0313, // addi t1, t1, 0x555: t1 = 0x5555 (FINISHER_PASS).
    0x0062_a023, // sw t1, 0(t0): request board shutdown.
];

#[test]
fn driver_starts_at_its_clock_and_records_early_exit() {
    let mut driver = crate::driver::fixture::driver(
        at(37),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(37)).as_nanos()));
            let machine = TcgMachine::configured(
                Path::new("qemu-system-riscv64"),
                manifest(&SHUTDOWN),
                SeededRng::new(0),
                clock,
            )?;
            assert_eq!(machine.pending_boots, 1);
            assert!(machine.process.is_none());
            Ok(vec![machine])
        },
    )
    .unwrap();
    let id = machine_id(&mut driver);
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished {
            at: at(41),
            reason: ExitReason::Shutdown
        }
    );
    assert_eq!(
        stopped(&mut driver, id),
        StopReason::Exited {
            reached: at(41),
            reason: ExitReason::Shutdown,
        }
    );
}

#[test]
fn execution_does_not_advance_shared_clock_and_adopts_elapsed_time() {
    let clock = clock(0);
    let mut machine = TcgMachine::new(
        Path::new("qemu-system-riscv64"),
        manifest(&[0x0015_0513, 0xffdf_f06f]),
        SeededRng::new(0), // addi a0,a0,1; jal zero,-4.
        clock.clone(),
    )
    .unwrap();
    machine.run(&mut (), at(10)).unwrap();
    assert_eq!(clock.now(), 0);
    assert_eq!(registers(&mut machine)[10], 5);
    clock.0.store(11, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(11)).unwrap(),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine)[10], 5);
    machine.run(&mut (), at(20)).unwrap();
    assert_eq!(registers(&mut machine)[10], 10);
}

#[test]
fn shutdown_stops_before_following_instructions_even_at_budget_boundary() {
    let mut words = SHUTDOWN.to_vec();
    words.extend([
        0x02a0_0513, // addi a0, zero, 42: must never execute after shutdown.
        0x0000_006f, // jal zero, 0: spin if shutdown failed to stop execution.
    ]);
    for limit in [4, 100_000] {
        let mut machine = start(&words);
        assert_eq!(
            machine.run(&mut (), at(limit)).unwrap(),
            StopReason::Exited {
                reached: at(4),
                reason: ExitReason::Shutdown,
            }
        );
        assert_eq!(registers(&mut machine)[10], 0);
    }
}

#[test]
fn idle_with_no_timer_stays_blocked_even_after_time_advances() {
    let mut machine = start(&[0x1050_0073]); // wfi: wait for interrupt.
    let blocked = StopReason::Blocked {
        reached: at(1),
        deadline: None,
    };
    assert_eq!(machine.run(&mut (), at(50)).unwrap(), blocked);
    assert_eq!(machine.run(&mut (), at(50)).unwrap(), blocked);
    let snapshot = machine.snapshot().unwrap();
    drop(machine);
    let clock = clock(1);
    let mut machine =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock.clone())
            .unwrap();
    assert_eq!(machine.run(&mut (), at(50)).unwrap(), blocked);
    clock.0.store(30, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(50)).unwrap(),
        StopReason::Blocked {
            reached: at(30),
            deadline: None
        }
    );
}

#[test]
fn idle_reports_timer_and_restored_guest_resumes_at_its_deadline() {
    // CLINT mtime runs at 10 MHz on virt. At the store (time 3), mtime is
    // still zero; QEMU schedules 10 ticks later, giving deadline 1003 ns.
    // Enable MTIE but leave global MIE clear: WFI wakes without entering a handler.
    let mut words = vec![
        0x0200_42b7, // lui t0, 0x2004: mtimecmp at 0x02004000.
        0x00a0_0313, // addi t1, zero, 10: timer compare ticks.
        0x0062_b023, // sd t1, 0(t0): program the timer.
        0x0800_0313, // addi t1, zero, 128: MTIE bit (1 << 7).
        0x3043_2073, // csrs mie, t1: enable the machine timer interrupt.
        0x1050_0073, // wfi: block until it becomes pending.
    ];
    words.extend(SHUTDOWN);
    let mut machine = start(&words);
    let blocked = StopReason::Blocked {
        reached: at(6),
        deadline: Some(at(1003)),
    };
    assert_eq!(machine.run(&mut (), at(500)).unwrap(), blocked);
    let snapshot = machine.snapshot().unwrap();
    drop(machine);
    let resumed_clock = clock(6);
    let mut machine = TcgMachine::from_snapshot(
        Path::new("qemu-system-riscv64"),
        &snapshot,
        resumed_clock.clone(),
    )
    .unwrap();
    assert_eq!(machine.run(&mut (), at(500)).unwrap(), blocked);
    resumed_clock.0.store(1004, Ordering::Relaxed);
    assert_eq!(
        machine.run(&mut (), at(1050)).unwrap(),
        StopReason::Exited {
            reached: at(1008),
            reason: ExitReason::Shutdown,
        }
    );
    // A second restored branch delivers the timer exactly at its deadline.
    let mut machine =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock(1003))
            .unwrap();
    assert_eq!(
        machine.run(&mut (), at(1050)).unwrap(),
        StopReason::Exited {
            reached: at(1007),
            reason: ExitReason::Shutdown
        }
    );
    let exited = machine.snapshot().unwrap();
    let mut restored =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &exited, clock(0)).unwrap();
    assert!(restored.run(&mut (), at(2000)).is_err());
}

#[test]
fn large_budget_matches_partitioned_execution_and_initial_snapshot() {
    let mut machine = start(&[0x0015_0513, 0xffdf_f06f]); // addi a0,a0,1; jal zero,-4.
    let initial = machine.snapshot().unwrap();
    machine.run(&mut (), at(1_000_001)).unwrap();
    assert_eq!(registers(&mut machine)[10], 500_001);
    let mut partitioned =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &initial, clock(0)).unwrap();
    for boundary in [1, 17, 256, 65_537, 1_000_001] {
        assert_eq!(
            partitioned.run(&mut (), at(boundary)).unwrap(),
            StopReason::BoundaryReached
        );
    }
    assert_eq!(registers(&mut machine), registers(&mut partitioned));
}

#[test]
fn crash_terminates_process_without_changing_saved_state() {
    let mut machine = start(&[
        0x0015_0513, // addi a0, a0, 1: increment the counter.
        0xffdf_f06f, // jal zero, -4: jump back one instruction.
    ]);
    machine.run(&mut (), at(4)).unwrap();
    let snapshot = machine.snapshot().unwrap();
    machine.request(crate::pv::Request::Crash).unwrap();
    assert!(machine.process.is_none());
    assert!(machine.run(&mut (), at(6)).is_err());
    assert!(matches!(
        machine.request(crate::pv::Request::Crash).unwrap(),
        StopReason::Exited {
            reason: ExitReason::Crashed,
            ..
        }
    ));
    let crashed = machine.snapshot().unwrap();
    let mut dead =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &crashed, clock(0)).unwrap();
    assert!(dead.run(&mut (), at(6)).is_err());
    let mut alive =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot, clock(0)).unwrap();
    alive.run(&mut (), at(6)).unwrap();
    assert_eq!(registers(&mut alive)[10], 3);
}

#[test]
fn invalid_requests_and_execution_failures_are_distinct() {
    assert!(
        // 0x0000_0013 is `addi zero, zero, 0`, the canonical RISC-V NOP.
        TcgMachine::new(
            Path::new("/missing/qemu"),
            manifest(&[0x0000_0013]),
            SeededRng::new(0),
            clock(0)
        )
        .err()
        .unwrap()
        .to_string()
        .contains("/missing/qemu")
    );
    let mut machine = start(&[
        0x0015_0513, // addi a0, a0, 1: increment the counter.
        0xffdf_f06f, // jal zero, -4: jump back one instruction.
    ]);
    machine.run(&mut (), at(2)).unwrap();
    assert!(machine.run(&mut (), at(1)).is_err());
    assert_eq!(
        machine.run(&mut (), at(2)).unwrap(),
        StopReason::BoundaryReached
    );
    // A dead emulator is a host execution error, never a simulated guest crash.
    machine.process.as_mut().unwrap().terminate().unwrap();
    assert!(machine.run(&mut (), at(3)).is_err());
    assert!(machine.snapshot().is_err());
}

#[test]
fn linux_boots_sleeps_and_shuts_down_with_the_machine_driver() {
    fn input(name: &str) -> std::path::PathBuf {
        std::env::var_os(name)
            .expect("run tests inside nix develop")
            .into()
    }
    let assets = input("SEEDBOX_TCG_LINUX");
    let mut driver = crate::driver::fixture::driver(
        at(0),
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> =
                Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
            TcgMachine::configured(
                &input("SEEDBOX_TCG_QEMU"),
                Manifest {
                    memory_mib: 256,
                    boot: Boot::Linux {
                        kernel: fs::read(assets.join("kernel")).unwrap(),
                        initramfs: fs::read(assets.join("initramfs")).unwrap(),
                        firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap(),
                        command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr".into(),
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
    let started = Instant::now();
    let exit_at = loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "Linux boot timed out: {}",
            String::from_utf8_lossy(&console(&mut driver, index))
        );
        step(&mut driver, index, &mut now, 1_000_000);
        match stopped(&mut driver, index) {
            StopReason::BoundaryReached => {}
            StopReason::Blocked {
                deadline: Some(_), ..
            } => {
                timer_wakeups += 1;
                let output = console(&mut driver, index);
                if fork.is_none()
                    && String::from_utf8_lossy(&output).contains("seedbox: Linux userspace booted")
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
    let mut driver = crate::driver::fixture::driver(
        fork.reached,
        std::time::Duration::from_millis(1),
        2,
        crate::rng::SeededRng::new(0),
        || Ok(()),
        || {
            let clock: Arc<dyn Clock + Send + Sync> = Arc::new(crate::clock::SimulationClock::new(
                (fork.reached).as_nanos(),
            ));
            TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &fork, clock)
                .map(|machine| vec![machine])
        },
    )
    .unwrap();
    let index = machine_id(&mut driver);
    let mut now = fork.reached;
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "Linux snapshot branch timed out"
        );
        step(&mut driver, index, &mut now, 131_071);
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
        kernel: fs::read(assets.join("kernel")).unwrap(),
        initramfs: fs::read(assets.join("initramfs")).unwrap(),
        firmware: fs::read(input("SEEDBOX_TCG_FIRMWARE")).unwrap(),
        command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr".into(),
    };
    let started = Instant::now();
    let mut reference = None;
    for (guests, step_limit, workers) in [
        ([("slow", "2"), ("fast", "1")], 1_000_000, 1),
        ([("slow", "2"), ("fast", "1")], 1_000_000, 2),
        ([("slow", "2"), ("fast", "1")], 131_071, 4),
        ([("fast", "1"), ("slow", "2")], 131_071, 4),
    ] {
        let mut driver = crate::driver::fixture::driver(
            at(0),
            std::time::Duration::from_millis(1),
            workers,
            crate::rng::SeededRng::new(0),
            || Ok(()),
            || {
                let clock: Arc<dyn Clock + Send + Sync> =
                    Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
                // The first machine is the oracle. Keep it alive beyond both
                // observed guests so neither creation order cuts off the other.
                std::iter::once(&("oracle", "3"))
                    .chain(guests.iter())
                    .map(|(name, sleep)| {
                        let mut boot = boot.clone();
                        if let Boot::Linux { command_line, .. } = &mut boot {
                            command_line
                                .push_str(&format!(" seedbox_name={name} seedbox_sleep={sleep}"));
                        }
                        TcgMachine::configured(
                            &input("SEEDBOX_TCG_QEMU"),
                            Manifest {
                                boot,
                                memory_mib: 256,
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
            .with_barrier(|barrier| Ok(barrier.machines().skip(1).map(|(id, _)| id).collect()))
            .unwrap();
        let mut now = at(0);
        let mut booted = [false; 2];
        let mut woke = [false; 2];
        let mut wake_order = Vec::new();
        loop {
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "paired Linux boot timed out"
            );
            let before = now;
            let upto = at(before.as_nanos() + step_limit);
            let outcome = driver.step(upto).unwrap();
            now = match outcome {
                StepOutcome::BarrierReached { at } | StepOutcome::Finished { at, .. } => at,
                StepOutcome::Blocked { .. } => panic!("Linux guests blocked without timers"),
                StepOutcome::Waiting { .. }
                | StepOutcome::BeforeAction { .. }
                | StepOutcome::Stopped { .. } => panic!("unexpected playback control in test"),
            };
            assert!(now >= before && now <= upto);
            driver
                .with_barrier(|barrier| {
                    for (index, &id) in ids.iter().enumerate() {
                        let guest = barrier.machine(id)?;
                        // Visible state never comes from beyond this barrier.
                        assert!(guest.reached <= now);
                        let output = String::from_utf8(guest.console().unwrap()).unwrap();
                        booted[index] |= output.contains("seedbox: Linux userspace booted");
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
    use crate::driver::{Action, PlaybackRate};
    let code = [0x0015_0513, 0xffdf_f06f]; // Increment a0; jump back one instruction.
    let mut reference = None;
    for workers in [1, 2, 4] {
        let mut driver = crate::driver::fixture::driver(
            at(0),
            Duration::from_nanos(10),
            workers,
            crate::rng::SeededRng::new(0),
            || Ok(()),
            || {
                let clock: Arc<dyn Clock + Send + Sync> =
                    Arc::new(crate::clock::SimulationClock::new((at(0)).as_nanos()));
                TcgMachine::configured(
                    Path::new("qemu-system-riscv64"),
                    manifest(&code),
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
                assert_eq!(barrier.machine(id)?.reached, at(0));
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
        let saved = driver
            .with_barrier(|barrier| {
                assert!(!barrier.playback().stepping);
                barrier.snapshot(id)
            })
            .unwrap();
        let mut restored =
            TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &saved, clock(10)).unwrap();
        let registers = registers(&mut restored);
        assert_eq!(registers[10], 5);
        if let Some(reference) = reference {
            assert_eq!(registers, reference);
        } else {
            reference = Some(registers);
        }
        driver
            .with_barrier(|barrier| {
                barrier.schedule(at(15), Action::Crash(id))?;
                barrier.pause_before_next_crash(PlaybackRate::Unlimited)
            })
            .unwrap();
        let held = StepOutcome::BeforeAction {
            at: at(15),
            action: Action::Crash(id),
        };
        assert_eq!(driver.step(at(100)).unwrap(), held);
        for _ in 0..2 {
            assert_eq!(driver.step(at(100)).unwrap(), held);
        }
        driver
            .with_barrier(|barrier| {
                assert_eq!(barrier.machine(id)?.reached, at(15));
                assert!(matches!(barrier.machine(id)?.state, State::Ready));
                barrier.set_rate(PlaybackRate::Unlimited)
            })
            .unwrap();
        assert_eq!(
            driver.step(at(100)).unwrap(),
            StepOutcome::Finished {
                at: at(15),
                // Crashing the oracle completes the simulation with its actual
                // exit reason, not the old all-machines-exited sentinel.
                reason: ExitReason::Crashed
            }
        );
        assert_eq!(
            stopped(&mut driver, id),
            StopReason::Exited {
                reached: at(15),
                reason: ExitReason::Crashed
            }
        );
    }
}

#[test]
fn deferred_startup_failure_is_a_run_error_and_invalidates_the_machine() {
    let mut machine =
        super::TcgMachine::new_raw(Path::new("/missing/qemu"), manifest(&[0x0000_0013])).unwrap();
    let clock = TestClock(AtomicU64::new(0));
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
    assert!(error.to_string().contains("/missing/qemu"));
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

// Replace one newc member in the Linux fixture without requiring host cpio.
fn lifecycle_tx_initramfs(input: &[u8]) -> Vec<u8> {
    // A simulated 10ms wait allows Linux's asynchronous TTY worker to drain
    // each short UART marker before the next device is written.
    lifecycle_initramfs(
        input,
        "while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done\nexec 9<>/dev/vport0p1\nprintf console-A >/dev/ttyS0\n/bin/busybox sleep 0.01\nprintf fra >&9\nprintf console-B >/dev/ttyS0\n/bin/busybox sleep 0.01\nprintf 'gment\\nsecond\\n' >&9\nprintf console-C >/dev/ttyS0\n/bin/busybox sleep 0.01\necho 'seedbox: Linux userspace booted'",
    )
}
fn lifecycle_initramfs(input: &[u8], injection: &str) -> Vec<u8> {
    transform_initramfs(input, |init| {
        init.replace("echo 'seedbox: Linux userspace booted'", injection)
    })
}
fn transform_initramfs(input: &[u8], transform: impl Fn(&str) -> String) -> Vec<u8> {
    let mut output = Vec::new();
    let mut at = 0;
    while at + 110 <= input.len() {
        let mut header = input[at..at + 110].to_vec();
        assert_eq!(&header[..6], b"070701");
        let hex = |offset| {
            usize::from_str_radix(
                std::str::from_utf8(&header[offset..offset + 8]).unwrap(),
                16,
            )
            .unwrap()
        };
        let size = hex(54);
        let namesize = hex(94);
        let name_end = at + 110 + namesize;
        let data_at = (name_end + 3) & !3;
        let end = (data_at + size + 3) & !3;
        let name = &input[at + 110..name_end - 1];
        let bytes = if name == b"init" || name == b"./init" {
            transform(std::str::from_utf8(&input[data_at..data_at + size]).unwrap()).into_bytes()
        } else {
            input[data_at..data_at + size].to_vec()
        };
        header[54..62].copy_from_slice(format!("{:08x}", bytes.len()).as_bytes());
        output.extend(header);
        output.extend_from_slice(&input[at + 110..data_at]);
        output.extend(bytes);
        while output.len() % 4 != 0 {
            output.push(0);
        }
        at = end;
        if name == b"TRAILER!!!" {
            break;
        }
    }
    output
}
#[test]
#[ignore = "requires TCG Linux fixture"]
fn linux_lifecycle_tx_retains_fragments_and_snapshot_drain_position() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    let manifest = Manifest {
        memory_mib: 256,
        boot: Boot::Linux {
            kernel: fs::read(assets.join("kernel")).unwrap(),
            initramfs: lifecycle_tx_initramfs(&fs::read(assets.join("initramfs")).unwrap()),
            firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap()).unwrap(),
            command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr seedbox_sleep=100"
                .into(),
        },
    };
    let mut source = Process::start(&qemu, &manifest, None, 3).unwrap();
    let mut now = at(0);
    let started = std::time::Instant::now();
    for _ in 0..10000 {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(60),
            "lifecycle TX guest did not become ready"
        );
        let result = source
            .execute(now, at(now.as_nanos() + 1_000_000), Default::default())
            .unwrap();
        now = timestamp(&result, "time").unwrap();
        if String::from_utf8_lossy(&source.console().unwrap())
            .contains("seedbox: Linux userspace booted")
        {
            break;
        }
        if result["reason"] == "blocked" {
            now = timestamp(&result, "deadline").unwrap();
            source.advance(now).unwrap();
        }
    }
    use crate::pv::transport::GuestWrite;
    let pending = source.snapshot().unwrap();
    let writes = source.drain_guest().unwrap();
    assert!(writes.windows(2).all(|w| w[0].time() <= w[1].time()));
    let lifecycle: Vec<_> = writes
        .iter()
        .enumerate()
        .filter_map(|(i, w)| match w {
            GuestWrite::Lifecycle(write) => Some((i, write)),
            GuestWrite::Console(_) => None,
        })
        .collect();
    assert_eq!(
        lifecycle
            .iter()
            .flat_map(|(_, w)| w.bytes.iter().copied())
            .collect::<Vec<_>>(),
        b"fragment\nsecond\n"
    );
    let console = |range: std::ops::Range<usize>| -> String {
        String::from_utf8_lossy(
            &writes[range]
                .iter()
                .flat_map(|w| match w {
                    GuestWrite::Console(w) => w.bytes.clone(),
                    GuestWrite::Lifecycle(_) => vec![],
                })
                .collect::<Vec<_>>(),
        )
        .into_owned()
    };
    assert_eq!(
        console(0..writes.len()).as_bytes(),
        source.console().unwrap()
    );
    let first = lifecycle.first().unwrap().0;
    let last = lifecycle.last().unwrap().0;
    assert!(console(0..first).contains("console-A"));
    assert!(console(first..last).contains("console-B"));
    assert!(console(last..writes.len()).contains("console-C"));
    let mut restored = Process::start(&qemu, &manifest, Some(&pending), 3).unwrap();
    assert_eq!(restored.drain_guest().unwrap(), writes);
    assert!(restored.drain_guest().unwrap().is_empty());
    let drained = restored.snapshot().unwrap();
    let mut restored = Process::start(&qemu, &manifest, Some(&drained), 3).unwrap();
    assert!(restored.drain_guest().unwrap().is_empty());
}

#[test]
#[ignore = "requires TCG Linux fixture"]
fn lifecycle_rx_backpressure_and_pending_suffix_survive_qemu_migration() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    let manifest = Manifest {
        memory_mib: 256,
        boot: Boot::Linux {
            kernel: fs::read(assets.join("kernel")).unwrap(),
            initramfs: lifecycle_initramfs(
                &fs::read(assets.join("initramfs")).unwrap(),
                "while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done\nexec 9<>/dev/vport0p1\necho RX_OPEN\n/bin/busybox sleep 0.1\n/bin/busybox dd bs=700013 count=1 iflag=fullblock <&9 >&9 2>/dev/null\necho RX_DONE\n/bin/busybox sleep 100",
            ),
            firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap()).unwrap(),
            command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr seedbox_sleep=100"
                .into(),
        },
    };
    let bytes: Vec<_> = (0..700013).map(|i| (i % 251) as u8).collect();
    let mut source = Process::start(&qemu, &manifest, None, 3).unwrap();
    source.input(&bytes[..25000]).unwrap();
    source.input(&bytes[25000..]).unwrap();
    let mut now = at(0);
    let started = Instant::now();
    loop {
        assert!(started.elapsed() < Duration::from_secs(60));
        let result = source
            .execute(now, at(now.as_nanos() + 1_000_000), Default::default())
            .unwrap();
        now = timestamp(&result, "time").unwrap();
        if String::from_utf8_lossy(&source.console().unwrap()).contains("RX_OPEN") {
            break;
        }
        if result["reason"] == "blocked" {
            now = timestamp(&result, "deadline").unwrap();
            source.advance(now).unwrap();
        }
    }
    let pending = source.snapshot().unwrap();
    let mut restored = Process::start(&qemu, &manifest, Some(&pending), 3).unwrap();
    assert!(restored.drain_lifecycle().unwrap().is_empty());
    let mut observed = Vec::new();
    loop {
        assert!(started.elapsed() < Duration::from_secs(60));
        let result = restored
            .execute(now, at(now.as_nanos() + 1_000_000), Default::default())
            .unwrap();
        now = timestamp(&result, "time").unwrap();
        for write in restored.drain_lifecycle().unwrap() {
            observed.extend(write.bytes);
        }
        if String::from_utf8_lossy(&restored.console().unwrap()).contains("RX_DONE") {
            break;
        }
        if result["reason"] == "blocked" {
            now = timestamp(&result, "deadline").unwrap();
            restored.advance(now).unwrap();
        }
    }
    assert_eq!(observed, bytes);
    assert!(restored.drain_lifecycle().unwrap().is_empty());
}

#[test]
#[ignore = "requires patched QEMU and TCG Linux inputs"]
fn application_outcome_remains_distinct_from_later_shutdown_across_snapshot() {
    let assets = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_LINUX").unwrap());
    let qemu = std::path::PathBuf::from(std::env::var_os("SEEDBOX_TCG_QEMU").unwrap());
    fn advance(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<crate::pv::Output>,
    ) -> StopReason {
        if let State::Blocked {
            deadline: Some(deadline),
        } = machine.state
        {
            clock.0.store(deadline.as_nanos(), Ordering::Relaxed);
        }
        let end = machine.reached.as_nanos().max(clock.now()) + 100_000;
        machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock,
                    rng,
                    inbox: crate::pv::Inbox::new(&mut Default::default(), at(clock.now())),
                    outbox: crate::pv::Outbox::new(outputs, at(clock.now()), at(end)),
                },
                at(end),
            )
            .unwrap()
    }
    fn finish(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<crate::pv::Output>,
    ) -> StopReason {
        let wall = Instant::now();
        loop {
            assert!(wall.elapsed() < Duration::from_secs(30));
            let stop = advance(machine, clock, rng, outputs);
            if matches!(stop, StopReason::Exited { .. }) {
                return stop;
            }
        }
    }
    fn run_at(
        machine: &mut super::TcgMachine,
        clock: &TestClock,
        rng: &mut SeededRng,
        outputs: &mut Vec<crate::pv::Output>,
    ) -> StopReason {
        clock.0.store(
            clock.now().max(machine.reached.as_nanos()),
            Ordering::Relaxed,
        );
        let now = at(clock.now());
        machine
            .run(
                &mut RunContext {
                    execution: Default::default(),
                    worker: &mut (),
                    clock,
                    rng,
                    inbox: crate::pv::Inbox::new(&mut Default::default(), now),
                    outbox: crate::pv::Outbox::new(outputs, now, now),
                },
                now,
            )
            .unwrap()
    }
    for code in [0, 23] {
        // This child defines the fixture's lifetime. Its actual wait status is
        // reported first; PID1 deliberately keeps Linux alive before poweroff.
        let script = format!(
            r#"#!/bin/sh
set -eu
/bin/busybox mount -t devtmpfs devtmpfs /dev
/bin/busybox mount -t proc proc /proc
/bin/busybox mount -t sysfs sysfs /sys
exec </dev/console >/dev/console 2>&1
/bin/busybox modprobe virtio_mmio
/bin/busybox modprobe virtio_console
while [ ! -e /sys/class/virtio-ports/vport0p1/name ]; do /bin/busybox sleep 0.001; done
exec 9<>/dev/vport0p1
printf 'SBX_READY1\n' >&9
set +e
/bin/busybox sh -c 'exit {code}'
status=$?
set -e
printf 'SBX_EXIT1 status=%s\n' "$status" >&9
/bin/busybox sleep 0.05
/bin/busybox poweroff -f
"#
        );
        let manifest = Manifest {
            memory_mib: 256,
            boot: Boot::Linux {
                kernel: fs::read(assets.join("kernel")).unwrap(),
                initramfs: transform_initramfs(
                    &fs::read(assets.join("initramfs")).unwrap(),
                    |_| script.clone(),
                ),
                firmware: fs::read(std::env::var_os("SEEDBOX_TCG_FIRMWARE").unwrap()).unwrap(),
                command_line: "console=ttyS0 earlycon=sbi rdinit=/init nokaslr".into(),
            },
        };
        let clock = clock(0);
        let mut rng = SeededRng::new(7);
        let mut outputs: Vec<crate::pv::Output> = Vec::new();
        let mut source = super::TcgMachine::new_raw(&qemu, manifest).unwrap();
        let wall = Instant::now();
        while !outputs.iter().any(|output| {
            matches!(
                output.payload,
                crate::pv::Response::ApplicationOutcome { .. }
            )
        }) {
            assert!(wall.elapsed() < Duration::from_secs(30));
            assert!(!matches!(
                advance(&mut source, &clock, &mut rng, &mut outputs),
                StopReason::Exited { .. }
            ));
        }
        assert!(outputs
            .iter()
            .any(|output| output.payload
                == crate::pv::Response::ApplicationOutcome { code: code as u8 }));
        let pending_rng = rng.clone();
        assert!(
            !matches!(source.state, State::Exited(_)),
            "application outcome must leave Linux running"
        );
        let pending = source.snapshot().unwrap();
        let expected = finish(&mut source, &clock, &mut rng, &mut outputs);
        assert!(matches!(
            expected,
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                ..
            }
        ));
        for _ in 0..2 {
            let branch_clock = super::tests::clock(pending.reached.as_nanos());
            let mut branch = super::TcgMachine::from_snapshot(&qemu, &pending).unwrap();
            let mut branch_rng = pending_rng.clone();
            let mut branch_outputs = Vec::new();
            assert_eq!(
                finish(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                expected
            );
            assert_eq!(
                run_at(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                expected
            );
            let adopted = branch.reached.as_nanos() + 17;
            branch_clock.0.store(adopted, Ordering::Relaxed);
            assert_eq!(
                run_at(
                    &mut branch,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                StopReason::Exited {
                    reached: at(adopted),
                    reason: ExitReason::Shutdown
                }
            );
            let exited = branch.snapshot().unwrap();
            let mut restored = super::TcgMachine::from_snapshot(&qemu, &exited).unwrap();
            assert_eq!(
                run_at(
                    &mut restored,
                    &branch_clock,
                    &mut branch_rng,
                    &mut branch_outputs
                ),
                StopReason::Exited {
                    reached: at(adopted),
                    reason: ExitReason::Shutdown
                }
            );
        }
        let crash_clock = super::tests::clock(pending.reached.as_nanos());
        let mut crashed = super::TcgMachine::from_snapshot(&qemu, &pending).unwrap();
        assert!(matches!(
            crate::machine::test_environment::run_requests(
                &mut crashed,
                crash_clock.as_ref(),
                &mut pending_rng.clone(),
                [crate::pv::Request::Crash],
                at(crash_clock.now())
            )
            .unwrap(),
            StopReason::Exited {
                reason: ExitReason::Crashed,
                ..
            }
        ));
    }
}

// Constructor checks remain portable: process creation is deferred until run.
fn common_inputs() -> (
    crate::application::GuestBootstrap,
    crate::definition::MachineDefinition,
) {
    use crate::application::{GuestBootstrap, Kernel};
    use crate::definition::*;
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(1);
    header.set_mode(0o755);
    header.set_uid(0);
    header.set_gid(0);
    header.set_mtime(0);
    header.set_cksum();
    tar.append_data(&mut header, "application", &[0_u8][..])
        .unwrap();
    (
        GuestBootstrap {
            kernel: Kernel {
                image: vec![1],
                command_line: "custom=verbatim".into(),
            },
            init: vec![2],
            base_initramfs: Vec::new(),
        },
        MachineDefinition {
            name: "portable".into(),
            resources: Resources {
                memory_bytes: 128 << 20,
            },
            application: PreparedApplication {
                filesystem: FilesystemArchive::new(tar.into_inner().unwrap()).unwrap(),
                launch: Launch {
                    argv: vec!["/application".into()],
                    environment: Default::default(),
                    working_directory: "/".into(),
                    user: "0:0".into(),
                },
            },
        },
    )
}

#[test]
fn common_constructor_preserves_inputs_and_reuses_prepared_application() {
    let (bootstrap, definition) = common_inputs();
    let original = definition.application.clone();
    let first = super::TcgMachine::new(
        Path::new("/missing/qemu"),
        vec![3],
        &bootstrap,
        definition.clone(),
    )
    .unwrap();
    let second = super::TcgMachine::new(
        Path::new("/missing/qemu"),
        vec![3],
        &bootstrap,
        definition.clone(),
    )
    .unwrap();
    assert_eq!(definition.application, original);
    assert_eq!(first.manifest.memory_mib, 128);
    match (&first.manifest.boot, &second.manifest.boot) {
        (
            Boot::Linux {
                kernel,
                initramfs,
                command_line,
                ..
            },
            Boot::Linux {
                initramfs: again, ..
            },
        ) => {
            assert_eq!(kernel, &[1]);
            assert_eq!(command_line, "custom=verbatim");
            assert_eq!(initramfs, again);
        }
        _ => unreachable!(),
    }
}

#[test]
fn common_constructor_checks_resource_and_kernel_argument_bounds() {
    let (mut bootstrap, definition) = common_inputs();
    for memory_bytes in [0, 1, (u64::from(u32::MAX) + 1) << 20] {
        let mut invalid = definition.clone();
        invalid.resources.memory_bytes = memory_bytes;
        assert!(
            super::TcgMachine::new(Path::new("/missing/qemu"), vec![3], &bootstrap, invalid)
                .is_err()
        );
    }
    for accepted in ["a".repeat(1023), format!("{}a", "é".repeat(511))] {
        bootstrap.kernel.command_line = accepted;
        assert!(super::TcgMachine::new(
            Path::new("/missing/qemu"),
            vec![3],
            &bootstrap,
            definition.clone()
        )
        .is_ok());
    }
    for rejected in ["a".repeat(1024), "é".repeat(512), "a\0b".into()] {
        bootstrap.kernel.command_line = rejected;
        assert!(super::TcgMachine::new(
            Path::new("/missing/qemu"),
            vec![3],
            &bootstrap,
            definition.clone()
        )
        .is_err());
    }
}
