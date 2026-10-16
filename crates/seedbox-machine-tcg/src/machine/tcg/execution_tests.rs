//! Native reference-backend tests: inspect CPU state independently through GDB.
use super::raw_program_data::{counting_loop, lapic, Asm, Reg, PROGRAM, RBX, RCX, RIP, STUB};
use super::*;
use crate::{
    clock::SimulationClock,
    execution::ExecutionConfig,
    pv::{Inbox, Input, Outbox, Request},
    rng::SeededRng,
};
use std::collections::VecDeque;

const QEMU: &str = "qemu-system-x86_64";
/// EIP before the loop's `inc ebx` (one byte), and before its `jmp`.
const INCREMENT: u64 = PROGRAM;
const JUMP: u64 = PROGRAM + 1;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn start(image: Vec<u8>) -> TcgMachine {
    TcgMachine::new_raw(
        Path::new(QEMU),
        Manifest {
            boot: Boot::Program(image.into()),
            memory_mib: 16,
            disk: None,
            network: None,
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
fn registers(machine: &mut TcgMachine) -> [u64; 17] {
    machine.process().unwrap().registers().unwrap()
}
fn branch(snapshot: &Snapshot) -> TcgMachine {
    TcgMachine::from_snapshot(Path::new(QEMU), snapshot).unwrap()
}
/// The counting loop, stopped exactly after the stub at factor 1000, so no
/// partial charge is pending. Later times are offset by [`STUB`].
fn entered() -> TcgMachine {
    let mut machine = start(counting_loop());
    assert_eq!(
        drive(&mut machine, 0, STUB, 1000, vec![]),
        StopReason::BoundaryReached
    );
    assert_eq!(registers(&mut machine)[RIP], INCREMENT);
    machine
}
/// EBX after `retired` instructions of the counting loop, stub included.
fn counted(retired: u64) -> u64 {
    retired.saturating_sub(STUB).div_ceil(2)
}

#[test]
fn inspector_attachment_completes_before_first_bounded_run() {
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..32)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..4 {
                        let mut machine = start(counting_loop());
                        assert_eq!(
                            drive(&mut machine, 0, 100_000, 1000, vec![]),
                            StopReason::BoundaryReached
                        );
                        assert_eq!(registers(&mut machine)[RBX], counted(100_000));
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    });
}

#[test]
fn execution_cost_fractional_and_maximum_rates_match_segmented_cpu_work() {
    for factor in [1000, 1001, 1_000_000] {
        let mut whole = start(counting_loop());
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
                registers(&mut split)[RBX],
                counted(retired),
                "factor={factor}, bound={bound}"
            );
        }
        assert_eq!(registers(&mut split), registers(&mut whole));
    }
}

#[test]
fn execution_cost_snapshot_and_rate_change_keep_old_partial_charge() {
    let s = STUB;
    let mut machine = entered();
    drive(&mut machine, 0, s + 1, 1001, vec![]);
    assert_eq!(registers(&mut machine)[RBX], 0);
    let first = machine.snapshot().unwrap();
    let mut restored = branch(&first);
    for candidate in [&mut machine, &mut restored] {
        // The first instruction still owes .001 ns at the old rate.
        drive(candidate, 0, s + 2, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RBX], 1);
        drive(candidate, 0, s + 1001, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RIP], JUMP);
        drive(candidate, 0, s + 1002, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RIP], INCREMENT);
    }
    assert_eq!(registers(&mut machine), registers(&mut restored));
    // A changed rate at a nonintegral completed instruction also retains the
    // charge already started with the fractional remainder of that completion.
    let mut machine = branch(&first);
    drive(&mut machine, 0, s + 2, 1001, vec![]);
    let partial_next = machine.snapshot().unwrap();
    let mut restored = branch(&partial_next);
    for candidate in [&mut machine, &mut restored] {
        drive(candidate, 0, s + 3, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RIP], INCREMENT);
        drive(candidate, 0, s + 1002, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RBX], 1);
        drive(candidate, 0, s + 1003, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RBX], 2);
    }
}

