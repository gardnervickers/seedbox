//! Stable raw Linux A64 payment gateway. It changes only X0, X8, X17 and
//! private State; X16 already points to State. SP, LR, TLS, NZCV and FP/SIMD
//! remain untouched. Linux SVC preserves all but the result X0. No C/Rust call
//! or application-stack frame is needed. This is NOT an arbitrary syscall ABI.
//!
//! No body continuation is retained: after payment, refund the captured OLD
//! charge exactly once and branch to the stable carried entry gate. A stopped
//! publisher can revoke that gate; the cold resolver then reconstructs and
//! translates current source. The owner must forbid translated reentry while
//! this private transaction is active, and retain any old diagnostic map pins
//! independently of gate permission. Host-only suspension resumes this exact
//! hardware PC/state, including the post-SVC/pre-commit interval.
use super::*;
use crate::meter::{DEFAULT_BUDGET, PROGRESS_MARK};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Debited,
    /// Syscall completed; quantum not yet committed to State.left.
    Returned,
    /// Quantum committed; payment counter not yet committed.
    BudgetCommitted,
    PaymentCommitted,
    Refunded,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observe {
    Enter,
    Returned,
    Paid,
    Refunded,
}
#[derive(Clone, Copy, Debug)]
pub struct GatewayMap {
    pub pc: u64,
    pub stage: Stage,
    pub x0_saved: bool,
    pub x8_saved: bool,
    pub observe: Option<Observe>,
}
impl GatewayMap {
    pub fn reconstruct(&self, regs: &mut [u64; 31], state: &State) {
        regs[16] = state.x16;
        regs[17] = state.x17;
        if self.x0_saved {
            regs[0] = state.x0;
        }
        if self.x8_saved {
            regs[8] = state.x8;
        }
    }
}
pub struct Gateway {
    pub bytes: Vec<u8>,
    pub maps: Vec<GatewayMap>,
}
impl Gateway {
    pub fn map(&self, pc: u64) -> Option<GatewayMap> {
        self.maps.iter().find(|m| m.pc == pc).copied()
    }
}
/// `observe` adds persistent diagnostic traps around the SAME raw syscall and
/// commits. Timing emission contains none. All maps denote pre-instruction state.
pub fn emit(base: u64, observe: bool) -> Result<Gateway, Error> {
    if !base.is_multiple_of(4) || base.checked_add(512).is_none() {
        return Err(Error::Range);
    }
    let mut words = Vec::new();
    let mut maps = Vec::new();
    let mut map = GatewayMap {
        pc: base,
        stage: Stage::Debited,
        x0_saved: false,
        x8_saved: false,
        observe: None,
    };
    let push = |w, map: &mut GatewayMap, words: &mut Vec<u32>, maps: &mut Vec<GatewayMap>| {
        map.pc = base + words.len() as u64 * 4;
        maps.push(*map);
        words.push(w);
    };
    macro_rules! ins {
        ($w:expr) => {
            push($w, &mut map, &mut words, &mut maps)
        };
    }
    macro_rules! obs {
        ($v:ident) => {
            if observe {
                map.observe = Some(Observe::$v);
                ins!(crate::aarch64::TRAP);
                map.observe = None;
            }
        };
    }
    ins!(0xf9401611); // Charge zero is an unresolved terminal, never a payment.
    let terminal_miss = words.len();
    ins!(0xb4000011); // CBZ X17,error (fail closed)
    ins!(0xf9001e00); // STR X0,[X16,#56]
    map.x0_saved = true;
    ins!(0xf9002208); // STR X8,[X16,#64]
    map.x8_saved = true;
    let repeat = words.len();
    ins!(0xd2800000 | ((PROGRESS_MARK as u32 & 0xffff) << 5));
    ins!(0xf2a00000 | (((PROGRESS_MARK >> 16) as u32 & 0xffff) << 5));
    ins!(0xd2801588); // MOV X8,#172 (getpid)
    obs!(Enter);
    ins!(0xd4000001); // SVC #0: the actual bounded PV operation
    map.stage = Stage::Returned;
    obs!(Returned);
    // A failed syscall cannot be treated as a payment. getpid has no EINTR
    // return; unexpected negative results take a fatal unmapped helper trap.
    let error = words.len();
    ins!(0xb7f80000); // TBNZ X0,#63,error
    ins!(0xf9400e11); // LDR X17,left
    ins!(0xd2800008 | (DEFAULT_BUDGET << 5));
    ins!(0x8b080231); // ADD X17,X17,X8, NZCV untouched
    ins!(0xf9000e11); // Commit quantum
    map.stage = Stage::BudgetCommitted;
    ins!(0xf9402608); // LDR X8,payments
    ins!(0x91000508); // ADD X8,X8,#1
    ins!(0xf9002608); // Commit count
    map.stage = Stage::PaymentCommitted;
    obs!(Paid);
    let back = words.len();
    ins!(0xb7f80011); // TBNZ X17,#63,repeat
    let at = base + back as u64 * 4;
    words[back] |= relative(at, at, base + repeat as u64 * 4, 14, 2)? << 5;
    // The next call starts with a new unpaid negative balance. The same mapped
    // repeat instructions are Debited irrespective of earlier payments.
    ins!(0xf9401608); // LDR X8,old charge
    ins!(0x8b080231); // ADD X17,X17,X8
    ins!(0xf9000e11); // Refund commit, never repeated on this continuation
    map.stage = Stage::Refunded;
    obs!(Refunded);
    ins!(0xf9401e00); // Restore original X0
    map.x0_saved = false;
    ins!(0xf9402208); // Restore original X8
    map.x8_saved = false;
    ins!(0xf9402a11); // LDR X17,stable carried gate
    ins!(0xd61f0220); // BR X17, not a stale body
    let at = base + error as u64 * 4;
    words[error] |= relative(at, at, base + words.len() as u64 * 4, 14, 2)? << 5;
    let at = base + terminal_miss as u64 * 4;
    words[terminal_miss] |= relative(at, at, base + words.len() as u64 * 4, 19, 2)? << 5;
    // Deliberately not a mapped recovery point: this is a fatal runtime failure.
    words.push(crate::aarch64::TRAP);
    Ok(Gateway {
        bytes: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        maps,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn raw_instruction_execution_matches_independent_payment_and_refund_oracle() {
        // Small test-only interpreter for the emitted gateway, not application
        // execution. Unknown opcodes fail the test; no map annotation drives it.
        for (left, charge, failure) in [
            (-1, 1, false),
            (-256, 256, false),
            (-100257, 256, false),
            (-1, 0, false),
            (-1, 3, true),
        ] {
            let g = emit(0x1000, false).unwrap();
            let ws: Vec<u32> = g
                .bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|w| u32::from_le_bytes(*w))
                .collect();
            let original: [u64; 31] = core::array::from_fn(|n| 100 + n as u64);
            let mut r = original;
            let mut mem = [0u64; 11];
            mem[0] = r[16];
            mem[1] = r[17];
            mem[2] = 0xabc;
            mem[3] = left as u64;
            mem[4] = 0x4000;
            mem[5] = charge;
            mem[10] = 0x8000;
            r[16] = 0x9000;
            r[17] = 0;
            let mut i = 0usize;
            let mut calls = 0;
            let mut done = false;
            for _ in 0..200 {
                let w = ws[i];
                if w == crate::aarch64::TRAP {
                    assert!(failure || charge == 0);
                    done = true;
                    break;
                }
                let m = g.maps[i];
                let state = State {
                    x16: mem[0],
                    x17: mem[1],
                    tls: mem[2],
                    left: mem[3] as i64,
                    gateway_pc: mem[4],
                    gateway_charge: mem[5],
                    target: mem[6],
                    x0: mem[7],
                    x8: mem[8],
                    payments: mem[9],
                    gateway_gate: mem[10],
                };
                let mut reconstructed = r;
                m.reconstruct(&mut reconstructed, &state);
                assert_eq!(reconstructed, original, "pc {:#x}", m.pc);
                let rd = (w & 31) as usize;
                let rn = ((w >> 5) & 31) as usize;
                let mut next = i + 1;
                if w & 0xffc00000 == 0xf9000000 {
                    mem[((w >> 10) & 4095) as usize] = r[rd];
                } else if w & 0xffc00000 == 0xf9400000 {
                    r[rd] = mem[((w >> 10) & 4095) as usize];
                } else if w & 0xff800000 == 0xd2800000 {
                    r[rd] = u64::from((w >> 5) & 65535) << (((w >> 21) & 3) * 16);
                } else if w & 0xff800000 == 0xf2800000 {
                    let shift = ((w >> 21) & 3) * 16;
                    r[rd] = (r[rd] & !(65535u64 << shift)) | (u64::from((w >> 5) & 65535) << shift);
                } else if w & 0xff000000 == 0x8b000000 {
                    r[rd] = r[rn].wrapping_add(r[((w >> 16) & 31) as usize]);
                } else if w & 0xff000000 == 0x91000000 {
                    r[rd] = r[rn].wrapping_add(u64::from((w >> 10) & 4095));
                } else if w & 0xff000000 == 0xb7000000 {
                    if r[rd] >> 63 != 0 {
                        let delta = ((((w >> 5) & 16383) as i32) << 18) >> 18;
                        next = (i as i32 + delta) as usize;
                    }
                } else if w & 0xff000000 == 0xb4000000 {
                    if r[rd] == 0 {
                        let delta = ((((w >> 5) & 524287) as i32) << 13) >> 13;
                        next = (i as i32 + delta) as usize;
                    }
                } else if w == 0xd4000001 {
                    assert_eq!(r[0], PROGRESS_MARK);
                    assert_eq!(r[8], 172);
                    calls += 1;
                    r[0] = if failure { u64::MAX } else { 1234 };
                } else if w == 0xd61f0220 {
                    assert_eq!(r[17], 0x8000);
                    done = true;
                    break;
                } else {
                    panic!("unhandled gateway opcode {w:#x}");
                }
                i = next;
            }
            assert!(done);
            if charge == 0 || failure {
                assert_eq!(mem[3] as i64, left);
                assert_eq!(mem[9], 0);
            } else {
                let expected = (-left + 49999) / 50000;
                assert_eq!(calls, expected);
                assert_eq!(mem[9] as i64, expected);
                assert_eq!(mem[3] as i64, left + expected * 50000 + charge as i64);
                assert_eq!(r[0], original[0]);
                assert_eq!(r[8], original[8]);
            }
        }
    }
    #[test]
    fn state_offsets_match_raw_gateway_encoding() {
        assert_eq!(core::mem::offset_of!(State, x0), 56);
        assert_eq!(core::mem::offset_of!(State, x8), 64);
        assert_eq!(core::mem::offset_of!(State, payments), 72);
        assert_eq!(core::mem::offset_of!(State, gateway_gate), 80);
    }
    #[test]
    fn every_partial_save_and_restore_reconstructs_exact_originals() {
        for observe in [false, true] {
            let g = emit(0x1000, observe).unwrap();
            let state = State {
                x0: 100,
                x8: 108,
                x16: 116,
                x17: 117,
                ..State::default()
            };
            for m in &g.maps {
                let mut regs = core::array::from_fn(|n| n as u64 + 100);
                regs[16] = 888;
                regs[17] = 999;
                if m.x0_saved {
                    regs[0] = 999;
                }
                if m.x8_saved {
                    regs[8] = 999;
                }
                m.reconstruct(&mut regs, &state);
                assert_eq!(regs, core::array::from_fn(|n| n as u64 + 100));
            }
            assert_eq!(
                g.maps.iter().filter(|m| m.observe.is_some()).count(),
                if observe { 4 } else { 0 }
            );
            let stages: Vec<_> = g.maps.iter().map(|m| m.stage).collect();
            for s in [
                Stage::Debited,
                Stage::Returned,
                Stage::BudgetCommitted,
                Stage::PaymentCommitted,
                Stage::Refunded,
            ] {
                assert!(stages.contains(&s));
            }
        }
    }
}
