//! x86 proof subset decoded and relocated by iced-x86 (MIT). No handwritten
//! variable-length decoder. LOCK/REP/XSAVE and partial-progress operations are
//! not implemented yet and fail before any instruction in the block executes.
use crate::{Block, Error, PcMap};
use alloc::vec::Vec;
use iced_x86::{Code, Decoder, DecoderOptions, Encoder, Mnemonic, OpKind, Register};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Direct(u64),
    Register(u8),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Jump(Target),
    Call(Target),
    Return,
    Conditional { target: u64, condition: u8 },
}
/// Register numbers follow the architectural ModRM numbering, not Linux gregs.
pub fn register(r: Register) -> Option<u8> {
    if (Register::RAX..=Register::R15).contains(&r) {
        Some((r as u32 - Register::RAX as u32) as u8)
    } else {
        None
    }
}
fn condition_code(m: Mnemonic) -> Option<u8> {
    Some(match m {
        Mnemonic::Jo => 0,
        Mnemonic::Jno => 1,
        Mnemonic::Jb => 2,
        Mnemonic::Jae => 3,
        Mnemonic::Je => 4,
        Mnemonic::Jne => 5,
        Mnemonic::Jbe => 6,
        Mnemonic::Ja => 7,
        Mnemonic::Js => 8,
        Mnemonic::Jns => 9,
        Mnemonic::Jp => 10,
        Mnemonic::Jnp => 11,
        Mnemonic::Jl => 12,
        Mnemonic::Jge => 13,
        Mnemonic::Jle => 14,
        Mnemonic::Jg => 15,
        _ => return None,
    })
}
pub fn condition(flags: u64, code: u8) -> bool {
    let c = flags & 1 != 0;
    let p = flags & 4 != 0;
    let z = flags & 64 != 0;
    let s = flags & 128 != 0;
    let o = flags & 2048 != 0;
    let base = match code >> 1 {
        0 => o,
        1 => c,
        2 => z,
        3 => c || z,
        4 => s,
        5 => p,
        6 => s != o,
        _ => s != o || z,
    };
    if code & 1 != 0 {
        !base
    } else {
        base
    }
}
pub fn translate(source: &[u8], pc: u64, cache: u64) -> Result<Block<Terminal>, Error> {
    let mut decoder = Decoder::with_ip(64, source, pc, DecoderOptions::NONE);
    let mut bytes = Vec::new();
    let mut maps = Vec::new();
    for n in 0..256u32 {
        let at = decoder.ip();
        if !decoder.can_decode() {
            return Err(Error::Truncated { pc: at });
        }
        let i = decoder.decode();
        at.checked_add(i.len() as u64).ok_or(Error::Range)?;
        if i.is_invalid() {
            return Err(Error::Unsupported { pc: at });
        }
        if i.has_lock_prefix() || i.has_rep_prefix() || i.has_repne_prefix() {
            return Err(Error::Unsupported { pc: at });
        }
        let branch_target = match i.op0_kind() {
            OpKind::NearBranch64 => Some(Target::Direct(i.near_branch_target())),
            OpKind::Register => register(i.op0_register()).map(Target::Register),
            _ => None,
        };
        let terminal = match i.code() {
            Code::Jmp_rel8_64 | Code::Jmp_rel32_64 | Code::Jmp_rm64 => {
                branch_target.map(Terminal::Jump)
            }
            Code::Call_rel32_64 | Code::Call_rm64 => branch_target.map(Terminal::Call),
            Code::Retnq => Some(Terminal::Return),
            _ => condition_code(i.mnemonic())
                .filter(|_| i.op0_kind() == OpKind::NearBranch64)
                .map(|condition| Terminal::Conditional {
                    target: i.near_branch_target(),
                    condition,
                }),
        };
        let emitted = cache.checked_add(bytes.len() as u64).ok_or(Error::Range)?;
        if let Some(terminal) = terminal {
            bytes.push(0xcc); // executor INT3, not the application branch
            maps.push(PcMap {
                cache: emitted..emitted.checked_add(1).ok_or(Error::Range)?,
                source: at,
            });
            return Ok(Block {
                bytes,
                maps,
                source: pc..decoder.ip(),
                instructions: n + 1,
                terminal,
                trap: emitted,
            });
        }
        let safe = match i.code() {
            Code::Nopq
            | Code::Nopd
            | Code::Mov_r64_imm64
            | Code::Mov_r64_rm64
            | Code::Lea_r64_m => true,
            Code::Mov_rm64_r64
            | Code::Mov_rm64_imm32
            | Code::Add_rm64_imm8
            | Code::Add_rm64_imm32
            | Code::Sub_rm64_imm8
            | Code::Sub_rm64_imm32
            | Code::Cmp_rm64_imm8
            | Code::Cmp_rm64_imm32
            | Code::Add_rm64_r64
            | Code::Sub_rm64_r64
            | Code::Cmp_rm64_r64
            | Code::Xor_rm64_r64 => i.op0_kind() == OpKind::Register,
            Code::Paddq_xmm_xmmm128 => i.op1_kind() == OpKind::Register,
            _ => false,
        };
        if !safe {
            return Err(Error::Unsupported { pc: at });
        }
        let mut encoder = Encoder::new(64);
        encoder
            .encode(&i, emitted)
            .map_err(|_| Error::Relocation { pc: at })?;
        let encoded = encoder.take_buffer();
        maps.push(PcMap {
            cache: emitted
                ..emitted
                    .checked_add(encoded.len() as u64)
                    .ok_or(Error::Range)?,
            source: at,
        });
        bytes.extend(encoded);
    }
    Err(Error::Unsupported { pc })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_wrapping_source_and_cache_ranges() {
        assert!(matches!(translate(&[0xc3], u64::MAX, 0), Err(Error::Range)));
        assert!(matches!(translate(&[0xc3], 0, u64::MAX), Err(Error::Range)));
    }
    #[test]
    fn copies_ordinary_and_replaces_backward_branch() {
        let b = translate(&[0x48, 0x83, 0xc0, 1, 0xeb, 0xfa], 0x1000, 0x2000).unwrap();
        assert_eq!(b.bytes, &[0x48, 0x83, 0xc0, 1, 0xcc]);
        assert_eq!(b.instructions, 2);
        assert_eq!(b.terminal, Terminal::Jump(Target::Direct(0x1000)));
    }
    #[test]
    fn rip_relative_load_still_addresses_original_data() {
        let b = translate(&[0x48, 0x8b, 0x05, 0x10, 0, 0, 0, 0xc3], 0x1000, 0x2000).unwrap();
        let mut d = Decoder::with_ip(64, &b.bytes, 0x2000, DecoderOptions::NONE);
        assert_eq!(d.decode().ip_rel_memory_address(), 0x1017);
        assert!(matches!(
            translate(&[0x48, 0x8b, 5, 0, 0, 0, 0, 0xc3], 0x1000, 0x1_0000_0000),
            Err(Error::Relocation { .. })
        ));
    }
    #[test]
    fn indirect_call_return_and_overlapping_entry_decode() {
        assert_eq!(
            translate(&[0xff, 0xd0], 0, 0x1000).unwrap().terminal,
            Terminal::Call(Target::Register(0))
        );
        assert_eq!(
            translate(&[0xc3], 0, 0x1000).unwrap().terminal,
            Terminal::Return
        );
        let code = [0x48, 0xb8, 0x90, 0xc3, 0, 0, 0, 0, 0, 0, 0xc3];
        assert_eq!(translate(&code, 0, 0x1000).unwrap().instructions, 2);
        assert_eq!(translate(&code[2..], 2, 0x2000).unwrap().instructions, 2);
    }
    #[test]
    fn rejects_store_atomic_rep_syscall_unknown_and_incomplete() {
        for bytes in [
            &[0x48, 0x89, 0x00][..],
            &[0xf0, 0x48, 0x01, 0x00],
            &[0xf3, 0xa4],
            &[0x0f, 0x05],
            &[0xcc],
            &[0x48],
        ] {
            assert!(matches!(
                translate(bytes, 0, 0x1000),
                Err(Error::Unsupported { .. })
            ));
        }
    }
}
