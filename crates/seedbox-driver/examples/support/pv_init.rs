//! Minimal native Linux application: nanosleep, then exit successfully.
//! Build the ELF in memory without a cross compiler.

#[cfg(target_arch = "aarch64")]
fn program() -> (u16, Vec<u8>) {
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
    // EM_AARCH64.
    (
        183,
        words.iter().flat_map(|word| word.to_le_bytes()).collect(),
    )
}

#[cfg(target_arch = "x86_64")]
fn program() -> (u16, Vec<u8>) {
    let code = vec![
        0x48, 0x8d, 0x3d, 0x14, 0x00, 0x00, 0x00, // LEA RDI,[RIP+20]: timespec after code.
        0x31, 0xf6, // XOR ESI,ESI.
        0xb8, 0x23, 0x00, 0x00, 0x00, // MOV EAX,35: nanosleep.
        0x0f, 0x05, // SYSCALL.
        0x31, 0xff, // XOR EDI,EDI: successful application outcome.
        0xb8, 0x3c, 0x00, 0x00, 0x00, // MOV EAX,60: exit.
        0x0f, 0x05, // SYSCALL.
        0xeb, 0xfe, // Remain here if exit unexpectedly returns.
    ];
    // EM_X86_64.
    (62, code)
}

pub fn sleeping_program(seconds: u32) -> Vec<u8> {
    let (machine, code) = program();
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
    let length = (elf.len() + code.len() + 16) as u64;
    elf[96..104].copy_from_slice(&length.to_le_bytes());
    elf[104..112].copy_from_slice(&length.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    elf.extend_from_slice(&code);
    // Linux timespec: seconds and nanoseconds, both signed 64-bit.
    elf.extend_from_slice(&u64::from(seconds).to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf
}
