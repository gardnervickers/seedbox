//! Minimal standard RV64 Linux application; no compiler or PV instrumentation.

pub fn sleeping_program(seconds: u32) -> Vec<u8> {
    let words: [u32; 10] = [
        0x0000_0517, // auipc a0,0: address of this instruction.
        0x0285_0513, // addi a0,a0,40: aligned timespec after the instructions.
        0x0000_0593, // addi a1,zero,0: no remaining-time result.
        0x0650_0893, // addi a7,zero,101: Linux nanosleep.
        0x0000_0073, // ecall: sleep on the normal kernel timer.
        0x0000_0513, // addi a0,zero,0: successful exit status.
        0x05d0_0893, // addi a7,zero,93: Linux exit.
        0x0000_0073, // ecall: terminate the program.
        0x0000_006f, // jal zero,0: remain here if exit unexpectedly returns.
        0x0000_0013, // nop: align the following 64-bit timespec fields.
    ];
    let mut elf = vec![0; 120];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC.
    elf[18..20].copy_from_slice(&243u16.to_le_bytes()); // EM_RISCV.
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
    elf.extend_from_slice(&u64::from(seconds).to_le_bytes());
    elf.extend_from_slice(&0u64.to_le_bytes());
    elf
}
