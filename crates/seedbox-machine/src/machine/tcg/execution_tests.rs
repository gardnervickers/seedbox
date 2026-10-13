//! Native reference-backend tests: inspect CPU state independently through GDB.
use super::*;
use crate::{
    clock::SimulationClock,
    execution::ExecutionConfig,
    pv::{Inbox, Input, Outbox, Request},
    rng::SeededRng,
};
use std::collections::VecDeque;

const LOOP: [u32; 2] = [
    0x0015_0513, // addi a0, a0, 1: count alternate retired instructions.
    0xffdf_f06f, // jal zero, -4: return to the increment.
];
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn start(code: &[u32]) -> TcgMachine {
    TcgMachine::new_raw(
        Path::new("qemu-system-riscv64"),
        Manifest {
            boot: Boot::Program(code.iter().flat_map(|word| word.to_le_bytes()).collect()),
            memory_mib: 16,
        },
    )
    .unwrap()
}
fn drive(
    machine: &mut TcgMachine,
    now: u64,
    end: u64,
    factor: u32,
    requests: Vec<Request>,
) -> StopReason {
    let clock = SimulationClock::new(now);
    let mut rng = SeededRng::new(7);
    let mut inbox: VecDeque<_> = requests
        .into_iter()
        .map(|payload| Input {
            at: at(now),
            payload,
        })
        .collect();
    let mut outputs = Vec::new();
    machine
        .run(
            &mut RunContext {
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                execution: ExecutionConfig::new(factor).unwrap(),
                inbox: Inbox::new(&mut inbox, at(now)),
                outbox: Outbox::new(&mut outputs, at(now), at(end)),
            },
            at(end),
        )
        .unwrap()
}
fn registers(machine: &mut TcgMachine) -> [u64; 33] {
    machine.process().unwrap().registers().unwrap()
}
fn branch(snapshot: &Snapshot) -> TcgMachine {
    TcgMachine::from_snapshot(Path::new("qemu-system-riscv64"), snapshot).unwrap()
}

#[test]
fn execution_cost_fractional_and_maximum_rates_match_segmented_cpu_work() {
    for factor in [1000, 1001, 1_000_000] {
        let mut whole = start(&LOOP);
        drive(&mut whole, 0, 0, factor, vec![]);
        let initial = whole.snapshot().unwrap();
        let end = 1001 * u64::from(factor);
        assert_eq!(
            drive(&mut whole, 0, end, factor, vec![]),
            StopReason::BoundaryReached
        );
        let mut split = branch(&initial);
        for bound in [1, 2, 3, 17, 999, 1000, 1001, end] {
            assert_eq!(
                drive(&mut split, 0, bound, factor, vec![]),
                StopReason::BoundaryReached
            );
            // Cumulative charging rounds completions, never each instruction's cost.
            let retired = bound * 1000 / u64::from(factor);
            assert_eq!(
                registers(&mut split)[10],
                retired.div_ceil(2),
                "factor={factor}, bound={bound}"
            );
        }
        assert_eq!(registers(&mut split), registers(&mut whole));
    }
}

#[test]
fn execution_cost_snapshot_and_rate_change_keep_old_partial_charge() {
    let mut machine = start(&LOOP);
    drive(&mut machine, 0, 1, 1001, vec![]);
    assert_eq!(registers(&mut machine)[10], 0);
    let first = machine.snapshot().unwrap();
    let mut restored = branch(&first);
    for candidate in [&mut machine, &mut restored] {
        // The first instruction still owes .001 ns at the old rate.
        drive(candidate, 0, 2, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[10], 1);
        drive(candidate, 0, 1001, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0004);
        drive(candidate, 0, 1002, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0000);
    }
    assert_eq!(registers(&mut machine), registers(&mut restored));
    // A changed rate at a nonintegral completed instruction also retains the
    // charge already started with the fractional remainder of that completion.
    let mut machine = branch(&first);
    drive(&mut machine, 0, 2, 1001, vec![]);
    let partial_next = machine.snapshot().unwrap();
    let mut restored = branch(&partial_next);
    for candidate in [&mut machine, &mut restored] {
        drive(candidate, 0, 3, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0000);
        drive(candidate, 0, 1002, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[10], 1);
        drive(candidate, 0, 1003, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[10], 2);
    }
}

