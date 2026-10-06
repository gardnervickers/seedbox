use sha2::{Digest, Sha256};
mod admission;
pub(crate) const GUEST: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/guest.bin"));
pub(crate) const ORG: u64 = 0x8000;
pub(crate) fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("fixed fixture word"))
}
pub(crate) fn fixture_table() -> [u64; 7] {
    assert_eq!(word(GUEST, 8), 0x53454544524e4731);
    std::array::from_fn(|i| word(GUEST, 16 + i * 8))
}
pub(crate) fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
// Stateless SplitMix64 domain/index mapping; no host entropy or hidden RNG state.
pub(crate) fn random(seed: u64, index: u64) -> u64 {
    let mut z = seed.wrapping_add((index + 1).wrapping_mul(0x9e3779b97f4a7c15));
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
    z ^ (z >> 31)
}
pub(crate) fn rewrite(bytes: &mut [u8]) -> Result<(), String> {
    let table = fixture_table();
    for (index, op) in [
        &[0x66, 0x0f, 0xc7, 0xf0][..],
        &[0x0f, 0xc7, 0xf8],
        &[0x48, 0x0f, 0xc7, 0xf0],
    ]
    .iter()
    .enumerate()
    {
        let at = (table[index] - ORG) as usize;
        if &bytes[at..at + op.len()] != *op {
            return Err(format!("manifest_original_bytes_mismatch site={index}"));
        }
        bytes[at..at + op.len()].fill(0x90);
        bytes[at..at + 2].copy_from_slice(&[0x0f, 0x0b]);
    }
    Ok(())
}
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod linux;
fn main() {
    if std::env::args().any(|arg| arg == "--admission-audit") {
        let mut rewritten = GUEST.to_vec();
        rewrite(&mut rewritten).unwrap();
        let table = fixture_table();
        let original_candidates: Vec<_> = admission::candidates(GUEST).into_iter().map(|at| {
            let pc = ORG + at as u64;
            serde_json::json!({"opcode_core_pc":pc,"manifested_rng":table[..3].iter().any(|site| pc >= *site && pc < *site + 4)})
        }).collect();
        println!(
            "{}",
            serde_json::json!({"scope":"offline_whole_fixture_candidate_scan_not_runtime_enforcement","guest_sha256":hash(GUEST),"original_candidates":original_candidates,"after_fixed_rewrite_candidates":admission::candidates(&rewritten).into_iter().map(|at| ORG + at as u64).collect::<Vec<_>>(),"interpretation":"raw scanning is conservative; handler immediates include opcode bytes; blind replacement is invalid"})
        );
        return;
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if let Err(e) = linux::run() {
        eprintln!("HARDWARE_GATE_FAILED: {e}");
        std::process::exit(1);
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        // Reference generated metadata here too, so native source checks include it.
        let _ = (
            fixture_table(),
            hash(GUEST),
            random(42, 0),
            ORG,
            rewrite(&mut GUEST.to_vec()),
        );
        eprintln!("UNSUPPORTED_HOST: requires x86_64 Linux /dev/kvm; no emulated admission claim");
        std::process::exit(42);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_rng_encodings_are_exact() {
        for (pc, op) in fixture_table()[..4].iter().zip([
            &[0x66, 0x0f, 0xc7, 0xf0][..],
            &[0x0f, 0xc7, 0xf8],
            &[0x48, 0x0f, 0xc7, 0xf0],
            &[0x0f, 0x0b],
        ]) {
            let at = (*pc - ORG) as usize;
            assert_eq!(&GUEST[at..at + op.len()], op);
        }
        for pc in &fixture_table()[..3] {
            let at = (*pc - ORG) as usize;
            assert_eq!(&GUEST[at - 2..at], &[0x90, 0x90]);
        }
    }
    #[test]
    fn manifest_rewrite_checks_original_bytes_and_preserves_length() {
        let mut bytes = GUEST.to_vec();
        rewrite(&mut bytes).unwrap();
        let table = fixture_table();
        for (i, len) in [4, 3, 4].into_iter().enumerate() {
            let at = (table[i] - ORG) as usize;
            assert_eq!(&bytes[at..at + 2], &[0x0f, 0x0b]);
            assert!(bytes[at + 2..at + len].iter().all(|b| *b == 0x90));
            assert_eq!(bytes[at + len], GUEST[at + len]);
        }
        let mut bad = GUEST.to_vec();
        bad[(table[1] - ORG) as usize] = 0x90;
        assert_eq!(
            rewrite(&mut bad).unwrap_err(),
            "manifest_original_bytes_mismatch site=1"
        );
    }
    #[test]
    fn named_seeded_responses_have_sensitive_seed_and_index() {
        assert_eq!(random(42, 0), 0xbdd732262feb6e95);
        assert_ne!(random(42, 0), random(43, 0));
        assert_ne!(random(42, 0), random(42, 1));
    }
}
