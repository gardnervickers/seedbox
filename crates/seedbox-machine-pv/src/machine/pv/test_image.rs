//! Raw boot images for native PV worker tests. Each fixture performs the same
//! PV ABI 3 operations on every architecture; only the encoding differs.
//! ARM64 fixtures are Linux Images; x86-64 fixtures are minimal 64-bit
//! bzImages entered in long mode at the protected-mode payload + 0x200.
#![allow(dead_code)]
use seedbox_exec::Guarded;

/// PV operations used by raw-image fixtures, in execution order.
#[derive(Clone, Copy)]
pub(super) enum Op {
    /// Publish the ABI 3 ready handshake.
    Ready,
    /// Arm the PV timer at an absolute deadline.
    Timer(u8),
    /// Arm the PV timer `n` nanoseconds after the published page time.
    TimerAfterNow(u8),
    /// Enter PV idle.
    Idle,
    /// Yield to the published bound.
    Yield,
    /// Power off the machine.
    PowerOff,
    /// Loop forever.
    Spin,
}

#[cfg(target_arch = "aarch64")]
pub(super) fn kernel(words: &[u32]) -> Vec<u8> {
    let mut bytes = vec![0; 64 + words.len() * 4];
    bytes[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes()); // B +64, skip Image header.
    bytes[16..24].copy_from_slice(&4096u64.to_le_bytes());
    bytes[56..60].copy_from_slice(b"ARM\x64");
    for (index, word) in words.iter().enumerate() {
        bytes[64 + index * 4..68 + index * 4].copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

#[cfg(target_arch = "aarch64")]
pub(super) fn program(ops: &[Op]) -> Vec<u8> {
    let mut words = Vec::new();
    for op in ops {
        words.extend_from_slice(match op {
            // MOV X1,#0x0a010000; MOV X0,#3; STR X0,[X1,#32].
            Op::Ready => &[0xd2a1_4021, 0xd280_0060, 0xf900_1020][..],
            // MOV X0,#n; STR X0,[X1].
            Op::Timer(5) => &[0xd280_00a0, 0xf900_0020],
            Op::Timer(_) => unimplemented!("fixture timer"),
            // MOV X2,#0x0a000000; LDR X0,[X2]; ADD X0,X0,#5; STR X0,[X1].
            Op::TimerAfterNow(5) => &[0xd2a1_4002, 0xf940_0040, 0x9100_1400, 0xf900_0020],
            Op::TimerAfterNow(_) => unimplemented!("fixture timer"),
            Op::Idle => &[0xf900_0820],  // STR X0,[X1,#16].
            Op::Yield => &[0xf900_0c20], // STR X0,[X1,#24].
            // MOV X0,#8; MOVK X0,#0x8400,LSL #16 (PSCI SYSTEM_OFF); HVC #0.
            Op::PowerOff => &[0xd280_0100, 0xf2b0_8000, 0xd400_0002],
            Op::Spin => &[0x1400_0000], // B .
        });
    }
    kernel(&words)
}

/// A minimal bzImage: one setup sector and a 64-bit entry 0x200 into the payload.
#[cfg(target_arch = "x86_64")]
pub(super) fn kernel(code: &[u8]) -> Vec<u8> {
    let mut image = vec![0; 1024 + 0x200];
    image[0x1f1] = 1;
    image[0x201] = 0x66; // Setup header ends at 0x268.
    image[0x202..0x206].copy_from_slice(b"HdrS");
    image[0x206..0x208].copy_from_slice(&0x020fu16.to_le_bytes());
    image[0x211] = 1; // LOADED_HIGH.
    image[0x22c..0x230].copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
    image[0x236..0x238].copy_from_slice(&1u16.to_le_bytes()); // XLF_KERNEL_64.
    image[0x258..0x260].copy_from_slice(&0x10_0000u64.to_le_bytes());
    image[0x260..0x264].copy_from_slice(&0x1_0000u32.to_le_bytes());
    image.extend_from_slice(code);
    image
}

#[cfg(target_arch = "x86_64")]
pub(super) fn program(ops: &[Op]) -> Vec<u8> {
    let mut code = Vec::new();
    for op in ops {
        match *op {
            // MOV EBX,0x0a010000; MOV EAX,3; MOV [RBX+32],RAX.
            Op::Ready => code.extend_from_slice(&[
                0xbb, 0x00, 0x00, 0x01, 0x0a, 0xb8, 0x03, 0x00, 0x00, 0x00, 0x48, 0x89, 0x43, 0x20,
            ]),
            // MOV EAX,n; MOV [RBX],RAX.
            Op::Timer(n) => {
                code.extend_from_slice(&[0xb8, n, 0, 0, 0, 0x48, 0x89, 0x03]);
            }
            // MOV ECX,0x0a000000; MOV RAX,[RCX]; ADD RAX,n; MOV [RBX],RAX.
            Op::TimerAfterNow(n) => code.extend_from_slice(&[
                0xb9, 0x00, 0x00, 0x00, 0x0a, 0x48, 0x8b, 0x01, 0x48, 0x83, 0xc0, n, 0x48, 0x89,
                0x03,
            ]),
            Op::Idle => code.extend_from_slice(&[0x48, 0x89, 0x43, 0x10]), // MOV [RBX+16],RAX.
            Op::Yield => code.extend_from_slice(&[0x48, 0x89, 0x43, 0x18]), // MOV [RBX+24],RAX.
            // MOV EAX,1; MOV [RBX+40],RAX (PV power-off register).
            Op::PowerOff => {
                code.extend_from_slice(&[0xb8, 0x01, 0x00, 0x00, 0x00, 0x48, 0x89, 0x43, 0x28])
            }
            Op::Spin => code.extend_from_slice(&[0xeb, 0xfe]), // JMP .
        }
    }
    kernel(&code)
}

/// A static Linux PID 1 that writes `PV_LINUX_PASS` and powers off.
pub(super) fn linux_initrd() -> Vec<u8> {
    let message = b"PV_LINUX_PASS\n";
    #[cfg(target_arch = "aarch64")]
    let (machine, code) = {
        let words: [u32; 15] = [
            0xd280_0020, // MOV X0,#1 (stdout).
            0x1000_01c1, // ADR X1,+56 (message).
            0xd280_0002 | ((message.len() as u32) << 5),
            0xd280_0808, // MOV X8,#64 (__NR_write).
            0xd400_0001, // SVC #0.
            0xd29b_d5a0, // MOV X0,#0xdead.
            0xf2bf_dc20, // MOVK X0,#0xfee1,LSL #16.
            0xd283_2d21, // MOV X1,#0x1969.
            0xf2a5_0241, // MOVK X1,#0x2812,LSL #16.
            0xd29f_db82, // MOV X2,#0xfedc.
            0xf2a8_6422, // MOVK X2,#0x4321,LSL #16 (POWER_OFF).
            0xd280_0003, // MOV X3,#0.
            0xd280_11c8, // MOV X8,#142 (__NR_reboot).
            0xd400_0001, // SVC #0.
            0x1400_0000, // B . if reboot unexpectedly returns.
        ];
        (
            183u16,
            words
                .iter()
                .flat_map(|w| w.to_le_bytes())
                .collect::<Vec<_>>(),
        )
    };
    #[cfg(target_arch = "x86_64")]
    let (machine, code) = {
        let mut code = vec![
            0xbf,
            0x01,
            0x00,
            0x00,
            0x00, // MOV EDI,1 (stdout).
            0x48,
            0x8d,
            0x35,
            0,
            0,
            0,
            0, // LEA RSI,[RIP+message].
            0xba,
            message.len() as u8,
            0x00,
            0x00,
            0x00, // MOV EDX,len.
            0xb8,
            0x01,
            0x00,
            0x00,
            0x00, // MOV EAX,1 (__NR_write).
            0x0f,
            0x05, // SYSCALL.
            0xbf,
            0xad,
            0xde,
            0xe1,
            0xfe, // MOV EDI,0xfee1dead.
            0xbe,
            0x69,
            0x19,
            0x12,
            0x28, // MOV ESI,0x28121969.
            0xba,
            0xdc,
            0xfe,
            0x21,
            0x43, // MOV EDX,0x4321fedc (POWER_OFF).
            0x45,
            0x31,
            0xd2, // XOR R10D,R10D.
            0xb8,
            0xa9,
            0x00,
            0x00,
            0x00, // MOV EAX,169 (__NR_reboot).
            0x0f,
            0x05, // SYSCALL.
            0xeb,
            0xfe, // JMP . if reboot unexpectedly returns.
        ];
        let displacement = (code.len() - 12) as u32;
        code[8..12].copy_from_slice(&displacement.to_le_bytes());
        (62u16, code)
    };
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC.
    elf[18..20].copy_from_slice(&machine.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&0x400078u64.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD.
    elf[68..72].copy_from_slice(&5u32.to_le_bytes()); // Read/execute.
    elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
    elf[88..96].copy_from_slice(&0x400000u64.to_le_bytes());
    let length = (elf.len() + code.len() + message.len()) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    elf.extend_from_slice(&code);
    elf.extend_from_slice(message);
    let mut archive = Vec::new();
    for (name, mode, data, major, minor) in [
        ("dev", 0o040755, &[][..], 0, 0),
        ("dev/console", 0o020600, &[][..], 5, 1),
        ("init", 0o100755, &elf[..], 0, 0),
        ("TRAILER!!!", 0, &[][..], 0, 0),
    ] {
        archive.extend_from_slice(b"070701");
        for field in [
            1,
            mode,
            0,
            0,
            1,
            0,
            data.len(),
            0,
            0,
            major,
            minor,
            name.len() + 1,
            0,
        ] {
            archive.extend_from_slice(format!("{field:08x}").as_bytes());
        }
        archive.extend_from_slice(name.as_bytes());
        archive.push(0);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
        archive.extend_from_slice(data);
        while archive.len() % 4 != 0 {
            archive.push(0);
        }
    }
    archive
}

/// Assemble a raw fixture with SEEDBOX_PV_CLANG/SEEDBOX_PV_LD into a boot image.
/// x86 sources run in long mode at the bzImage entry; use RIP-relative data.
pub(super) fn assemble(source: &str) -> Vec<u8> {
    use std::{fs, process::Command};
    let input = |name: &str| std::env::var_os(name).unwrap_or_else(|| panic!("set {name}"));
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fixture.S");
    let object = directory.path().join("fixture.o");
    let raw = directory.path().join("fixture.bin");
    fs::write(&path, source).unwrap();
    #[cfg(target_arch = "aarch64")]
    let (target, emulation) = ("--target=aarch64-none-elf", "aarch64elf");
    #[cfg(target_arch = "x86_64")]
    let (target, emulation) = ("--target=x86_64-none-elf", "elf_x86_64");
    assert!(Command::new(input("SEEDBOX_PV_CLANG"))
        .args([target, "-c"])
        .arg(&path)
        .arg("-o")
        .arg(&object)
        .guarded_status()
        .unwrap()
        .success());
    assert!(Command::new(input("SEEDBOX_PV_LD"))
        .args([
            "-m",
            emulation,
            "-Ttext=0",
            "--image-base=0",
            "-e",
            "_start",
            "--oformat=binary"
        ])
        .arg(&object)
        .arg("-o")
        .arg(&raw)
        .guarded_status()
        .unwrap()
        .success());
    let code = fs::read(raw).unwrap();
    #[cfg(target_arch = "aarch64")]
    let image = {
        let mut kernel = vec![0; 64];
        kernel[..4].copy_from_slice(&0x1400_0010u32.to_le_bytes());
        kernel[16..24].copy_from_slice(&4096u64.to_le_bytes());
        kernel[56..60].copy_from_slice(b"ARM\x64");
        kernel.extend(code);
        kernel
    };
    #[cfg(target_arch = "x86_64")]
    let image = kernel(&code);
    image
}

pub(super) fn timed_guest() -> Vec<u8> {
    program(&[Op::Ready, Op::Timer(5), Op::Idle, Op::PowerOff])
}
