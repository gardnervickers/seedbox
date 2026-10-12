//! Explicit A64 proof subset. This is not a production ISA admission policy.
//! Fixed-width classification rejects unknown instructions (notably LL/SC,
//! LSE, system instructions and stores) rather than copying them blindly.
use crate::{Block, Error, PcMap};
use alloc::vec::Vec;

pub const TRAP: u32 = 0xd420_bc00; // brk #0x5e0
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    Direct {
        target: u64,
        link: bool,
    },
    Register {
        register: u8,
        link: bool,
    },
    Condition {
        target: u64,
        condition: u8,
    },
    Compare {
        target: u64,
        register: u8,
        nonzero: bool,
        wide: bool,
    },
    Test {
        target: u64,
        register: u8,
        bit: u8,
        nonzero: bool,
    },
}
fn sext(v: u32, bits: u32) -> i64 {
    ((v as i64) << (64 - bits)) >> (64 - bits)
}
fn target(pc: u64, immediate: i64) -> u64 {
    pc.wrapping_add_signed(immediate)
}
fn relative(pc: u64, cache: u64, dest: u64, bits: u32, scale: u32) -> Result<u32, Error> {
    let delta = i128::from(dest) - i128::from(cache);
    let align = 1i128 << scale;
    let scaled = delta / align;
    if delta % align != 0 || scaled < -(1i128 << (bits - 1)) || scaled >= (1i128 << (bits - 1)) {
        return Err(Error::Relocation { pc });
    }
    Ok((scaled as u32) & ((1u32 << bits) - 1))
}

pub fn translate(source: &[u8], pc: u64, cache: u64) -> Result<Block<Terminal>, Error> {
    if !pc.is_multiple_of(4) || !cache.is_multiple_of(4) {
        return Err(Error::Range);
    }
    let mut bytes = Vec::new();
    let mut maps = Vec::new();
    for n in 0..256usize {
        let at = pc.checked_add((n * 4) as u64).ok_or(Error::Range)?;
        let data = source
            .get(n * 4..n * 4 + 4)
            .ok_or(Error::Truncated { pc: at })?;
        let w = u32::from_le_bytes(data.try_into().unwrap());
        let emitted = cache.checked_add(bytes.len() as u64).ok_or(Error::Range)?;
        let source_end = at.checked_add(4).ok_or(Error::Range)?;
        let cache_end = emitted.checked_add(4).ok_or(Error::Range)?;
        let terminal = if w & 0x7c00_0000 == 0x1400_0000 {
            Some(Terminal::Direct {
                target: target(at, sext(w & 0x03ff_ffff, 26) * 4),
                link: w >> 31 != 0,
            })
        } else if w & 0xffff_fc1f == 0xd61f_0000
            || w & 0xffff_fc1f == 0xd63f_0000
            || w & 0xffff_fc1f == 0xd65f_0000
        {
            Some(Terminal::Register {
                register: ((w >> 5) & 31) as u8,
                link: w & 0xffff_fc1f == 0xd63f_0000,
            })
        } else if w & 0xff00_0010 == 0x5400_0000 {
            Some(Terminal::Condition {
                target: target(at, sext((w >> 5) & 0x7ffff, 19) * 4),
                condition: (w & 15) as u8,
            })
        } else if w & 0x7e00_0000 == 0x3400_0000 {
            Some(Terminal::Compare {
                target: target(at, sext((w >> 5) & 0x7ffff, 19) * 4),
                register: (w & 31) as u8,
                nonzero: w & (1 << 24) != 0,
                wide: w >> 31 != 0,
            })
        } else if w & 0x7e00_0000 == 0x3600_0000 {
            Some(Terminal::Test {
                target: target(at, sext((w >> 5) & 0x3fff, 14) * 4),
                register: (w & 31) as u8,
                bit: (((w >> 26) & 32) | ((w >> 19) & 31)) as u8,
                nonzero: w & (1 << 24) != 0,
            })
        } else {
            None
        };
        maps.push(PcMap {
            cache: emitted..cache_end,
            source: at,
        });
        if let Some(terminal) = terminal {
            bytes.extend(TRAP.to_le_bytes());
            return Ok(Block {
                bytes,
                maps,
                source: pc..source_end,
                instructions: n as u32 + 1,
                terminal,
                trap: emitted,
            });
        }
        let relocated = if w & 0x9f00_0000 == 0x1000_0000 {
            // ADR
            let imm = sext(((w >> 5) & 0x7ffff) << 2 | ((w >> 29) & 3), 21);
            let r = relative(at, emitted, target(at, imm), 21, 0)?;
            (w & 0x9f00_001f) | ((r & 3) << 29) | ((r >> 2) << 5)
        } else if w & 0x9f00_0000 == 0x9000_0000 {
            // ADRP
            let imm = sext(((w >> 5) & 0x7ffff) << 2 | ((w >> 29) & 3), 21) * 4096;
            let r = relative(at, emitted & !4095, target(at & !4095, imm), 21, 12)?;
            (w & 0x9f00_001f) | ((r & 3) << 29) | ((r >> 2) << 5)
        } else if w & 0xff00_0000 == 0x5800_0000 {
            // LDR Xt, literal
            let dest = target(at, sext((w >> 5) & 0x7ffff, 19) * 4);
            (w & 0xff00_001f) | (relative(at, emitted, dest, 19, 2)? << 5)
        } else if ordinary(w) {
            w
        } else {
            return Err(Error::Unsupported { pc: at });
        };
        bytes.extend(relocated.to_le_bytes());
    }
    Err(Error::Unsupported { pc })
}
fn ordinary(w: u32) -> bool {
    w == 0xd503_201f // NOP
        || w & 0x1f80_0000 == 0x1100_0000 // ADD/SUB immediate, including flags/SP
        || (w & 0x1f80_0000 == 0x1280_0000 && (w>>29)&3 != 1 && (w>>31!=0 || (w>>21)&3<2)) // move wide
        || w & 0xffe0_ffe0 == 0xaa00_03e0 // MOV Xd, Xm (ORR Xd, XZR, Xm)
        || w & 0xffc0_0000 == 0xf940_0000 // LDR Xt, [Xn|SP, #imm]
        || w & 0xffc0_0000 == 0x3dc0_0000 // LDR Qt, [Xn|SP, #imm]
        || w & 0xffe0_fc00 == 0x4ee0_8400 // ADD Vd.2D, Vn.2D, Vm.2D
}