#[test]
fn execution_cost_adoption_snapshot_and_reboot_preserve_external_authority() {
    let s = STUB;
    let mut machine = entered();
    drive(&mut machine, 0, s + 500, 1_000_000, vec![]);
    let partial = machine.snapshot().unwrap();
    let mut restored = branch(&partial);
    let adopted = s + 10_000;
    for candidate in [&mut machine, &mut restored] {
        let before = registers(candidate);
        drive(candidate, adopted, adopted, 1001, vec![]);
        assert_eq!(registers(candidate), before);
        drive(candidate, adopted, adopted + 499, 1001, vec![]);
        assert_eq!(registers(candidate)[RBX], 0);
        drive(candidate, adopted, adopted + 500, 1001, vec![]);
        assert_eq!(registers(candidate)[RBX], 1);
        drive(candidate, adopted, adopted + 501, 1001, vec![]);
        assert_eq!(registers(candidate)[RIP], JUMP);
        drive(candidate, adopted, adopted + 502, 1001, vec![]);
        assert_eq!(registers(candidate)[RIP], INCREMENT);
    }
    // Preserve sub-nanosecond carry as well as a large integer partial charge.
    let mut fractional = entered();
    drive(&mut fractional, 0, s + 2, 1001, vec![]);
    let fractional_saved = fractional.snapshot().unwrap();
    let mut fractional_restored = branch(&fractional_saved);
    for candidate in [&mut fractional, &mut fractional_restored] {
        let before = registers(candidate);
        drive(candidate, adopted, adopted, 1_000_000, vec![]);
        assert_eq!(registers(candidate), before);
        drive(candidate, adopted, adopted + 1, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RIP], INCREMENT);
        drive(candidate, adopted, adopted + 1000, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RBX], 1);
        drive(candidate, adopted, adopted + 1001, 1_000_000, vec![]);
        assert_eq!(registers(candidate)[RBX], 2);
    }
    // Reboot discards old-boot paid work and takes the supplied configuration:
    // the new boot's stub and first increment are priced at 1001.
    let mut reboot = branch(&partial);
    let rebooted = s + 500;
    drive(&mut reboot, rebooted, rebooted, 1001, vec![Request::Reboot]);
    let increment = rebooted + ((s + 1) * 1001).div_ceil(1000);
    drive(&mut reboot, rebooted, increment - 1, 1001, vec![]);
    assert_eq!(registers(&mut reboot)[RBX], 0);
    drive(&mut reboot, rebooted, increment, 1001, vec![]);
    assert_eq!(registers(&mut reboot)[RBX], 1);
}

/// LAPIC timer count of the pricing tests.
fn timer(count: u32) -> Asm {
    let mut code = Asm::default();
    code.lapic_timer(count);
    code
}

#[test]
fn execution_cost_prices_mmio_but_not_hlt_timer_deadlines() {
    // The count store is program instruction 4; HLT is instruction 6. After
    // the wakeup, the EOI handler (two instructions) precedes the increment.
    let count = 100_000;
    let image = timer(count).sti().hlt().inc(Reg::Ebx).spin().image();
    for factor in [1000, 1001, 1_000_000] {
        let mut machine = start(image.clone());
        let store = ((STUB + 4) * u64::from(factor)).div_ceil(1000);
        let blocked = ((STUB + 6) * u64::from(factor)).div_ceil(1000);
        let stop = drive(&mut machine, 0, 20_000, factor, vec![]);
        let StopReason::Blocked {
            reached,
            deadline: Some(deadline),
        } = stop
        else {
            panic!("{stop:?}")
        };
        assert_eq!(reached, at(blocked));
        // The one-shot LAPIC timer (divide by one) loads at the store's exact
        // completion time and expires count + 1 nanoseconds later.
        assert_eq!(deadline.as_nanos(), store + u64::from(count) + 1);
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
            assert_eq!(registers(candidate)[RBX], 0);
            let carry = blocked * 1000 - (STUB + 6) * u64::from(factor);
            let remaining = 3 * u64::from(factor) - carry;
            let completion = deadline.as_nanos() + remaining.div_ceil(1000);
            drive(
                candidate,
                deadline.as_nanos(),
                completion - 1,
                factor,
                vec![],
            );
            assert_eq!(registers(candidate)[RBX], 0);
            drive(candidate, deadline.as_nanos(), completion, factor, vec![]);
            assert_eq!(registers(candidate)[RBX], 1);
        }
    }
}

