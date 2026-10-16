//! Minimal standard x86-64 Linux application; no compiler or PV instrumentation.

pub fn sleeping_program(seconds: u32) -> Vec<u8> {
    let code: [u8; 32] = [
        0x48, 0x8D, 0x3D, 0x19, 0x00, 0x00,
        0x00, // lea rdi,[rip+25]: timespec after the code.
        0x31, 0xF6, // xor esi,esi: no remaining-time result.
        0xB8, 0x23, 0x00, 0x00, 0x00, // mov eax,35: Linux nanosleep.
        0x0F, 0x05, // syscall: sleep on the normal kernel timer.
        0x31, 0xFF, // xor edi,edi: successful exit status.
        0xB8, 0x3C, 0x00, 0x00, 0x00, // mov eax,60: Linux exit.
        0x0F, 0x05, // syscall: terminate the program.
        0xEB, 0xFE, // jmp $: remain here if exit unexpectedly returns.
        0x90, 0x90, 0x90, 0x90, 0x90, // nop: align the following 64-bit timespec fields.
    ];
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC.
    elf[18..20].copy_from_slice(&62u16.to_le_bytes()); // EM_X86_64.
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
    elf.extend_from_slice(&u64::from(seconds).to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf
}
