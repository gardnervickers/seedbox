//! Minimal ARM64 Linux PID 1: nanosleep for the supplied duration, then power off.
//! Build the ELF and uncompressed newc initramfs in memory, without a cross compiler.

pub fn sleeping_initrd(seconds: u32) -> Vec<u8> {
    let words: [u32; 14] = [
        0x1000_01c0, // ADR X0,+56: address of the timespec after these instructions.
        0xd280_0001, // MOV X1,#0: no remaining-time output.
        0xd280_0ca8, // MOV X8,#101: Linux AArch64 __NR_nanosleep.
        0xd400_0001, // SVC #0: sleep through the guest kernel's timer.
        0xd29b_d5a0, // MOV X0,#0xdead: low half of reboot magic 0xfee1dead.
        0xf2bf_dc20, // MOVK X0,#0xfee1,LSL #16: high half of reboot magic.
        0xd283_2d21, // MOV X1,#0x1969: low half of reboot magic 0x28121969.
        0xf2a5_0241, // MOVK X1,#0x2812,LSL #16: high half of second magic.
        0xd29f_db82, // MOV X2,#0xfedc: low half of LINUX_REBOOT_CMD_POWER_OFF.
        0xf2a8_6422, // MOVK X2,#0x4321,LSL #16: complete command 0x4321fedc.
        0xd280_0003, // MOV X3,#0: unused reboot argument.
        0xd280_11c8, // MOV X8,#142: Linux AArch64 __NR_reboot.
        0xd400_0001, // SVC #0: power off the guest.
        0x1400_0000, // B .: remain here if poweroff unexpectedly returns.
    ];
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC.
    elf[18..20].copy_from_slice(&183u16.to_le_bytes()); // EM_AARCH64.
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
    let length = (elf.len() + words.len() * 4 + 16) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    for word in words {
        elf.extend_from_slice(&word.to_le_bytes());
    }
    // Linux AArch64 timespec: seconds and nanoseconds, both signed 64-bit.
    elf.extend_from_slice(&u64::from(seconds).to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
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
