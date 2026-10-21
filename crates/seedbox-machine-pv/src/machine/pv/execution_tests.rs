//! Native ABI3 work fixture: the same load-once bounded charge loop as the PV
//! kernel. This is not a claim to price arbitrary uninstrumented instructions.
use super::*;
use crate::{
    clock::{Clock, SimulationClock},
    rng::SeededRng,
};
use seedbox_machine::execution::ExecutionConfig;
use std::collections::VecDeque;
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn input(name: &str) -> PathBuf {
    std::env::var_os(name)
        .unwrap_or_else(|| panic!("set {name}"))
        .into()
}
/// Charge two syscall-priced work units through bounded clock reads, writing
/// one console byte after each, then arm a timer 5000 ns after the last read,
/// idle, and power off.
fn image() -> Vec<u8> {
    #[cfg(target_arch = "aarch64")]
    let source = r#".text
.global _start
_start:
 movz x20, #0xa00, lsl #16
 movz x21, #0xa01, lsl #16
 movz x22, #0x900, lsl #16
 mov x24, #0
 mov x3, #3
 str x3, [x21, #32]
unit:
 ldr x19, [x20, #48]
charge:
 ldr x0, [x20]
 ldr x1, [x20, #8]
 cmp x0, x1
 b.hi candidate
 add x0, x1, #1
candidate:
 ldr x2, [x20, #16]
 cmp x0, x2
 b.ls charged
 str xzr, [x21, #24]
 b charge
charged:
 str x0, [x20, #8]
 subs x19, x19, #1
 b.ne charge
 add x24, x24, #1
 str w24, [x22]
 cmp x24, #2
 b.lt unit
 ldr x0, [x20, #8]
 add x0, x0, #4096
 add x0, x0, #904
 str x0, [x21]
 str xzr, [x21, #16]
 movz x0, #8
 movk x0, #0x8400, lsl #16
 hvc #0
"#;
    #[cfg(target_arch = "x86_64")]
    let source = r#".text
.code64
.global _start
_start:
 mov $0x0a000000, %r12d
 mov $0x0a010000, %r13d
 xor %r14d, %r14d
 movq $3, 32(%r13)
unit:
 mov 48(%r12), %rbx
charge:
 mov (%r12), %rax
 mov 8(%r12), %rcx
 cmp %rcx, %rax
 ja candidate
 lea 1(%rcx), %rax
candidate:
 mov 16(%r12), %rsi
 cmp %rsi, %rax
 jbe charged
 movq $0, 24(%r13)
 jmp charge
charged:
 mov %rax, 8(%r12)
 dec %rbx
 jnz charge
 inc %r14
 mov %r14d, %eax
 mov $0x3f8, %dx
 out %al, %dx
 cmp $2, %r14
 jb unit
 mov 8(%r12), %rax
 add $5000, %rax
 mov %rax, (%r13)
 movq $0, 16(%r13)
 movq $1, 40(%r13)
"#;
    super::test_image::assemble(source)
}
fn start(kernel: Vec<u8>) -> PVMachine {
    PVMachine::from_boot_inputs(&input("SEEDBOX_PV_WORKER"), kernel, b"initrd".to_vec()).unwrap()
}
fn run(
    machine: &mut PVMachine,
    clock: &SimulationClock,
    rng: &mut SeededRng,
    factor: u32,
    upto: u64,
    requests: impl IntoIterator<Item = seedbox_machine::pv::Request>,
) -> StopReason {
    let mut inbox: VecDeque<_> = requests
        .into_iter()
        .map(|payload| seedbox_machine::pv::Input {
            at: at(clock.now()),
            payload,
        })
        .collect();
    let mut outputs = Vec::new();
    let stopped = machine
        .run(
            &mut RunContext {
                worker: &mut (),
                clock,
                rng,
                execution: ExecutionConfig::new(factor).unwrap(),
                inbox: seedbox_machine::pv::Inbox::new(&mut inbox, at(clock.now())),
                outbox: seedbox_machine::pv::Outbox::new(&mut outputs, at(clock.now()), at(upto)),
            },
            at(upto),
        )
        .unwrap();
    assert!(inbox.is_empty());
    stopped
}
#[test]
#[ignore = "requires a native PV worker and fixture clang/linker"]
fn native_execution_price_partial_charge_restore_gap_and_sleep() {
    let kernel = image();
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(7);
    let mut machine = start(kernel);
    assert_eq!(
        run(&mut machine, &clock, &mut rng, 2000, 500, []),
        StopReason::BoundaryReached
    );
    assert!(machine.console.is_empty());
    clock.set(500);
    assert_eq!(
        run(&mut machine, &clock, &mut rng, 1001, 500, []),
        StopReason::BoundaryReached
    );
    let snapshot = machine.snapshot().unwrap();
    let saved_rng = rng.clone();
    assert_eq!(
        run(&mut machine, &clock, &mut rng, 1001, 2000, []),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.console, [1]);
    assert_eq!(
        run(&mut machine, &clock, &mut rng, 1001, 3001, []),
        StopReason::BoundaryReached
    );
    assert_eq!(machine.console, [1, 2]);
    let StopReason::Blocked {
        deadline: Some(deadline),
        ..
    } = run(&mut machine, &clock, &mut rng, 1_000_000, 3002, [])
    else {
        panic!("fixture did not sleep")
    };
    assert_eq!(deadline, at(8001));
    clock.set(8001);
    assert_eq!(
        run(&mut machine, &clock, &mut rng, 1_000_000, 8001, []),
        StopReason::BoundaryReached
    );
    assert!(
        matches!(run(&mut machine,&clock,&mut rng,1_000_000,8002,[]),StopReason::Exited {reached,..} if reached==at(8001))
    );
    drop(machine);
    let mut branch = PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), &snapshot).unwrap();
    let branch_clock = SimulationClock::new(500);
    let mut branch_rng = saved_rng.clone();
    run(&mut branch, &branch_clock, &mut branch_rng, 3000, 2000, []);
    assert_eq!(branch.console, [1]);
    assert_eq!(
        run(&mut branch, &branch_clock, &mut branch_rng, 3000, 4999, []),
        StopReason::BoundaryReached
    );
    assert_eq!(branch.console, [1]);
    assert_eq!(
        run(&mut branch, &branch_clock, &mut branch_rng, 3000, 5000, []),
        StopReason::BoundaryReached
    );
    assert_eq!(branch.reached, 5000);
    assert_eq!(branch.console, [1, 2]);
    // A paused gap adopts canonical time without consuming the saved remaining
    // charge. A resumed progress read must pay from after the adopted point.
    let mut gap = PVMachine::from_snapshot(&input("SEEDBOX_PV_WORKER"), &snapshot).unwrap();
    let gap_clock = SimulationClock::new(10_000);
    let mut gap_rng = saved_rng;
    run(&mut gap, &gap_clock, &mut gap_rng, 3000, 10_000, []);
    assert!(gap.console.is_empty());
    run(&mut gap, &gap_clock, &mut gap_rng, 3000, 11_499, []);
    assert!(gap.console.is_empty());
    assert_eq!(
        run(&mut gap, &gap_clock, &mut gap_rng, 3000, 11_500, []),
        StopReason::BoundaryReached
    );
    assert_eq!(gap.console, [1]);
    run(&mut gap, &gap_clock, &mut gap_rng, 3000, 14_499, []);
    assert_eq!(gap.console, [1]);
    assert_eq!(
        run(&mut gap, &gap_clock, &mut gap_rng, 3000, 14_500, []),
        StopReason::BoundaryReached
    );
    assert_eq!(gap.console, [1, 2]);
}
#[test]
#[ignore = "requires a native PV worker and fixture clang/linker"]
fn native_execution_price_exact_subpercent_split_bounds_and_reboot() {
    let kernel = image();
    for factor in [1000, 1001, 1_000_000] {
        let clock = SimulationClock::new(0);
        let mut rng = SeededRng::new(7);
        let mut machine = start(kernel.clone());
        let total = u64::from(factor) * 2;
        run(&mut machine, &clock, &mut rng, factor, total, []);
        assert_eq!(machine.console, [1, 2]);
        assert_eq!(machine.reached, total);
        clock.set(total);
        run(
            &mut machine,
            &clock,
            &mut rng,
            factor,
            total,
            [seedbox_machine::pv::Request::Reboot],
        );
        // Console bytes already observed remain external history across reboot.
        assert_eq!(machine.console, [1, 2]);
        assert_eq!(
            run(&mut machine, &clock, &mut rng, factor, total * 2, []),
            StopReason::BoundaryReached
        );
        assert_eq!(machine.reached, total * 2);
        assert_eq!(machine.console, [1, 2, 1, 2]);
    }
    let clock = SimulationClock::new(0);
    let mut rng = SeededRng::new(7);
    let mut split = start(kernel);
    for bound in [1, 2, 17, 499, 500, 999, 1001, 1500, 2002] {
        run(&mut split, &clock, &mut rng, 1001, bound, []);
    }
    assert_eq!(split.console, [1, 2]);
    assert_eq!(split.reached, 2002);
}

use super::guest as linux;
#[test]
#[ignore = "requires a native PV worker, current PV Linux kernel and fixture clang/linker"]
fn native_linux_syscall_work_observes_subpercent_execution_price() {
    let initrd = linux::initrd_source(
        r#"
/* Freestanding Linux PID1: measure syscall progress, then power off. */
#include "pv-syscall.h"
enum {
    SYS_WRITE = 64, SYS_PPOLL = 73, SYS_CLOCK_GETTIME = 113,
    SYS_REBOOT = 142, SYS_GETPID = 172, CLOCK_MONOTONIC = 1,
    LINUX_REBOOT_MAGIC1 = 0xfee1dead,
    LINUX_REBOOT_MAGIC2 = 0x28121969,
    LINUX_REBOOT_CMD_POWER_OFF = 0x4321fedc,
};
__attribute__((noreturn)) void _start(void)
{
    long before[2], after[2];
    call(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC, (long)before, 0, 0, 0, 0);
    for (unsigned i = 0; i < 1000; ++i) {
        call(SYS_GETPID, 0, 0, 0, 0, 0, 0);
    }
    call(SYS_CLOCK_GETTIME, CLOCK_MONOTONIC, (long)after, 0, 0, 0, 0);
    unsigned long delta = (after[0] - before[0]) * 1000000000UL
                        + after[1] - before[1];
    call(SYS_WRITE, 1, (long)"PV_RATE:", 8, 0, 0, 0);
    call(SYS_WRITE, 1, (long)&delta, sizeof(delta), 0, 0, 0);
    call(SYS_REBOOT, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2,
         LINUX_REBOOT_CMD_POWER_OFF, 0, 0, 0);
    for (;;) {
        call(SYS_PPOLL, 0, 0, 0, 0, 8, 0);
    }
}
"#
        .to_owned(),
    );
    let mut deltas = Vec::new();
    for factor in [1000, 1001] {
        let mut measurements = Vec::new();
        // The narrow bound repeatedly splits actual Linux syscall charges;
        // the prime interval also moves the split through successive charges.
        for quantum in [1_000_000, 997] {
            let mut machine = PVMachine::from_boot_inputs(
                &input("SEEDBOX_PV_WORKER"),
                fs::read(input("SEEDBOX_PV_KERNEL")).unwrap(),
                initrd.clone(),
            )
            .unwrap();
            let clock = SimulationClock::new(0);
            let mut rng = SeededRng::new(7);
            let guard = seedbox_test_time::HangGuard::new(
                "native CPU rate measurement must return",
                std::time::Duration::from_secs(60),
            );
            for tick in 1..=10_000_000_000 / quantum {
                guard
                    .remaining()
                    .expect("native CPU rate infrastructure hang");
                if matches!(
                    run(&mut machine, &clock, &mut rng, factor, tick * quantum, []),
                    StopReason::Exited { .. }
                ) {
                    break;
                }
                clock.set(tick * quantum);
            }
            let offset = machine
                .console
                .windows(8)
                .position(|bytes| bytes == b"PV_RATE:")
                .unwrap_or_else(|| {
                    panic!(
                        "no work measurement: {}",
                        String::from_utf8_lossy(&machine.console)
                    )
                })
                + 8;
            measurements.push(u64::from_le_bytes(
                machine.console[offset..offset + 8].try_into().unwrap(),
            ));
        }
        assert_eq!(
            measurements[0], measurements[1],
            "price {factor}: splitting syscall charges changed measured guest time"
        );
        deltas.push(measurements[0]);
    }
    // This measured Linux region contains 1000 getpid calls and the concluding
    // clock syscall, plus unscaled kernel clock reads. It proves a sub-percent
    // slowdown survives pricing, not exact pricing of every native instruction.
    assert!(deltas[1] > deltas[0], "{deltas:?}");
    assert!(
        (1000..=2000).contains(&(deltas[1] - deltas[0])),
        "{deltas:?}"
    );
}
