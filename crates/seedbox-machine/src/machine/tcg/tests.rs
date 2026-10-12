use super::*;
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

fn start(words: &[u32]) -> TcgMachine {
    TcgMachine::new(Path::new("qemu-system-riscv64"), manifest(words))
        .expect("QEMU fixture startup; run tests inside nix develop")
}

fn at(nanos: u64) -> Timestamp {
    Timestamp::from_nanos(nanos)
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
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot).unwrap();
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
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot).unwrap();
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
    let mut machine =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot).unwrap();
    assert_eq!(machine.run(&mut (), at(50)).unwrap(), blocked);
    assert!(machine.wake(at(0)).is_err());
    machine.wake(at(30)).unwrap();
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
    let mut machine =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot).unwrap();
    assert_eq!(machine.run(&mut (), at(500)).unwrap(), blocked);
    assert!(machine.wake(at(1004)).is_err());
    machine.wake(at(1003)).unwrap();
    assert_eq!(
        machine.run(&mut (), at(1050)).unwrap(),
        StopReason::Exited {
            reached: at(1007),
            reason: ExitReason::Shutdown
        }
    );
    let exited = machine.snapshot().unwrap();
    let mut restored =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &exited).unwrap();
    assert!(restored.run(&mut (), at(2000)).is_err());
}

#[test]
fn large_budget_matches_partitioned_execution_and_initial_snapshot() {
    let mut machine = start(&[0x0015_0513, 0xffdf_f06f]); // addi a0,a0,1; jal zero,-4.
    let initial = machine.snapshot().unwrap();
    machine.run(&mut (), at(1_000_001)).unwrap();
    assert_eq!(registers(&mut machine)[10], 500_001);
    let mut partitioned =
        TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &initial).unwrap();
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
    let mut dead = TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &crashed).unwrap();
    assert!(dead.run(&mut (), at(6)).is_err());
    let mut alive = TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), &snapshot).unwrap();
    alive.run(&mut (), at(6)).unwrap();
    assert_eq!(registers(&mut alive)[10], 3);
}

#[test]
fn invalid_requests_and_execution_failures_are_distinct() {
    assert!(
        // 0x0000_0013 is `addi zero, zero, 0`, the canonical RISC-V NOP.
        TcgMachine::new(Path::new("/missing/qemu"), manifest(&[0x0000_0013]))
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
    let mut machine = TcgMachine::new(
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
    )
    .unwrap();
    let mut now = 0;
    let mut timer_wakeups = 0;
    let mut fork = None;
    let mut output_at_fork = 0;
    let started = Instant::now();
    let exit_at = loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "Linux boot timed out: {}",
            String::from_utf8_lossy(&machine.console().unwrap())
        );
        let upto = at(now + 1_000_000);
        match machine.run(&mut (), upto).unwrap() {
            StopReason::BoundaryReached => now = upto.as_nanos(),
            StopReason::Blocked {
                deadline: Some(deadline),
                ..
            } => {
                timer_wakeups += 1;
                let output = machine.console().unwrap();
                if fork.is_none()
                    && String::from_utf8_lossy(&output).contains("seedbox: Linux userspace booted")
                {
                    output_at_fork = output.len();
                    fork = Some(machine.snapshot().unwrap());
                }
                machine.wake(deadline).unwrap();
                now = deadline.as_nanos();
            }
            StopReason::Exited {
                reason: ExitReason::Shutdown,
                reached,
            } => break reached,
            reason => panic!(
                "unexpected Linux stop: {reason:?}\n{}",
                String::from_utf8_lossy(&machine.console().unwrap())
            ),
        }
    };
    let output = String::from_utf8(machine.console().unwrap()).unwrap();
    assert!(
        output.contains("seedbox: Linux userspace booted"),
        "{output}"
    );
    assert!(output.contains("seedbox: Linux timer woke"), "{output}");
    assert!(timer_wakeups > 0);
    drop(machine);

    // Restore after source destruction and drive with a different quantum.
    // The remaining console bytes and final simulation position must agree.
    let mut branch = TcgMachine::from_snapshot(
        &input("SEEDBOX_TCG_QEMU"),
        &fork.expect("Linux slept after its boot marker"),
    )
    .unwrap();
    let mut now = branch.reached.as_nanos();
    loop {
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "Linux snapshot branch timed out"
        );
        let upto = at(now + 131_071);
        match branch.run(&mut (), upto).unwrap() {
            StopReason::BoundaryReached => now = upto.as_nanos(),
            StopReason::Blocked {
                deadline: Some(deadline),
                ..
            } => {
                branch.wake(deadline).unwrap();
                now = deadline.as_nanos();
            }
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
        branch.console().unwrap(),
        &output.as_bytes()[output_at_fork..]
    );
}
