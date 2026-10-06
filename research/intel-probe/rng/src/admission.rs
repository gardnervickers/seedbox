//! Conservative offline candidate finder, deliberately not an x86 decoder.
//! A runtime profile must also pin/control every executable mapping and alias.
pub fn candidates(bytes: &[u8]) -> Vec<usize> {
    bytes
        .windows(3)
        .enumerate()
        .filter_map(|(at, op)| {
            (op[0] == 0x0f && op[1] == 0xc7 && op[2] & 0xf0 == 0xf0).then_some(at)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_register_rng_candidates_are_rejected_including_prefixed_forms() {
        for modrm in 0xf0..=0xff {
            for prefix in [vec![], vec![0x66], vec![0x48], vec![0x49], vec![0x66, 0x41]] {
                let mut code = prefix;
                let at = code.len();
                code.extend([0x0f, 0xc7, modrm]);
                assert_eq!(candidates(&code), vec![at]);
            }
        }
    }
    #[test]
    fn replacement_removes_candidate_but_literals_cannot_be_rewritten_blindly() {
        assert!(candidates(&[0x0f, 0x0b, 0x90, 0x90]).is_empty());
        // MOV EAX, imm32 contains the same raw bytes and is a false positive.
        assert_eq!(candidates(&[0xb8, 0x48, 0x0f, 0xc7, 0xf0]), vec![2]);
        assert!(candidates(&[0x0f, 0xc7, 0x30]).is_empty()); // memory operand, not RNG
    }
}
