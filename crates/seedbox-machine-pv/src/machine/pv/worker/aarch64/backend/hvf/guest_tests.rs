//! Real-HVF guests at EL1 for host interruptions and checkpointed interrupts.
//!
//! The guest is assembled into this test binary (never executed on the host)
//! and copied into an Image after a 4 KiB header. It turns the MMU on (RAM
//! normal write-back, so exclusives behave as in Linux), installs vectors, and
//! runs one mode chosen by a word in its image:
//!
//! * `FUTEX`: futex-style read-modify-write and PI-lock compare-and-swap loops
//!   on LDXR/STLXR that retry a failed store without counting it, as the
//!   patched kernel's `futex_atomic_op_inuser` does.
//! * `PENDING`, `ACTIVE`, `MASKED`: a periodic PV timer whose handler counts
//!   (x19) and rearms it, and a PL011 device interrupt raised on every fourth
//!   tick (x20). `ACTIVE` idles inside the handler until the next deadline
//!   before its EOI; `MASKED` masks IRQs across each deadline and unmasks
//!   only once the machine publishes the deferred interrupt, as the PV kernel
//!   does at its next rendezvous.
//!
//! HVF allows one VM per process: run these with `--test-threads=1` in a test
//! executable signed with the hypervisor entitlement.
use super::*;
use crate::machine::pv::{
    policy::Named,
    protocol::{self, Stop},
    worker::{read_snapshot, restore_machine, BootInputs, Machine},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[link(name = "Hypervisor", kind = "framework")]
unsafe extern "C" {
    /// Force the listed vCPUs out of guest execution: a host interruption.
    fn hv_vcpus_exit(cpus: *const u64, count: u32) -> i32;
}

const FUTEX: u32 = 0;
const PENDING: u32 = 1;
const ACTIVE: u32 = 2;
const MASKED: u32 = 3;
/// Timer period of the interrupt modes, in guest ns.
const PERIOD: u64 = 1_000;
const FUTEX_ITERATIONS: u64 = 1 << 20;
const FUTEX_TID: u64 = 0x1234;
const TIMER: u32 = 40;
const MEMORY: u64 = 16 << 20;
const INITRD: &[u8] = b"initrd";

std::arch::global_asm!(
    r#"
    .text
    .p2align 12
    .globl _sbguest_start
_sbguest_start:
    b Lsb_code
    .p2align 3
    // Layout, read by the test: a single global symbol keeps the blob one
    // linker atom, so these offsets are fixed at assembly time.
    .quad Lsb_end - _sbguest_start
    .quad Lsb_mode - _sbguest_start
    .quad Lsb_words - _sbguest_start
    .quad Lsb_irq - _sbguest_start
Lsb_code:
    adr x1, Lsb_mode
    ldr w21, [x1]
    movz x23, #0x0a01, lsl #16          // PV event device
    movz x24, #0x0a00, lsl #16          // PV time page
    mov x25, #{period}
    mov x19, #0
    mov x20, #0
    mov x22, #0
    mov x0, #{abi}
    str x0, [x23, #32]                  // PV handshake
    adr x1, Lsb_vectors
    msr vbar_el1, x1
    // Identity map: 0-1 GiB device, 1-2 GiB normal write-back inner shareable.
    adr x1, Lsb_l1
    mov x2, #0x401
    str x2, [x1]
    mov x2, #0x40000000
    mov x3, #0x705
    orr x2, x2, x3
    str x2, [x1, #8]
    dc civac, x1
    dsb sy
    mov x2, #0xff00
    msr mair_el1, x2
    mov x2, #25
    orr x2, x2, #0x100
    orr x2, x2, #0x400
    orr x2, x2, #0x3000
    orr x2, x2, #0x800000
    orr x2, x2, #0x100000000
    msr tcr_el1, x2
    msr ttbr0_el1, x1
    isb
    tlbi vmalle1
    dsb sy
    isb
    mrs x2, sctlr_el1
    orr x2, x2, #1
    orr x2, x2, #4
    orr x2, x2, #0x1000
    msr sctlr_el1, x2
    isb
    cbz w21, 50f
    // GICv3: wake the redistributor, enable group 1 and SPIs 33 and 40.
    movz x1, #0x0800, lsl #16
    movz x2, #0x080a, lsl #16
    ldr w3, [x2, #0x14]
    bic w3, w3, #2
    str w3, [x2, #0x14]
1:  ldr w3, [x2, #0x14]
    tbnz w3, #2, 1b
    mov w3, #0x13
    str w3, [x1]
2:  ldr w3, [x1]
    tbnz w3, #31, 2b
    mov w3, #0x102
    str w3, [x1, #0x84]                 // IGROUPR1: SPIs 33 and 40
    mov w3, #0xa0
    strb w3, [x1, #0x421]
    strb w3, [x1, #0x428]
    add x4, x1, #0x6000
    str xzr, [x4, #0x108]
    str xzr, [x4, #0x140]
    mov w3, #0x102
    str w3, [x1, #0x104]                // ISENABLER1
    mrs x3, icc_sre_el1
    orr x3, x3, #1
    msr icc_sre_el1, x3
    isb
    mov x3, #0xff
    msr icc_pmr_el1, x3
    msr icc_bpr1_el1, xzr
    mov x3, #1
    msr icc_igrpen1_el1, x3
    isb
    cmp w21, #{masked}
    b.eq 40f
    // PENDING and ACTIVE: arm, unmask and idle; raise the UART every fourth tick.
    ldr x10, [x24]
    add x10, x10, x25
    str x10, [x23]
    msr daifclr, #2
10: str x0, [x23, #16]
    cmp x19, x22
    b.eq 10b
    mov x22, x19
    tst x19, #3
    b.ne 10b
    bl 70f
    b 10b
    // MASKED: arm and yield with IRQs masked until the machine defers the
    // timer, then unmask at a rendezvous to take the deferred SPI.
40: msr daifclr, #2
41: msr daifset, #2
    ldr x10, [x24]
    add x11, x10, x25
    str x11, [x23]
42: str x0, [x23, #24]
    ldr x10, [x24, #32]                 // published deferred interrupt
    cbz x10, 42b
    msr daifclr, #2
    str x0, [x23, #24]
    tst x19, #3
    b.ne 41b
    cmp x19, x22
    b.eq 41b
    mov x22, x19
    bl 70f
    b 41b
    // FUTEX: futex-op and PI-lock exclusive loops; store failures just retry.
50: adr x1, Lsb_words
    add x8, x1, #4
    add x15, x1, #8
    mov x26, #0
    mov x6, #{tid}
    mov x16, #0
51: add x26, x26, #1
    orr x2, x26, #1
52: ldxr w3, [x1]                       // FUTEX_OP_ADD
    add w3, w3, w2
    nop
    nop
    stlxr w4, w3, [x1]
    cbnz w4, 62f
    mov x2, #0x9e37
    mul x2, x2, x26
53: ldxr w3, [x8]                       // FUTEX_OP_XOR
    eor w3, w3, w2
    nop
    stlxr w4, w3, [x8]
    cbnz w4, 63f
54: ldaxr w3, [x15]                     // PI lock: 0 -> TID
    cbnz w3, 99f
    stlxr w4, w6, [x15]
    cbnz w4, 64f
55: ldxr w3, [x15]                      // FUTEX_WAITERS
    orr w3, w3, #0x80000000
    stlxr w4, w3, [x15]
    cbnz w4, 65f
    orr w5, w6, #0x80000000
56: ldaxr w3, [x15]                     // PI unlock: TID|WAITERS -> 0
    cmp w3, w5
    b.ne 99f
    stlxr w4, wzr, [x15]
    cbnz w4, 66f
    tst x26, #0xfff
    b.ne 57f
    str x0, [x23, #24]                  // yield to the run bound
57: mov x2, #{iterations}
    cmp x26, x2
    b.lo 51b
    ldr w27, [x1]
    ldr w28, [x1, #4]
    str w16, [x1, #12]                  // retry count; the test clears it
    mov x16, #0
58: str x0, [x23, #16]
    b 58b
    // Failed stores (host-induced) retry; x16 counts them for diagnostics only.
62: add x16, x16, #1
    b 52b
63: add x16, x16, #1
    b 53b
64: add x16, x16, #1
    b 54b
65: add x16, x16, #1
    b 55b
66: add x16, x16, #1
    b 56b
99: brk #1                              // a lock word the guest never wrote
    // Raise the PL011 transmit interrupt.
70: movz x13, #0x0900, lsl #16
    mov w14, #0x101
    str w14, [x13, #0x30]
    mov w14, #0x20
    str w14, [x13, #0x38]
    ret
    // IRQ handler: count, acknowledge and rearm the timer, or quiet the UART.
Lsb_irq:
80: mrs x9, icc_iar1_el1
    cmp x9, #40
    b.ne 82f
    add x19, x19, #1
    mov x10, #1
    str x10, [x23, #8]
    ldr x10, [x24]
    add x10, x10, x25
    str x10, [x23]
    cmp w21, #{active}
    b.ne 83f
84: str x10, [x23, #16]                 // idle while this interrupt is active,
    ldr x11, [x24]                      // until the next deadline
    cmp x11, x10
    b.lo 84b
    b 83f
82: cmp x9, #33
    b.ne 83f
    add x20, x20, #1
    movz x10, #0x0900, lsl #16
    str wzr, [x10, #0x38]
83: msr icc_eoir1_el1, x9
    eret

    .p2align 11
Lsb_vectors:
    .rept 4
    brk #2
    .p2align 7
    .endr
    brk #3                              // sync, current EL, SPx
    .p2align 7
    b 80b                               // IRQ, current EL, SPx
    .p2align 7
    .rept 10
    brk #4
    .p2align 7
    .endr
    .p2align 12
Lsb_l1:
    .space 4096
Lsb_words:
    .space 64
Lsb_mode:
    .word 0
    .p2align 4
Lsb_end:
"#,
    abi = const crate::machine::pv::worker::ABI,
    period = const PERIOD,
    masked = const MASKED,
    active = const ACTIVE,
    tid = const FUTEX_TID,
    iterations = const FUTEX_ITERATIONS,
);

unsafe extern "C" {
    static sbguest_start: u8;
}

/// Offsets in the assembled blob: its length, then the mode word, the futex
/// words and the IRQ handler.
#[derive(Clone, Copy)]
enum Layout {
    End = 1,
    Mode,
    Words,
    Irq,
}

fn offset(field: Layout) -> usize {
    unsafe {
        let start = &raw const sbguest_start;
        std::ptr::read_unaligned(start.add(8 * field as usize) as *const u64) as usize
    }
}

fn image(mode: u32) -> Vec<u8> {
    let blob = unsafe {
        std::slice::from_raw_parts(&raw const sbguest_start, offset(Layout::End)).to_vec()
    };
    let mut kernel = vec![0u8; 4096];
    kernel[..4].copy_from_slice(&0x1400_0400u32.to_le_bytes()); // B +4096
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel.extend_from_slice(&blob);
    let mode_at = 4096 + offset(Layout::Mode);
    kernel[mode_at..mode_at + 4].copy_from_slice(&mode.to_le_bytes());
    let size = kernel.len() as u64;
    kernel[16..24].copy_from_slice(&size.to_le_bytes());
    kernel
}

fn policy() -> protocol::BootPolicy {
    Named::new(
        "node",
        &seedbox_machine::definition::BackendPolicy::default(),
    )
    .unwrap()
    .setup(0)
}

fn machine(kernel: &[u8]) -> Machine {
    let mut vm =
        Machine::<Native>::boot_with_memory(kernel, INITRD, 1, "", None, None, MEMORY).unwrap();
    // Stopped evidence needs a configured boundary.
    vm.configure_policy(Some(&policy())).unwrap();
    vm
}

fn core(vm: &Machine, n: u32) -> u64 {
    vm.native.core(n).unwrap()
}

/// Spin briefly without sleeping, so interruptions are dense.
fn spin(iterations: u32) {
    for _ in 0..iterations {
        std::hint::spin_loop();
    }
}

/// Everything a stopped machine exposes for comparison: the stopped state
/// parts (CPU, GIC, clock, devices, RAM and PV page), the execution chain and
/// pending output.
#[derive(Debug, PartialEq)]
struct Evidence {
    parts: Vec<(String, String)>,
    chain: [u8; 32],
    pending: Vec<u8>,
}

fn evidence(vm: &mut Machine) -> Evidence {
    let root = tempfile::tempdir().unwrap();
    let stopped = vm.stopped_evidence(&root.path().join("pages")).unwrap();
    let execution = vm.native_execution().unwrap();
    Evidence {
        parts: stopped.parts,
        chain: execution.chain,
        pending: execution.pending,
    }
}

/// Guest entries so far, interrupted ones included.
fn entries(vm: &Machine) -> u64 {
    vm.native.entries
}

struct Run {
    evidence: Evidence,
    words: [u32; 3],
    registers: [u64; 3],
    entries: u64,
    retries: u32,
}

/// Run the futex guest through fixed bounds. With `interrupt`, another thread
/// keeps forcing the vCPU out of guest execution (`hv_vcpus_exit`) while host
/// threads compete for CPUs.
fn futex_run(interrupt: bool) -> Run {
    let kernel = image(FUTEX);
    let mut vm = machine(&kernel);
    let stop = Arc::new(AtomicBool::new(false));
    let cpu = vm.native.cpu.unwrap();
    let mut helpers = Vec::new();
    if interrupt {
        let done = stop.clone();
        helpers.push(std::thread::spawn(move || {
            while !done.load(Ordering::Acquire) {
                unsafe { hv_vcpus_exit(&cpu, 1) };
                spin(2_000);
            }
        }));
        for _ in 0..2 {
            let stop = stop.clone();
            helpers.push(std::thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    spin(10_000);
                }
            }));
        }
    }
    let mut horizon = 0;
    while core(&vm, 27) == 0 && horizon < 4_000_000 {
        horizon += 1_000;
        let now = vm.state.now;
        vm.run(now, horizon, 1000, false, &[], Vec::new()).unwrap();
    }
    // A few more bounds in the final idle loop.
    for _ in 0..4 {
        horizon += 1_000;
        let now = vm.state.now;
        vm.run(now, horizon, 1000, false, &[], Vec::new()).unwrap();
    }
    stop.store(true, Ordering::Release);
    for helper in helpers {
        helper.join().unwrap();
    }
    // The vCPU is stopped; nothing else writes guest RAM.
    let layout = vm.ram.layout().unwrap().clone();
    let at = layout
        .offset(layout.base + 4096 + offset(Layout::Words) as u64, 16)
        .unwrap();
    let mut words = [0u32; 3];
    let ram = unsafe { vm.ram.bytes() };
    for (index, word) in words.iter_mut().enumerate() {
        *word = u32::from_le_bytes(ram[at + index * 4..at + index * 4 + 4].try_into().unwrap());
    }
    // Store failures the guest retried: host-dependent, so cleared before comparison.
    let retries = u32::from_le_bytes(ram[at + 12..at + 16].try_into().unwrap());
    unsafe { vm.ram.modify(|ram| ram[at + 12..at + 16].fill(0)) };
    Run {
        evidence: evidence(&mut vm),
        words,
        registers: [core(&vm, 26), core(&vm, 27), core(&vm, 28)],
        entries: entries(&vm),
        retries,
    }
}

/// Host-only interruptions during exclusive retry loops (vCPU cancellation
/// and host CPU contention, which clear the physical monitor) change no
/// guest-visible result: the same futex words, registers, execution chain,
/// clock, PV page with its exclusive epoch, and stopped state as an
/// uninterrupted run.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn futex_retries_are_invisible_to_host_interruptions() {
    let quiet = futex_run(false);
    let mut add = 0u32;
    let mut xor = 0u32;
    for i in 1..=FUTEX_ITERATIONS {
        add = add.wrapping_add((i | 1) as u32);
        xor ^= (i.wrapping_mul(0x9e37)) as u32;
    }
    assert_eq!(quiet.words, [add, xor, 0], "futex words");
    assert_eq!(quiet.registers, [FUTEX_ITERATIONS, add as u64, xor as u64]);
    let mut retried = 0;
    for attempt in 0..3 {
        let interrupted = futex_run(true);
        retried += interrupted.retries;
        assert!(
            interrupted.entries > quiet.entries,
            "attempt {attempt}: no host interruption reached the guest ({} entries)",
            interrupted.entries
        );
        assert_eq!(interrupted.words, quiet.words, "attempt {attempt}");
        assert_eq!(interrupted.registers, quiet.registers, "attempt {attempt}");
        assert_eq!(interrupted.evidence, quiet.evidence, "attempt {attempt}");
        eprintln!(
            "futex: {} guest entries and {} retried stores uninterrupted; {} and {} interrupted",
            quiet.entries, quiet.retries, interrupted.entries, interrupted.retries
        );
    }
    // Cancellations between LDXR and STLXR clear the physical monitor.
    assert!(
        retried > 0,
        "no interruption landed inside an exclusive pair"
    );
}

/// One machine's result at the end of a run.
struct Final {
    evidence: Evidence,
    ticks: u64,
    uart: u64,
}

fn finish(vm: &mut Machine) -> Final {
    Final {
        evidence: evidence(vm),
        ticks: core(vm, 19),
        uart: core(vm, 20),
    }
}

/// Drive `vm` to `to` as the Driver does: each blocked stop resumes at its
/// deadline, and a machine idle past `to` adopts `to` without entering.
fn run_to(vm: &mut Machine, to: u64) {
    let mut now = vm.state.now;
    loop {
        match vm.run(now, to, 1000, false, &[], Vec::new()).unwrap() {
            Stop::Horizon => return,
            Stop::Blocked(Some(deadline)) if deadline < to => now = deadline,
            Stop::Blocked(_) if now < to => now = to,
            Stop::Blocked(_) => return,
            stop => panic!("unexpected stop {stop:?}"),
        }
    }
}

/// Run an interrupt-mode guest to `capture`, checkpoint it and continue to
/// `end`; then (HVF allows one VM per process) restore the checkpoint into a
/// fresh VM and run it to `end`. Returns both results, the captured deferred
/// and asserted lines, and the captured PC.
fn capture_and_restore(
    mode: u32,
    capture: u64,
    end: u64,
) -> (Final, Final, Vec<u32>, Vec<u32>, u64) {
    let kernel = image(mode);
    let mut original = machine(&kernel);
    run_to(&mut original, capture);
    assert_eq!(original.state.now, capture);
    let deferred = original.state.devices.deferred.iter().copied().collect();
    let asserted = original.state.devices.asserted.iter().copied().collect();
    let pc = core(&original, 32);
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("machine.snapshot");
    original.capture(&path).unwrap();
    let inputs = BootInputs {
        witness: protocol::BootWitness {
            mark: None,
            boot: original.state.witness_boot,
            initial: original.state.witness_origin,
        },
        memory_bytes: MEMORY,
        kernel: kernel.clone(),
        initrd: INITRD.to_vec(),
        seed: 1,
        command_line: String::new(),
        disk: None,
        network: None,
        policy: Some(policy()),
    };
    run_to(&mut original, end);
    let continued = finish(&mut original);
    drop(original);
    let mut restored = restore_machine(&inputs, read_snapshot(&path, &inputs).unwrap()).unwrap();
    run_to(&mut restored, end);
    (continued, finish(&mut restored), deferred, asserted, pc)
}

/// Checkpoint a guest whose timer interrupt is pending in the GIC, active
/// (acknowledged, before EOI), or deferred while DAIF masks IRQs, with a
/// PL011 device interrupt as a second source; restore into a fresh VM. The
/// restored run must take each interrupt once, keep counting and rearming,
/// and end with the same registers, execution chain and stopped state as the
/// uninterrupted continuation.
#[test]
#[ignore = "requires HVF and a test executable signed with the hypervisor entitlement"]
fn checkpoints_restore_pending_active_and_masked_interrupts() {
    let end = 40 * PERIOD + PERIOD / 2;
    for (mode, name, capture) in [
        (PENDING, "pending", 10 * PERIOD),
        (ACTIVE, "active", 10 * PERIOD + PERIOD / 2),
        (MASKED, "masked", 9 * PERIOD),
    ] {
        let (continued, restored, deferred, asserted, pc) = capture_and_restore(mode, capture, end);
        eprintln!(
            "{name}: captured at {capture} ns with PC {pc:#x}, deferred {deferred:?}, \
             asserted {asserted:?}; ticks {} uart {}",
            continued.ticks, continued.uart
        );
        match mode {
            PENDING => assert!(asserted.contains(&TIMER), "{name}: {asserted:?}"),
            MASKED => assert!(deferred.contains(&TIMER), "{name}: {deferred:?}"),
            _ => {
                // Stopped inside the handler, between acknowledge and EOI.
                let handler = 0x4000_0000 + 4096 + offset(Layout::Irq) as u64;
                assert!(
                    (handler..handler + 0x80).contains(&pc),
                    "{name}: PC {pc:#x}"
                );
            }
        }
        // Every deadline was taken once and rearmed.
        assert!(
            continued.ticks >= 30,
            "{name}: only {} ticks",
            continued.ticks
        );
        assert!(
            continued.uart + 1 >= continued.ticks / 4,
            "{name}: UART interrupts"
        );
        assert_eq!(
            (restored.ticks, restored.uart),
            (continued.ticks, continued.uart),
            "{name}"
        );
        assert_eq!(restored.evidence, continued.evidence, "{name}");
    }
}