#[test]
fn execution_cost_adoption_snapshot_and_reboot_preserve_external_authority() {
    let mut machine = start(&LOOP);
    drive(&mut machine, 0, 500, 1_000_000, vec![]);
    let partial = machine.snapshot().unwrap();
    let mut restored = branch(&partial);
    for candidate in [&mut machine, &mut restored] {
        let before = registers(candidate);
        drive(candidate, 10_000, 10_000, 1001, vec![]);
        assert_eq!(registers(candidate), before);
        drive(candidate, 10_000, 10_499, 1001, vec![]);
        assert_eq!(registers(candidate)[10], 0);
        drive(candidate, 10_000, 10_500, 1001, vec![]);
        assert_eq!(registers(candidate)[10], 1);
        drive(candidate, 10_000, 10_501, 1001, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0004);
        drive(candidate, 10_000, 10_502, 1001, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0000);
    }
    // Preserve sub-nanosecond carry as well as a large integer partial charge.
    let mut fractional = start(&LOOP);
    drive(&mut fractional, 0, 2, 1001, vec![]);
    let fractional_saved = fractional.snapshot().unwrap();
    let mut fractional_restored = branch(&fractional_saved);
    for candidate in [&mut fractional, &mut fractional_restored] {
        let before = registers(candidate);
        drive(candidate, 10_000, 10_000, 1_000_000, vec![]);
        assert_eq!(registers(candidate), before);
        drive(candidate, 10_000, 10_001, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[32], 0x8000_0000);
        drive(candidate, 10_000, 11_000, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[10], 1);
        drive(candidate, 10_000, 11_001, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[10], 2);
    }
    // Reboot discards old-boot paid work and takes the supplied configuration.
    let mut reboot = branch(&partial);
    drive(&mut reboot, 500, 500, 1001, vec![Request::Reboot]);
    drive(&mut reboot, 500, 501, 1001, vec![]);
    assert_eq!(registers(&mut reboot)[10], 0);
    drive(&mut reboot, 500, 502, 1001, vec![]);
    assert_eq!(registers(&mut reboot)[10], 1);
}

#[test]
fn execution_cost_prices_mmio_but_not_wfi_timer_deadlines() {
    // CLINT compare 100000 ticks; only the store instruction's time changes.
    let code = [
        0x0200_42b7, // lui t0, 0x2004: CLINT mtimecmp address.
        0x0001_8337, // lui t1, 0x18: upper bits of 100000 ticks.
        0x6a03_0313, // addi t1, t1, 0x6a0: complete 100000 ticks.
        0x0062_b023, // sd t1, 0(t0): schedule the machine timer.
        0x0800_0313, // addi t1, zero, 128: MTIE bit.
        0x3043_2073, // csrs mie, t1: permit timer wakeup; global MIE stays clear.
        0x1050_0073, // wfi: stop instruction work until the timer becomes pending.
        0x0015_0513, // addi a0, a0, 1: observe the first post-wakeup instruction.
        0x0000_006f, // jal zero, 0: spin after the observed increment.
    ];
    for factor in [1000, 1001, 1_000_000] {
        let mut machine = start(&code);
        let store = (4 * u64::from(factor)).div_ceil(1000);
        let blocked = (7 * u64::from(factor)).div_ceil(1000);
        let stop = drive(&mut machine, 0, 20_000, factor, vec![]);
        let StopReason::Blocked {
            reached,
            deadline: Some(deadline),
        } = stop
        else {
            panic!("{stop:?}")
        };
        assert_eq!(reached, at(blocked));
        // 10MHz mtime truncates the observed MMIO time to whole ticks; QEMU
        // schedules the remaining tick interval from the store's exact timestamp.
        assert_eq!(deadline.as_nanos(), store + (100_000 - store / 100) * 100);
        let saved = machine.snapshot().unwrap();
        let mut restored = branch(&saved);
        for candidate in [&mut machine, &mut restored] {
            drive(
                candidate,
                deadline.as_nanos(),
                deadline.as_nanos(),
                factor,
                vec![],
            );
            assert_eq!(registers(candidate)[10], 0);
            let carry = blocked * 1000 - 7 * u64::from(factor);
            let remaining = u64::from(factor) - carry;
            let completion = deadline.as_nanos() + remaining.div_ceil(1000);
            drive(
                candidate,
                deadline.as_nanos(),
                completion - 1,
                factor,
                vec![],
            );
            assert_eq!(registers(candidate)[10], 0);
            drive(candidate, deadline.as_nanos(), completion, factor, vec![]);
            assert_eq!(registers(candidate)[10], 1);
        }
    }
}