pub fn condition(nzcv: u64, code: u8) -> bool {
    let n = nzcv & (1 << 31) != 0;
    let z = nzcv & (1 << 30) != 0;
    let c = nzcv & (1 << 29) != 0;
    let v = nzcv & (1 << 28) != 0;
    let base = match code >> 1 {
        0 => z,
        1 => c,
        2 => n,
        3 => v,
        4 => c && !z,
        5 => n == v,
        6 => n == v && !z,
        _ => true,
    };
    if code & 1 != 0 && code != 15 {
        !base
    } else {
        base
    }
}
/// Control transfer at the trap; LR always contains an original application PC.
pub fn dispatch(t: Terminal, fallthrough: u64, regs: &mut [u64; 31], nzcv: u64) -> u64 {
    let read = |r: u8| if r == 31 { 0 } else { regs[r as usize] };
    let (next, link) = match t {
        Terminal::Direct { target, link } => (target, link),
        Terminal::Register { register, link } => (read(register), link),
        Terminal::Condition {
            target,
            condition: c,
        } => (
            if condition(nzcv, c) {
                target
            } else {
                fallthrough
            },
            false,
        ),
        Terminal::Compare {
            target,
            register,
            nonzero,
            wide,
        } => {
            let v = if wide {
                read(register)
            } else {
                read(register) & 0xffff_ffff
            };
            (
                if (v != 0) == nonzero {
                    target
                } else {
                    fallthrough
                },
                false,
            )
        }
        Terminal::Test {
            target,
            register,
            bit,
            nonzero,
        } => (
            if ((read(register) >> bit) & 1 != 0) == nonzero {
                target
            } else {
                fallthrough
            },
            false,
        ),
    };
    if link {
        regs[30] = fallthrough;
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
    #[test]
    fn rejects_wrapping_source_and_cache_ranges() {
        let code = words(&[0xd65f03c0]);
        assert!(matches!(
            translate(&code, u64::MAX - 3, 0),
            Err(Error::Range)
        ));
        assert!(matches!(
            translate(&code, 0, u64::MAX - 3),
            Err(Error::Range)
        ));
    }
    #[test]
    fn copies_native_loop_and_dispatches_backward_branch() {
        let b = translate(&words(&[0x91000400, 0x17ffffff]), 0x1000, 0x2000).unwrap();
        assert_eq!(&b.bytes[..4], &0x91000400u32.to_le_bytes());
        assert_eq!(b.instructions, 2);
        assert_eq!(
            b.terminal,
            Terminal::Direct {
                target: 0x1000,
                link: false
            }
        );
        assert_eq!(b.source_pc(0x2000), Some(0x1000));
        assert_eq!(b.source_pc(b.trap), Some(0x1004));
    }
    #[test]
    fn relocates_adr_adrp_literal_and_rejects_out_of_reach() {
        for instruction in [0x10000001, 0x90000001, 0x58000001] {
            let source = words(&[instruction, 0xd65f03c0]);
            let b = translate(&source, 0x4000, 0x8000).unwrap();
            let back = translate(
                &b.bytes[..4]
                    .iter()
                    .copied()
                    .chain(0xd65f03c0u32.to_le_bytes())
                    .collect::<Vec<_>>(),
                0x8000,
                0x4000,
            )
            .unwrap();
            assert_eq!(&back.bytes[..4], &source[..4]);
        }
        assert!(matches!(
            translate(&words(&[0x10000001, 0xd65f03c0]), 0x4000, 0x1_0000_0000),
            Err(Error::Relocation { .. })
        ));
    }
    #[test]
    fn rejects_exclusives_atomics_syscalls_unknown_and_truncated() {
        for w in [
            0xc85f7c00, 0xc8007c00, 0xc87f0000, 0xc8a07c41, 0xf8200020, 0xd5033f5f, 0xd4000001, 0,
            TRAP,
        ] {
            assert!(matches!(
                translate(&words(&[w]), 0, 0),
                Err(Error::Unsupported { .. })
            ));
        }
        assert!(matches!(
            translate(&[0, 1, 2], 0, 0),
            Err(Error::Truncated { .. })
        ));
    }
    #[test]
    fn original_link_and_indirect_return_and_flags() {
        let mut r = [0; 31];
        assert_eq!(
            dispatch(
                Terminal::Direct {
                    target: 0x2000,
                    link: true
                },
                0x1004,
                &mut r,
                0
            ),
            0x2000
        );
        assert_eq!(r[30], 0x1004);
        assert_eq!(
            dispatch(
                Terminal::Register {
                    register: 30,
                    link: false
                },
                0,
                &mut r,
                0
            ),
            0x1004
        );
        for flags in 0..16 {
            for c in 0..14 {
                assert_ne!(condition(flags << 28, c), condition(flags << 28, c ^ 1));
            }
        }
    }
}
