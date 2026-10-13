//! Minimal ARM64 Linux application: nanosleep, then exit successfully.
//! Build the ELF in memory without a cross compiler.

pub fn sleeping_program(seconds: u32) -> Vec<u8> {
    let words: [u32; 8] = [
        0x1000_0100, // ADR X0,+32: timespec immediately after instructions.
        0xd280_0001, // MOV X1,#0.
        0xd280_0ca8, // MOV X8,#101: nanosleep.
        0xd400_0001, // SVC #0.
        0xd280_0000, // MOV X0,#0: successful application outcome.
        0xd280_0ba8, // MOV X8,#93: exit.
        0xd400_0001, // SVC #0.
        0x1400_0000, // Remain here if exit unexpectedly returns.
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
    elf
}
