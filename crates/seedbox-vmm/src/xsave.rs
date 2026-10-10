//! Canonical evidence for the standard-format KVM_GET_XSAVE buffer.
//!
//! XINUSE may mark an initial component present or absent after host FPU
//! save/restore. For YMM_Hi128, both representations restore zero registers.
//! Only normalize the observed YMM ambiguity; do not mask other components or
//! change the live vCPU or the raw state retained by checkpoint save/restore.

const XSTATE_BV: usize = 512 / 4;
const YMM_PRESENT: u32 = 1 << 2;
// Standard-format AVX component: upper 128 bits of the sixteen YMM registers.
const YMM: std::ops::Range<usize> = 576 / 4..832 / 4;

pub(crate) fn canonicalize_ymm(region: &mut [u32; 1024]) {
    if region[XSTATE_BV] & YMM_PRESENT == 0 {
        // XRSTOR ignores these bytes when the component is absent.
        region[YMM].fill(0);
    } else if region[YMM].iter().all(|&word| word == 0) {
        region[XSTATE_BV] &= !YMM_PRESENT;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_stale_and_present_zero_ymm_have_one_representation() {
        let mut absent = [0; 1024];
        absent[XSTATE_BV] = 0x202;
        let mut stale = absent;
        stale[YMM].fill(u32::MAX);
        let mut present = absent;
        present[XSTATE_BV] |= YMM_PRESENT;
        for state in [&mut absent, &mut stale, &mut present] {
            canonicalize_ymm(state);
        }
        assert_eq!(absent, stale);
        assert_eq!(absent, present);
    }

    #[test]
    fn every_nonzero_ymm_bit_remains_distinguishable() {
        for word in YMM {
            for bit in 0..32 {
                let mut state = [0; 1024];
                state[XSTATE_BV] = YMM_PRESENT;
                state[word] = 1 << bit;
                let before = state;
                canonicalize_ymm(&mut state);
                assert_eq!(state, before, "word {word}, bit {bit}");
            }
        }
    }

    #[test]
    fn normalization_preserves_all_other_bytes_and_is_idempotent() {
        let mut state = [0xa5a5_a5a5; 1024];
        state[YMM].fill(0);
        let mut expected = state;
        expected[XSTATE_BV] &= !YMM_PRESENT;
        canonicalize_ymm(&mut state);
        assert_eq!(state, expected);
        canonicalize_ymm(&mut state);
        assert_eq!(state, expected);
    }
}
