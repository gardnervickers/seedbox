//! Actual UART/lifecycle interleave through the stopped PV worker protocol.
use super::*;
use crate::{
    clock::SimulationClock,
    pv::{Inbox, Outbox, Output, Response as GuestResponse},
    rng::SeededRng,
};
use std::{collections::VecDeque, process::Command};

fn image() -> Vec<u8> {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("console.S");
    let object = directory.path().join("console.o");
    let raw = directory.path().join("console.bin");
    fs::write(
        &source,
        r#".text
.global _start
_start:
 movz x20, #0xa00, lsl #16
 movz x21, #0xa01, lsl #16
 movz x22, #0x900, lsl #16
 movz x23, #0xb01, lsl #16
 mov x0, #3
 str x0, [x21, #32]
 // Two TX descriptors, made available separately around a UART write.
 adr x9, descriptors
 adr x0, message1
 str x0, [x9]
 mov w0, #14
 str w0, [x9, #8]
 adr x0, message2
 str x0, [x9, #16]
 mov w0, #14
 str w0, [x9, #24]
 mov w0, #1
 str w0, [x23, #0x30]
 mov w0, #2
 str w0, [x23, #0x38]
 str w9, [x23, #0x80]
 adr x10, available
 str w10, [x23, #0x90]
 adr x0, used
 str w0, [x23, #0xa0]
 mov w0, #1
 str w0, [x23, #0x44]
 mov w0, #4
 str w0, [x23, #0x70]
 mov x19, #1
 bl charge
 mov w0, #65
 str w0, [x22]
 mov w0, #1
 strh w0, [x10, #2]
 str w0, [x23, #0x50]
 mov w0, #67
 str w0, [x22]
 mov w0, #2
 strh w0, [x10, #2]
 mov w0, #1
 str w0, [x23, #0x50]
 mov x19, #4
 bl charge
 mov w0, #69
 str w0, [x22]
 str xzr, [x21, #24]
 movz x0, #8
 movk x0, #0x8400, lsl #16
 hvc #0
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
 ret
.balign 16
descriptors: .zero 32
// Suppress TX used-ring IRQ: the fixture has no exception handler.
available: .hword 1,0,0,1,0
.balign 16
used: .zero 32
message1: .ascii "SBX_LOG1 1 42\n"
message2: .ascii "SBX_LOG1 2 44\n"
"#,
    )
    .unwrap();
    let clang = std::env::var_os("SEEDBOX_PV_CLANG").unwrap();
    let ld = std::env::var_os("SEEDBOX_PV_LD").unwrap();
    assert!(
        Command::new(clang)
            .args(["--target=aarch64-none-elf", "-c"])
            .arg(&source)
            .arg("-o")
            .arg(&object)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(ld)
            .args([
                "-m",
                "aarch64elf",
                "-Ttext=0",
                "--image-base=0",
                "-e",
                "_start",
                "--oformat=binary"
            ])
            .arg(&object)
            .arg("-o")
            .arg(&raw)
            .status()
            .unwrap()
            .success()
    );
    let mut kernel = vec![0; 64];
    kernel[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes());
    kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
    kernel[56..60].copy_from_slice(b"ARM\x64");
    kernel.extend(fs::read(raw).unwrap());
    kernel
}
fn at(n: u64) -> Timestamp {
    Timestamp::from_nanos(n)
}
fn run(machine: &mut PVMachine, now: u64, upto: u64) -> Vec<Output> {
    let clock = SimulationClock::new(now);
    let mut rng = SeededRng::new(1);
    let mut inbox = VecDeque::new();
    let mut outputs = Vec::new();
    let stopped = machine
        .run(
            &mut RunContext {
                worker: &mut (),
                clock: &clock,
                rng: &mut rng,
                execution: Default::default(),
                inbox: Inbox::new(&mut inbox, at(now)),
                outbox: Outbox::new(&mut outputs, at(now), at(upto)),
            },
            at(upto),
        )
        .unwrap();
    assert!(
        matches!(stopped, StopReason::BoundaryReached),
        "unexpected stop: {stopped:?}"
    );
    outputs
}
#[test]
#[ignore = "requires signed SEEDBOX_PV_WORKER and ARM64 SEEDBOX_PV_CLANG/SEEDBOX_PV_LD"]
fn native_console_interleave_has_actual_times_across_partitions_and_restore() {
    let worker = PathBuf::from(std::env::var_os("SEEDBOX_PV_WORKER").unwrap());
    let image = image();
    let create = || PVMachine::from_boot_inputs(&worker, image.clone(), b"initrd".to_vec()).unwrap();
    let mut whole = create();
    let expected = run(&mut whole, 0, 10);
    assert_eq!(
        expected
            .iter()
            .map(|output| output.at.as_nanos())
            .collect::<Vec<_>>(),
        [1, 1, 1, 1, 5]
    );
    let bytes: Vec<_> = expected
        .iter()
        .flat_map(|output| match &output.payload {
            GuestResponse::BootConsole { bytes }
            | GuestResponse::ApplicationOutput { bytes, .. } => bytes.clone(),
            _ => panic!("unexpected response"),
        })
        .collect();
    assert_eq!(bytes, b"ABCDE");
    assert_eq!(whole.console, b"ACE");
    let mut split = create();
    let before = run(&mut split, 0, 3);
    let snapshot = split.snapshot().unwrap();
    let mut trace = before.clone();
    trace.extend(run(&mut split, 3, 10));
    assert_eq!(trace, expected);
    assert_eq!(split.console, b"ACE");
    for _ in 0..2 {
        let mut restored = PVMachine::from_snapshot(&worker, &snapshot).unwrap();
        let mut trace = before.clone();
        trace.extend(run(&mut restored, 3, 10));
        assert_eq!(trace, expected);
        // Historical observations remain with the caller, as before this change.
        assert_eq!(restored.console, b"E");
    }
}
