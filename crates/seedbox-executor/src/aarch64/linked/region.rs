//! Scratch-carrying linked ABI. Mechanisms informed by DynamoRIO 11.91.20735
//! `ext/drreg/drreg.c` (restore before reads, respill after writes) and
//! `core/arch/aarch64/emit_utils.c` (explicit IBL entry/exit spill conventions).
//! Independently implemented; no DynamoRIO source is copied.
//!
//! Every block has a native entry and a carried entry. Only the former spills.
//! All outgoing edges use revocable carried gates: X16 is State, X16/X17 app
//! values are in State, TLS is architectural. A source scratch operand causes
//! normalization before that instruction and a fresh spill after it commits.
//! Even dead values survive until their architectural overwrite. Charges are
//! still committed/tested at EACH original block, never combined into traces.
use super::*;
use crate::aarch64::relocate;

/// Whether an admitted nonterminal instruction reads OR writes scalar X16/X17.
/// SIMD register numbers are not scalar operands. Unknown forms conservatively
/// normalize, but are independently rejected by `translate`/`relocate`.
fn scratch_operand(w: u32) -> bool {
    let scratch = |r| r == 16 || r == 17;
    if w == 0xd503201f || w & 0xffe0fc00 == 0x4ee08400 {
        false
    } else if w & 0xffc00000 == 0x3dc00000 {
        scratch((w >> 5) & 31)
    } else if w & 0x1f800000 == 0x11000000 || w & 0xffc00000 == 0xf9400000 {
        scratch(w & 31) || scratch((w >> 5) & 31)
    } else if w & 0xffe0ffe0 == 0xaa0003e0 {
        scratch(w & 31) || scratch((w >> 16) & 31)
    } else if w & 0x1f800000 == 0x12800000
        || w & 0x9f000000 == 0x10000000
        || w & 0x9f000000 == 0x90000000
        || w & 0xff000000 == 0x58000000
    {
        scratch(w & 31)
    } else {
        true
    }
}

