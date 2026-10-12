use super::*;
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
    TcgMachine::new(Path::new("qemu-system-riscv64"), manifest(words), clock(0))
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
        StepOutcome::BarrierReached { at } | StepOutcome::Finished { at } => at,
        StepOutcome::Blocked { .. } => panic!("guest blocked without a timer"),
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
    let mut driver = Driver::new(at(37), (), |clock| {
        let machine =
            TcgMachine::new(Path::new("qemu-system-riscv64"), manifest(&SHUTDOWN), clock)?;
        assert_eq!(machine.reached, at(37));
        Ok(vec![machine])
    })
    .unwrap();
    let id = machine_id(&mut driver);
    assert_eq!(
        driver.step(at(100)).unwrap(),
        StepOutcome::Finished { at: at(41) }
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
fn execution_does_not_advance_shared_clock_or_allow_skipped_work() {
    let clock = clock(0);
    let mut machine = TcgMachine::new(
        Path::new("qemu-system-riscv64"),
        manifest(&[0x0015_0513, 0xffdf_f06f]), // addi a0,a0,1; jal zero,-4.
        clock.clone(),
    )
    .unwrap();
    machine.run(&mut (), at(10)).unwrap();
    assert_eq!(clock.now(), 0);
    assert_eq!(registers(&mut machine)[10], 5);
    clock.0.store(11, Ordering::Relaxed);
    assert!(machine.run(&mut (), at(20)).is_err());
    assert_eq!(registers(&mut machine)[10], 5);
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
    assert!(machine.run(&mut (), at(1050)).is_err());
    // Restore the environment consistently after demonstrating the invalid jump.
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
    machine.crash().unwrap();
    assert!(machine.process.is_none());
    assert!(machine.run(&mut (), at(6)).is_err());
    assert!(machine.crash().is_err());
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
    let mut driver = Driver::new(at(0), (), |clock| {
        TcgMachine::new(
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
            clock,
        )
        .map(|machine| vec![machine])
    })
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
    let mut driver = Driver::new(fork.reached, (), |clock| {
        TcgMachine::from_snapshot(&input("SEEDBOX_TCG_QEMU"), &fork, clock)
            .map(|machine| vec![machine])
    })
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
    for (guests, quantum) in [
        ([("slow", "2"), ("fast", "1")], 1_000_000),
        ([("fast", "1"), ("slow", "2")], 131_071),
    ] {
        let mut driver = Driver::new(at(0), (), |clock| {
            guests
                .iter()
                .map(|(name, sleep)| {
                    let mut boot = boot.clone();
                    if let Boot::Linux { command_line, .. } = &mut boot {
                        command_line
                            .push_str(&format!(" seedbox_name={name} seedbox_sleep={sleep}"));
                    }
                    TcgMachine::new(
                        &input("SEEDBOX_TCG_QEMU"),
                        Manifest {
                            boot,
                            memory_mib: 256,
                        },
                        clock.clone(),
                    )
                })
                .collect()
        })
        .unwrap();
        // Machines and their read-only clock can be transferred to workers.
        // Dispatch remains serial until the effect/assignment contract is added.
        fn assert_send<T: Send>() {}
        assert_send::<Driver<TcgMachine>>();
        let ids: Vec<_> = driver
            .with_barrier(|barrier| Ok(barrier.machines().map(|(id, _)| id).collect()))
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
            let upto = at(before.as_nanos() + quantum);
            let outcome = driver.step(upto).unwrap();
            now = match outcome {
                StepOutcome::BarrierReached { at } | StepOutcome::Finished { at } => at,
                StepOutcome::Blocked { .. } => panic!("Linux guests blocked without timers"),
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
        if let Some(reference) = &reference {
            assert_eq!(&exits, reference);
        } else {
            reference = Some(exits);
        }
    }
}
