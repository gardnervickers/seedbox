use sha2::{Digest, Sha256};
pub(crate) const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.bin"));
pub(crate) const ORG: u64 = 0x8000;
pub(crate) fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed fixture word"))
}
pub(crate) fn fixture_table() -> [u64; 7] {
    assert_eq!(word(GUEST, 8), 0x5345454454534331);
    std::array::from_fn(|i| word(GUEST, 16 + i * 8))
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
pub(crate) fn timestamp(seed: u64, index: u64) -> u64 {
    (0x12345678fedc0000_u64 ^ seed).wrapping_add(index.wrapping_mul(0x100000041))
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;
fn main() {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Err(e) = linux::run() {
        eprintln!("HARDWARE_GATE_FAILED: {e}");
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        // Reference generated metadata here too, so native source checks include it.
        let _ = (fixture_table(), hash(GUEST), timestamp(42, 0), ORG);
        eprintln!("UNSUPPORTED_HOST: requires x86_64 Linux /dev/kvm; no emulated admission claim");
        std::process::exit(42);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn generated_exact_instruction_cases() {
        let table = fixture_table();
        for (pc, opcode) in table[..4].iter().zip([
            &[0x0f, 0x31][..],
            &[0x0f, 0x01, 0xf9],
            &[0x66, 0x0f, 0x31],
            &[0xfa],
        ]) {
            let at = (*pc - ORG) as usize;
            assert_eq!(&GUEST[at..at + opcode.len()], opcode);
        }
        for pc in &table[4..] {
            assert!(*pc >= 0x9000 && *pc < 0xb000);
        }
    }
    #[test]
    fn controlled_time_has_nonzero_high_and_low_parts() {
        for index in 0..3 {
            let t = timestamp(0x51eeb5eed, index);
            assert_ne!(t as u32, 0);
            assert_ne!((t >> 32) as u32, 0);
        }
        assert_ne!(timestamp(42, 0), timestamp(43, 0));
        assert_eq!(timestamp(42, 1).wrapping_sub(timestamp(42, 0)), 0x100000041);
    }
}