#[test]
fn execution_cost_timer_inside_partial_charge_matches_segmented_and_restored_cpu() {
    // Interrupts stay disabled: the loop reads the LAPIC IRR into ECX to
    // observe the pending timer vector (0x20, bit 0 of IRR word 1).
    let count = 100;
    let image = timer(count)
        .label("loop")
        .load(Reg::Ecx, lapic::IRR_1)
        .inc(Reg::Ebx)
        .jmp("loop")
        .image();
    for factor in [1000, 1001, 1_000_000] {
        let store = ((STUB + 4) * u64::from(factor)).div_ceil(1000);
        let deadline = store + u64::from(count) + 1;
        let end = deadline + 10 * u64::from(factor).div_ceil(1000);
        let mut whole = start(image.clone());
        drive(&mut whole, 0, end, factor, vec![]);
        assert_ne!(registers(&mut whole)[RCX] & 1, 0);
        let mut split = start(image.clone());
        drive(&mut split, 0, deadline - 1, factor, vec![]);
        assert_eq!(registers(&mut split)[RCX] & 1, 0);
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
    // COM1 at port 0x3F8: THR +0, MCR +4, MSR +6.
    let code = Asm::default()
        .mov_imm(Reg::Edx, 0x3F8)
        .mov_al(b'A')
        .out_dx() // Emit A on program instruction 3.
        .mov_al(b'B')
        .out_dx() // Emit B on program instruction 5.
        .mov_imm(Reg::Edx, 0x3FE)
        .in_dx() // MSR reads must not recapture stale TSR.
        .in_dx() // Repeated status observation emits nothing.
        .mov_imm(Reg::Edx, 0x3FC)
        .mov_al(0x10) // UART_MCR_LOOP.
        .out_dx() // Enable internal UART loopback.
        .mov_imm(Reg::Edx, 0x3F8)
        .mov_al(b'C')
        .out_dx() // Loopback must not emit console C.
        .mov_imm(Reg::Edx, 0x3FE)
        .in_dx() // Loopback modem-status read.
        .spin() // No further output.
        .image();
    for factor in [1000, 1001, 1_000_000] {
        let machine = start(code.clone());
        let manifest = machine.manifest.clone();
        let qemu = Path::new(QEMU);
        let end = u64::from(factor) * 6;
        let mut whole = Process::start(qemu, &manifest, None, 7, None).unwrap();
        whole
            .execute(at(0), at(end), ExecutionConfig::new(factor).unwrap())
            .unwrap();
        let snapshot = whole.snapshot().unwrap();
        let expected: Vec<_> = [(3u64, b'A'), (5, b'B')]
            .into_iter()
            .map(|(instruction, byte)| {
                GuestWrite::Console(LifecycleWrite {
                    time: ((STUB + instruction) * u64::from(factor)).div_ceil(1000),
                    bytes: vec![byte],
                })
            })
            .collect();
        assert_eq!(whole.drain_guest().unwrap(), expected);
        assert_eq!(whole.console().unwrap(), b"AB");
        let mut restored = Process::start(qemu, &manifest, Some(&snapshot), 7, None).unwrap();
        assert_eq!(restored.drain_guest().unwrap(), expected);
        assert!(restored.drain_guest().unwrap().is_empty());
        let mut split = Process::start(qemu, &manifest, None, 7, None).unwrap();
        let mut observed = vec![];
        let mut previous = 0;
        // Every instruction boundary through the second byte's successor.
        for boundary in (1..=STUB + 6).chain([1000, end]) {
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
        let mut machine = start(code.clone());
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