/// `targets` names carried gates, not native gates. A gate must preserve the
/// carried convention even on a miss; its resolver reconstructs before entering
/// the native entry. Entry permission and immutable code/map pins are separate.
pub fn emit(
    source: &[u8],
    pc: u64,
    cache: u64,
    state: u64,
    gateway: u64,
    targets: &[Target],
) -> Result<LinkedBlock, Error> {
    let maximum = targets
        .len()
        .checked_mul(64)
        .and_then(|n| n.checked_add(12288))
        .and_then(|n| u64::try_from(n).ok())
        .ok_or(Error::Range)?;
    if !state.is_multiple_of(8) || !cache.is_multiple_of(4) || cache.checked_add(maximum).is_none()
    {
        return Err(Error::Range);
    }
    // Decode at source addresses first; relocate each instruction at its actual
    // emitted PC after any inserted scratch transitions.
    let block = translate(source, pc, pc)?;
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
    let carried_entry = e.pc();
    e.ins(0xf9400e11);
    e.ins(0xd1000231 | (charge << 10));
    e.ins(0xf9000e11);
    e.phase = Phase::Debited {
        instructions: charge,
    };
    let due = e.ins(0xb7f80011);
    e.phase = Phase::Running {
        instructions: charge,
    };
    for n in 0..charge as usize - 1 {
        let w = u32::from_le_bytes(source[n * 4..n * 4 + 4].try_into().unwrap());
        e.source = pc + n as u64 * 4;
        let normalize = scratch_operand(w);
        if normalize {
            e.restore();
        }
        let relocated = relocate(w, e.source, e.pc())?;
        e.word(relocated, false);
        if normalize {
            e.source += 4;
            e.spill(state);
        }
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
            // Common case keeps the actual application target in its own GPR
            // until a match, including BLR X30 before updating original LR.
            // Scratch targets use the saved original, never the State pointer.
            if register == 16 || register == 17 {
                e.ins(if register == 16 {
                    0xf9400211
                } else {
                    0xf9400611
                });
                e.ins(0xf9001a11);
            }
            for t in targets {
                e.literal(17, t.source);
                if register == 16 || register == 17 {
                    e.ins(0xf9401a10);
                    e.ins(0xca100231);
                    e.literal(16, state);
                } else {
                    e.ins(0xca000231 | (u32::from(register) << 16));
                }
                let miss = e.ins(0xb5000011);
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
                e.patch19(miss, e.pc())?;
            }
            if register != 16 && register != 17 {
                e.ins(0xaa0003f1 | (u32::from(register) << 16));
                e.ins(0xf9001a11);
            }
            e.cold(block.source.end - 4, 0, gateway)?;
        }
        terminal => {
            let (target, word, normalize) = match terminal {
                Terminal::Condition { target, condition } => {
                    (target, 0x54000000 | u32::from(condition), false)
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
                    register == 16 || register == 17,
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
                    register == 16 || register == 17,
                ),
                _ => unreachable!(),
            };
            if normalize {
                e.restore();
            }
            // Two independently mapped side exits, each normalizes to carried
            // state before its gate. NZCV is never used by instrumentation.
            let taken = e.ins(word);
            e.source = block.source.end;
            e.phase = Phase::Ready;
            if normalize {
                e.spill(state);
            }
            direct(&mut e, block.source.end, false)?;
            let dest = e.pc();
            let at = cache + taken as u64 * 4;
            let bits = if matches!(terminal, Terminal::Test { .. }) {
                14
            } else {
                19
            };
            e.words[taken] |= relative(at, at, dest, bits, 2)? << 5;
            e.source = target;
            e.saved = if normalize {
                Saved::Native
            } else {
                Saved::BothInState
            };
            if normalize {
                e.spill(state);
            }
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
    // Stable original-entry gate, not a generation-body continuation. The raw
    // gateway refunds this old charge then re-enters CURRENT lookup permission.
    e.literal(17, gate(pc)?);
    e.ins(0xf9002a11); // STR X17,[X16,#80]
    e.cold(pc, charge, gateway)?;
    let (bytes, maps) = e.finish()?;
    Ok(LinkedBlock {
        bytes,
        maps,
        source: block.source,
        instructions: charge,
        carried_entry: Some(carried_entry),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn make(ws: &[u32]) -> LinkedBlock {
        let bytes: Vec<_> = ws.iter().flat_map(|w| w.to_le_bytes()).collect();
        emit(
            &bytes,
            0x1000,
            0x2000,
            0x8000,
            0x3000,
            &[
                Target {
                    source: 0x1000,
                    gate: 0x4000,
                },
                Target {
                    source: 0x1008,
                    gate: 0x4004,
                },
                Target {
                    source: 0x1004,
                    gate: 0x4008,
                },
            ],
        )
        .unwrap()
    }
    #[test]
    fn carried_meter_keeps_exact_charge_and_original_fault_scratch() {
        let b = make(&[0xf9400280, 0x17ffffff]);
        assert_eq!(b.carried_entry, Some(0x201c));
        assert_eq!(b.maps[9].phase, Phase::Ready);
        assert_eq!(b.maps[10].phase, Phase::Debited { instructions: 2 });
        assert_eq!(b.maps[11].source, 0x1000);
        assert!(!b.maps[11].instrumentation);
        assert_eq!(b.maps[11].phase, Phase::Running { instructions: 2 });
        assert_eq!(b.maps[11].saved, Saved::BothInState);
        let mut regs = [999; 31];
        let s = State {
            x16: 16,
            x17: 17,
            tls: 0xabc,
            ..State::default()
        };
        assert_eq!(b.maps[11].reconstruct(&mut regs, 0xabc, &s), 0xabc);
        assert_eq!((regs[16], regs[17]), (16, 17));
    }
    #[test]
    fn scratch_operands_normalize_before_fault_and_respill_only_after_commit() {
        for w in [0xf9400290, 0xf9400200, 0x91000610, 0xd2800031, 0xaa1103f0] {
            let b = make(&[w, 0x17ffffff]);
            let i = b.maps.iter().position(|m| !m.instrumentation).unwrap();
            assert_eq!(i, 13);
            assert_eq!(b.maps[i].saved, Saved::Native);
            assert_eq!(b.maps[i].source, 0x1000);
            assert_eq!(b.maps[i + 1].source, 0x1004);
            assert_eq!(b.maps[i + 1].saved, Saved::Native);
            assert_eq!(b.maps[i + 6].saved, Saved::BothInState);
        }
        assert!(!scratch_operand(0x4ef08610), "SIMD16/17 are not GPR16/17");
        assert!(!scratch_operand(0x3dc00290), "Q16 destination is not X16");
        assert!(scratch_operand(0x3dc00210), "scalar base X16");
    }
    #[test]
    fn scratch_conditional_has_two_carried_side_exits() {
        for word in [0xb4000050, 0xb7f80051] {
            let b = make(&[word, 0x14000000]);
            let exits: Vec<_> = b
                .maps
                .iter()
                .filter(|m| {
                    m.phase == Phase::Ready
                        && m.pc >= 0x2000 + 13 * 4
                        && m.saved == Saved::BothInState
                })
                .collect();
            assert!(exits.iter().any(|m| m.source == 0x1008));
        }
    }
    #[test]
    fn unknown_and_atomic_inputs_never_gain_native_execution() {
        for w in [
            0xc85f7c00u32,
            0xc8007c00,
            0xc8a07c01,
            0xd5033f5f,
            0xd4000001,
        ] {
            assert!(emit(&w.to_le_bytes(), 0x1000, 0x2000, 0x8000, 0x3000, &[]).is_err());
        }
    }
}