#[test]
fn execution_cost_timer_inside_partial_charge_matches_segmented_and_restored_cpu() {
    let code = [
        0x0200_42b7, // lui t0, 0x2004: CLINT mtimecmp address.
        0x0650_0313, // addi t1, zero, 101: timer compare ticks.
        0x0062_b023, // sd t1, 0(t0): schedule a timer while the CPU stays runnable.
        0x3440_25f3, // csrr a1, mip: observe pending interrupts.
        0x0015_0513, // addi a0, a0, 1: count loop iterations.
        0xff9f_f06f, // jal zero, -8: read mip and count again.
    ];
    for factor in [1000, 1001, 1_000_000] {
        let store = (3 * u64::from(factor)).div_ceil(1000);
        let deadline = store + (101 - store / 100) * 100;
        let end = deadline + 10 * u64::from(factor).div_ceil(1000);
        let mut whole = start(&code);
        drive(&mut whole, 0, end, factor, vec![]);
        assert_ne!(registers(&mut whole)[11] & 128, 0);
        let mut split = start(&code);
        drive(&mut split, 0, deadline - 1, factor, vec![]);
        assert_eq!(registers(&mut split)[11] & 128, 0);
        drive(&mut split, 0, deadline, factor, vec![]);
        let snapshot = split.snapshot().unwrap();
        let mut restored = branch(&snapshot);
        for candidate in [&mut split, &mut restored] {
            drive(candidate, 0, deadline + 1, factor, vec![]);
            drive(candidate, 0, end, factor, vec![]);
            assert_eq!(registers(candidate), registers(&mut whole));
        }
    }
}

/// UART observation is the store's canonical completion, independent of polling.
#[test]
fn console_observations_keep_instruction_times_across_rates_partitions_and_snapshot() {
    use crate::pv::transport::{GuestWrite, LifecycleWrite};
    let code = [
        0x1000_02b7, // lui t0,0x10000: reference UART at 0x10000000.
        0x0410_0313, // addi t1,zero,'A'.
        0x0062_8023, // sb t1,0(t0): emit A on instruction 3.
        0x0420_0313, // addi t1,zero,'B'.
        0x0062_8023, // sb t1,0(t0): emit B on instruction 5.
        0x0062_c383, // lbu t2,6(t0): MSR reads must not recapture stale TSR.
        0x0062_c383, // lbu t2,6(t0): repeated status observation emits nothing.
        0x0100_0313, // addi t1,zero,16: UART_MCR_LOOP.
        0x0062_8223, // sb t1,4(t0): enable internal UART loopback.
        0x0430_0313, // addi t1,zero,'C'.
        0x0062_8023, // sb t1,0(t0): loopback must not emit console C.
        0x0062_c383, // lbu t2,6(t0): loopback modem-status read.
        0x0000_006f, // jal zero,0: no further output.
    ];
    for factor in [1000, 1001, 1_000_000] {
        let machine = start(&code);
        let manifest = machine.manifest.clone();
        let qemu = Path::new("qemu-system-riscv64");
        let end = u64::from(factor) * 6;
        let mut whole = Process::start(qemu, &manifest, None, 7).unwrap();
        whole
            .execute(at(0), at(end), ExecutionConfig::new(factor).unwrap())
            .unwrap();
        let snapshot = whole.snapshot().unwrap();
        let expected: Vec<_> = [(3u64, b'A'), (5, b'B')]
            .into_iter()
            .map(|(instruction, byte)| {
                GuestWrite::Console(LifecycleWrite {
                    time: (instruction * u64::from(factor)).div_ceil(1000),
                    bytes: vec![byte],
                })
            })
            .collect();
        assert_eq!(whole.drain_guest().unwrap(), expected);
        assert_eq!(whole.console().unwrap(), b"AB");
        let mut restored = Process::start(qemu, &manifest, Some(&snapshot), 7).unwrap();
        assert_eq!(restored.drain_guest().unwrap(), expected);
        assert!(restored.drain_guest().unwrap().is_empty());
        let mut split = Process::start(qemu, &manifest, None, 7).unwrap();
        let mut observed = vec![];
        let mut previous = 0;
        for boundary in [1, 2, 3, 4, 5, 6, 1000, end] {
            if boundary > end || boundary <= previous {
                continue;
            }
            split
                .execute(
                    at(previous),
                    at(boundary),
                    ExecutionConfig::new(factor).unwrap(),
                )
                .unwrap();
            observed.extend(split.drain_guest().unwrap());
            previous = boundary;
        }
        assert_eq!(observed, expected, "factor={factor}");
        // Exercise the adapter too: public outputs retain the device times,
        // rather than the later stopped horizon used to drain the transport.
        let mut machine = start(&code);
        let clock = SimulationClock::new(0);
        let mut rng = SeededRng::new(7);
        let mut inbox = VecDeque::new();
        let mut outputs = vec![];
        machine
            .run(
                &mut RunContext {
                    worker: &mut (),
                    clock: &clock,
                    rng: &mut rng,
                    execution: ExecutionConfig::new(factor).unwrap(),
                    inbox: Inbox::new(&mut inbox, at(0)),
                    outbox: Outbox::new(&mut outputs, at(0), at(end)),
                },
                at(end),
            )
            .unwrap();
        let console: Vec<_> = outputs
            .into_iter()
            .filter_map(|output| match output.payload {
                crate::pv::Response::BootConsole { bytes } => {
                    Some(GuestWrite::Console(LifecycleWrite {
                        time: output.at.as_nanos(),
                        bytes,
                    }))
                }
                _ => None,
            })
            .collect();
        assert_eq!(console, expected);
    }
}
