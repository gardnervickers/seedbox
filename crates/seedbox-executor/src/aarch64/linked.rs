//! Task-bound experimental linked A64 emitter. No application register is
//! reserved and no application stack slot is borrowed. TPIDR_EL0 holds X16
//! transiently during the seven-instruction spill; the private state's `tls`
//! retains application TLS throughout. Linux may suspend this task anywhere:
//! it must resume the same internal PC/state, or reconstruct through these maps.
//! Arbitrary application signal delivery and shared multi-task caches are not
//! implemented by this emitter.
use super::{relative, translate, Terminal};
use crate::{meter::Phase, Error};
use alloc::vec::Vec;

pub mod region;

/// Private, resident memory belonging to exactly one execution task. The caller
/// must keep this allocation and the original TLS value stable while code lives.
#[repr(C)]
#[derive(Debug, Default)]
pub struct State {
    pub x16: u64,
    pub x17: u64,
    pub tls: u64,
    pub left: i64,
    pub gateway_pc: u64,
    pub gateway_charge: u64,
    pub target: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Saved {
    Native,
    X16InTls,
    X16InTlsX17InState,
    BothInState,
    X16InState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Map {
    pub pc: u64,
    pub source: u64,
    pub saved: Saved,
    pub phase: Phase,
    pub instrumentation: bool,
}
impl Map {
    /// Reconstruct logical values without modifying the hardware continuation.
    /// A synchronous fault at instrumentation is an executor failure, NOT an
    /// application fault. A test trap may inspect and then resume that exact PC.
    pub fn reconstruct(&self, regs: &mut [u64; 31], hardware_tls: u64, state: &State) -> u64 {
        match self.saved {
            Saved::Native => (),
            Saved::X16InTls => regs[16] = hardware_tls,
            Saved::X16InTlsX17InState => {
                regs[16] = hardware_tls;
                regs[17] = state.x17;
            }
            Saved::BothInState => {
                regs[16] = state.x16;
                regs[17] = state.x17;
            }
            Saved::X16InState => regs[16] = state.x16,
        }
        state.tls
    }
}
#[derive(Debug)]
pub struct LinkedBlock {
    pub bytes: Vec<u8>,
    pub maps: Vec<Map>,
    pub source: core::ops::Range<u64>,
    pub instructions: u32,
    /// Entry with X16 holding State and both original scratch values saved.
    /// Baseline blocks have no such entry.
    pub carried_entry: Option<u64>,
}
impl LinkedBlock {
    pub fn map(&self, pc: u64) -> Option<Map> {
        self.maps.iter().find(|m| m.pc == pc).copied()
    }
}
/// An original entry and its stable execution gate. Revoking that gate before
/// source mutation revokes BOTH direct incoming edges and indirect cached hits.
/// Gates are never reused for a different original entry. A generation pin does
/// not license execution through a revoked gate.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub source: u64,
    pub gate: u64,
}
struct Emit {
    base: u64,
    words: Vec<u32>,
    maps: Vec<Map>,
    literals: Vec<(usize, u64)>,
    source: u64,
    saved: Saved,
    phase: Phase,
}
impl Emit {
    fn pc(&self) -> u64 {
        self.base + self.words.len() as u64 * 4
    }
    fn word(&mut self, word: u32, instrumentation: bool) -> usize {
        let index = self.words.len();
        self.maps.push(Map {
            pc: self.pc(),
            source: self.source,
            saved: self.saved,
            phase: self.phase,
            instrumentation,
        });
        self.words.push(word);
        index
    }
    fn ins(&mut self, word: u32) -> usize {
        self.word(word, true)
    }
    fn literal(&mut self, register: u32, value: u64) {
        let index = self.ins(0x58000000 | register);
        self.literals.push((index, value));
    }
    fn branch(&mut self, destination: u64) -> Result<(), Error> {
        let word = branch(self.pc(), destination)?;
        self.ins(word);
        Ok(())
    }
    fn patch19(&mut self, index: usize, dest: u64) -> Result<(), Error> {
        let pc = self.base + index as u64 * 4;
        self.words[index] |= relative(pc, pc, dest, 19, 2)? << 5;
        Ok(())
    }
    fn spill(&mut self, state: u64) {
        self.ins(0xd51bd050); // MSR TPIDR_EL0,X16
        self.saved = Saved::X16InTls;
        self.literal(16, state);
        self.ins(0xf9000611); // STR X17,[X16,#8]
        self.saved = Saved::X16InTlsX17InState;
        self.ins(0xd53bd051); // MRS X17,TPIDR_EL0
        self.ins(0xf9000211); // STR X17,[X16]
        self.saved = Saved::BothInState;
        self.ins(0xf9400a11); // LDR X17,[X16,#16]
        self.ins(0xd51bd051); // MSR TPIDR_EL0,X17
    }
    fn restore(&mut self) {
        self.ins(0xf9400611); // LDR X17,[X16,#8]
        self.saved = Saved::X16InState;
        self.ins(0xf9400210); // LDR X16,[X16]
        self.saved = Saved::Native;
    }
    fn cold(&mut self, source: u64, charge: u32, gateway: u64) -> Result<(), Error> {
        self.literal(17, source);
        self.ins(0xf9001211); // STR X17,[X16,#32]
        self.literal(17, charge.into());
        self.ins(0xf9001611); // STR X17,[X16,#40]
        self.branch(gateway)
    }
    fn finish(mut self) -> Result<(Vec<u8>, Vec<Map>), Error> {
        for (index, value) in self.literals {
            let dest = self
                .base
                .checked_add(self.words.len() as u64 * 4)
                .ok_or(Error::Range)?;
            let pc = self.base + index as u64 * 4;
            self.words[index] |= relative(pc, pc, dest, 19, 2)? << 5;
            self.words.push(value as u32);
            self.words.push((value >> 32) as u32);
        }
        Ok((
            self.words.iter().flat_map(|w| w.to_le_bytes()).collect(),
            self.maps,
        ))
    }
}
pub fn branch(pc: u64, dest: u64) -> Result<u32, Error> {
    Ok(0x14000000 | relative(pc, pc, dest, 26, 2)?)
}

/// Every terminal resolves to a stable gate, never a generation's raw cache PC.
/// Indirect hits use a finite fully-tagged inline cache, with a cold miss gate.
/// Callers may rebuild this cache at a stopped ownership boundary as targets are
/// discovered. No miss escapes to untranslated application code.
pub fn emit(
    source: &[u8],
    pc: u64,
    cache: u64,
    state: u64,
    gateway: u64,
    targets: &[Target],
) -> Result<LinkedBlock, Error> {
    emit_with_return_hint(source, pc, cache, state, gateway, targets, None)
}

/// Optional fully guarded RET-X30 fast arm. The hint is an original address,
/// never execution authority: compare the actual LR and enter its stable gate.
/// A miss retains the complete finite target lookup, including fail-closed
/// unknown targets. Charge points, scratch ownership and due paths are unchanged.
pub fn emit_with_return_hint(
    source: &[u8],
    pc: u64,
    cache: u64,
    state: u64,
    gateway: u64,
    targets: &[Target],
    return_hint: Option<u64>,
) -> Result<LinkedBlock, Error> {
    // Includes the maximum native body, veneers and literal pool. Bound the
    // entire emission, not just the fixed header, before computing cache PCs.
    let maximum = targets
        .len()
        .checked_mul(64)
        .and_then(|n| n.checked_add(2048))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(Error::Range)?;
    if !state.is_multiple_of(8) || !cache.is_multiple_of(4) || cache.checked_add(maximum).is_none()
    {
        return Err(Error::Range);
    }
    // Spill + load/sub/store/sign-test + restore = 13 instructions.
    let body = cache + 13 * 4;
    let block = translate(source, pc, body)?;
    let charge = block.instructions;
    let mut e = Emit {
        base: cache,
        words: Vec::new(),
        maps: Vec::new(),
        literals: Vec::new(),
        source: pc,
        saved: Saved::Native,
        phase: Phase::Ready,
    };
    e.spill(state);
    e.ins(0xf9400e11); // LDR X17,[X16,#24]
    e.ins(0xd1000231 | (charge << 10)); // SUB X17,X17,#charge (NZCV untouched)
    e.ins(0xf9000e11); // STR X17,[X16,#24]: debit commits here
    e.phase = Phase::Debited {
        instructions: charge,
    };
    let due = e.ins(0xb7f80011); // TBNZ X17,#63,due (14-bit displacement)
    e.phase = Phase::Running {
        instructions: charge,
    };
    e.restore();
    debug_assert_eq!(e.pc(), body);
    for (n, word) in block.bytes[..block.bytes.len() - 4]
        .as_chunks::<4>()
        .0
        .iter()
        .enumerate()
    {
        e.source = pc + n as u64 * 4;
        e.word(u32::from_le_bytes(*word), false);
    }
    e.source = block.source.end - 4;
    let gate = |source| {
        targets
            .iter()
            .find(|t| t.source == source)
            .map(|t| t.gate)
            .ok_or(Error::Unsupported { pc: source })
    };
    let direct = |e: &mut Emit, target, link| -> Result<(), Error> {
        if link {
            e.literal(30, block.source.end);
        }
        e.source = target;
        e.phase = Phase::Ready;
        e.branch(gate(target)?)
    };
    match block.terminal {
        Terminal::Direct { target, link } => direct(&mut e, target, link)?,
        Terminal::Register { register, link } => {
            e.spill(state);
            let terminal_offset = (block.source.end - pc - 4) as usize;
            let is_ret_x30 =
                source[terminal_offset..terminal_offset + 4] == 0xd65f03c0u32.to_le_bytes();
            if let Some(hint) = return_hint.filter(|_| is_ret_x30) {
                e.literal(17, hint);
                e.ins(0xca1e0231); // EOR X17,X17,X30: compare actual original LR, NZCV intact
                let miss = e.ins(0xb5000011); // CBNZ X17,full tagged lookup
                e.restore();
                e.source = hint;
                e.phase = Phase::Ready;
                e.branch(gate(hint)?)?;
                e.source = block.source.end - 4;
                e.phase = Phase::Running {
                    instructions: charge,
                };
                e.saved = Saved::BothInState;
                e.patch19(miss, e.pc())?;
            }
            match register {
                16 => {
                    e.ins(0xf9400211);
                }
                17 => {
                    e.ins(0xf9400611);
                }
                r => {
                    e.ins(0xaa0003f1 | (u32::from(r) << 16));
                }
            }
            e.ins(0xf9001a11); // STR X17,[X16,#48]: original target, before LR changes
            for t in targets {
                e.literal(17, t.source);
                // Temporarily use X16 as target value, both originals are in state.
                e.ins(0xf9401a10); // LDR X16,[X16,#48]
                e.ins(0xca100231); // EOR X17,X17,X16 (does not change flags)
                e.literal(16, state);
                let miss = e.ins(0xb5000011); // CBNZ X17,next
                e.restore();
                if link {
                    e.literal(30, block.source.end);
                }
                e.source = t.source;
                e.phase = Phase::Ready;
                e.branch(t.gate)?;
                e.source = block.source.end - 4;
                e.phase = Phase::Running {
                    instructions: charge,
                };
                e.saved = Saved::BothInState;
                e.patch19(miss, e.pc())?;
            }
            // Charge zero identifies a terminal miss, not another precharge.
            e.cold(block.source.end - 4, 0, gateway)?;
        }
        terminal => {
            let (target, word) = match terminal {
                Terminal::Condition { target, condition } => {
                    (target, 0x54000000 | u32::from(condition))
                }
                Terminal::Compare {
                    target,
                    register,
                    nonzero,
                    wide,
                } => (
                    target,
                    0x34000000
                        | (u32::from(wide) << 31)
                        | (u32::from(nonzero) << 24)
                        | u32::from(register),
                ),
                Terminal::Test {
                    target,
                    register,
                    bit,
                    nonzero,
                } => (
                    target,
                    0x36000000
                        | (u32::from(bit & 32) << 26)
                        | (u32::from(bit & 31) << 19)
                        | (u32::from(nonzero) << 24)
                        | u32::from(register),
                ),
                _ => unreachable!(),
            };
            // Conditional target is two instructions away (branch + fallthrough B).
            e.ins(word | (2 << 5));
            direct(&mut e, block.source.end, false)?;
            direct(&mut e, target, false)?;
        }
    }
    let due_pc = e.pc();
    let due_at = cache + due as u64 * 4;
    e.words[due] |= relative(due_at, due_at, due_pc, 14, 2)? << 5;
    e.source = pc;
    e.saved = Saved::BothInState;
    e.phase = Phase::Debited {
        instructions: charge,
    };
    e.cold(pc, charge, gateway)?;
    let (bytes, maps) = e.finish()?;
    Ok(LinkedBlock {
        bytes,
        maps,
        source: block.source,
        instructions: charge,
        carried_entry: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
    #[test]
    fn rejects_wrapping_emission_and_missing_direct_gate() {
        assert!(matches!(
            emit(
                &words(&[0xd65f03c0]),
                0x1000,
                u64::MAX - 3,
                0x8000,
                0x3000,
                &[]
            ),
            Err(Error::Range)
        ));
        assert!(matches!(
            emit(&words(&[0x14000000]), 0x1000, 0x2000, 0x8000, 0x3000, &[]),
            Err(Error::Unsupported { pc: 0x1000 })
        ));
    }
    #[test]
    fn conditional_edges_preserve_test_operands_and_map_both_successors() {
        let targets = [
            Target {
                source: 0x1004,
                gate: 0x4000,
            },
            Target {
                source: 0x1008,
                gate: 0x5000,
            },
        ];
        for word in [0x54000041, 0xb4000050, 0x35000051, 0xb7f80051, 0x36000050] {
            let b = emit(&words(&[word]), 0x1000, 0x2000, 0x8000, 0x3000, &targets).unwrap();
            assert_eq!(&b.bytes[52..56], &word.to_le_bytes());
            assert_eq!(b.maps[13].phase, Phase::Running { instructions: 1 });
            for (i, t) in targets.iter().enumerate() {
                assert_eq!(b.maps[14 + i].phase, Phase::Ready);
                assert_eq!(b.maps[14 + i].source, t.source);
                assert_eq!(b.maps[14 + i].saved, Saved::Native);
            }
        }
    }
    #[test]
    fn linked_meter_has_exact_commit_maps_and_no_normal_trap() {
        let b = emit(
            &words(&[0x91000400, 0x17ffffff]),
            0x1000,
            0x2000,
            0x8000,
            0x3000,
            &[Target {
                source: 0x1000,
                gate: 0x4000,
            }],
        )
        .unwrap();
        assert_eq!(b.instructions, 2);
        assert_eq!(b.maps[0].saved, Saved::Native);
        assert_eq!(b.maps[1].saved, Saved::X16InTls);
        assert_eq!(b.maps[3].saved, Saved::X16InTlsX17InState);
        assert_eq!(b.maps[5].saved, Saved::BothInState);
        assert_eq!(b.maps[9].phase, Phase::Ready);
        assert_eq!(b.maps[10].phase, Phase::Debited { instructions: 2 });
        assert_eq!(b.maps[11].phase, Phase::Running { instructions: 2 });
        assert_eq!(b.maps[12].saved, Saved::X16InState);
        assert_eq!(b.maps[13].saved, Saved::Native);
        for map in &b.maps {
            let offset = (map.pc - 0x2000) as usize;
            let w = u32::from_le_bytes(b.bytes[offset..offset + 4].try_into().unwrap());
            assert_ne!(w & 0xffe0001f, 0xd4200000, "no BRK");
            assert_ne!(w, 0xd4000001, "no syscall");
        }
    }
    #[test]
    fn every_spill_stage_reconstructs_scratch_and_tls() {
        let state = State {
            x16: 16,
            x17: 17,
            tls: 0x1234,
            ..State::default()
        };
        for saved in [
            Saved::Native,
            Saved::X16InTls,
            Saved::X16InTlsX17InState,
            Saved::BothInState,
            Saved::X16InState,
        ] {
            let mut regs = [0; 31];
            regs[16] = if saved == Saved::Native { 16 } else { 999 };
            regs[17] = if matches!(saved, Saved::Native | Saved::X16InTls | Saved::X16InState) {
                17
            } else {
                999
            };
            let map = Map {
                pc: 0,
                source: 0,
                saved,
                phase: Phase::Ready,
                instrumentation: true,
            };
            assert_eq!(map.reconstruct(&mut regs, 16, &state), 0x1234);
            assert_eq!((regs[16], regs[17]), (16, 17));
        }
    }
    #[test]
    fn guarded_return_hint_preserves_meter_and_falls_back_with_original_scratch() {
        let targets = [
            Target {
                source: 0x1000,
                gate: 0x4000,
            },
            Target {
                source: 0x1004,
                gate: 0x4004,
            },
        ];
        let source = words(&[0x91000400, 0xd65f03c0]);
        let baseline = emit(&source, 0x1000, 0x2000, 0x8000, 0x3000, &targets).unwrap();
        let disabled =
            emit_with_return_hint(&source, 0x1000, 0x2000, 0x8000, 0x3000, &targets, None).unwrap();
        assert_eq!(baseline.bytes, disabled.bytes);
        assert_eq!(baseline.maps, disabled.maps);
        let b = emit_with_return_hint(
            &source,
            0x1000,
            0x2000,
            0x8000,
            0x3000,
            &targets,
            Some(0x1004),
        )
        .unwrap();
        assert_eq!(b.instructions, baseline.instructions);
        assert_eq!(&b.maps[..14], &baseline.maps[..14]);
        let ws: Vec<u32> = b
            .bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        let compare = ws.iter().position(|w| *w == 0xca1e0231).unwrap();
        assert_eq!(b.maps[compare].saved, Saved::BothInState);
        assert_eq!(b.maps[compare].phase, Phase::Running { instructions: 2 });
        let miss = ws[compare + 1];
        assert_eq!(miss & 0xff00001f, 0xb5000011);
        let fallback = compare + 1 + ((miss >> 5) & 0x7ffff) as usize;
        assert_eq!(ws[fallback], 0xaa1e03f1, "capture real X30 on miss");
        assert_eq!(b.maps[fallback].saved, Saved::BothInState);
        assert_eq!(b.maps[fallback].phase, Phase::Running { instructions: 2 });
        assert_eq!(b.maps[compare + 4].source, 0x1004);
        assert_eq!(b.maps[compare + 4].phase, Phase::Ready);
        assert_eq!(
            ws[compare + 4],
            branch(b.maps[compare + 4].pc, 0x4004).unwrap()
        );
        assert!(b
            .maps
            .iter()
            .rev()
            .take(5)
            .all(|m| m.phase == Phase::Debited { instructions: 2 }));
        assert!(matches!(
            emit_with_return_hint(
                &source,
                0x1000,
                0x2000,
                0x8000,
                0x3000,
                &targets,
                Some(0xdead)
            ),
            Err(Error::Unsupported { pc: 0xdead })
        ));
    }
    #[test]
    fn return_hint_does_not_change_other_register_transfers() {
        let targets = [Target {
            source: 0x1000,
            gate: 0x4000,
        }];
        for word in [0xd61f03c0, 0xd63f03c0, 0xd65f0200, 0xd65f0220] {
            let source = words(&[word]);
            let a = emit(&source, 0x1000, 0x2000, 0x8000, 0x3000, &targets).unwrap();
            let b = emit_with_return_hint(
                &source,
                0x1000,
                0x2000,
                0x8000,
                0x3000,
                &targets,
                Some(0x1000),
            )
            .unwrap();
            assert_eq!(a.bytes, b.bytes);
            assert_eq!(a.maps, b.maps);
        }
    }
    #[test]
    fn return_cache_uses_original_target_and_revocable_gates() {
        let targets = [
            Target {
                source: 0x1000,
                gate: 0x4000,
            },
            Target {
                source: 0x1004,
                gate: 0x5000,
            },
        ];
        for register in [0, 16, 17, 30] {
            let b = emit(
                &words(&[0xd63f0000 | (register << 5)]),
                0x1000,
                0x2000,
                0x8000,
                0x3000,
                &targets,
            )
            .unwrap();
            assert_eq!(b.instructions, 1);
            for t in targets {
                assert!(b.maps.iter().any(|m| {
                    let i = (m.pc - 0x2000) as usize;
                    b.bytes[i..i + 4] == branch(m.pc, t.gate).unwrap().to_le_bytes()
                }));
            }
        }
    }
}
