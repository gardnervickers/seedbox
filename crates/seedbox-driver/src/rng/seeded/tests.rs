use super::*;
#[test]
fn chunks_and_restored_snapshots_continue_identical_bytes() {
    let mut whole = SeededRng::new(9);
    let mut want = [0; 107];
    whole.fill_bytes(&mut want);
    let mut pieces = SeededRng::new(9);
    let mut got = [0; 107];
    pieces.fill_bytes(&mut got[..3]);
    pieces.fill_bytes(&mut []);
    pieces.fill_bytes(&mut got[3..37]);
    let (domain, seed, block, bytes, position) = pieces.parts();
    let mut restored = SeededRng::from_parts(domain, seed, block, bytes, position).unwrap();
    let mut clone = pieces.clone();
    pieces.fill_bytes(&mut got[37..]);
    assert_eq!(want, got);
    let mut a = [0; 70];
    let mut b = [0; 70];
    restored.fill_bytes(&mut a);
    clone.fill_bytes(&mut b);
    assert_eq!(a, b);
    assert_eq!(a, want[37..]);
    let mut other = [0; 107];
    SeededRng::new(10).fill_bytes(&mut other);
    assert_ne!(want, other);
}
#[test]
#[should_panic(expected = "block counter")]
fn exhaustion_does_not_wrap() {
    let mut rng = SeededRng::new(0);
    rng.block = u64::MAX;
    rng.fill_bytes(&mut [0]);
}
#[test]
fn domains_share_a_seed_without_sharing_bytes() {
    let mut machine = SeededRng::new(9);
    let mut clock = SeededRng::derived(9, Domain::ClockFaults);
    let (mut a, mut b) = ([0; 64], [0; 64]);
    machine.fill_bytes(&mut a);
    clock.fill_bytes(&mut b);
    assert_ne!(a, b);
    // The machine domain keeps its original format tag and bytes.
    let mut hash = Sha256::new();
    hash.update(b"seedbox-machine seeded bytes v1\0");
    hash.update(9u64.to_le_bytes());
    hash.update(0u64.to_le_bytes());
    assert_eq!(a[..32], <[u8; 32]>::from(hash.finalize()));
}

#[test]
fn common_spec_stream_preserves_machine_domain_bytes_and_split_cursors() {
    use super::RngStream;
    for seed in [0, 1, 9, 41, u64::MAX] {
        for fills in [
            &[0, 1, 31, 0, 32, 33, 63, 64, 65][..],
            &[32, 0, 32, 1, 31, 33][..],
        ] {
            let mut production = super::SeededRng::new(seed);
            let mut fixture = seedbox_machine::test_support::SeededRng::new(seed);
            for &length in fills {
                let mut want = vec![0; length];
                let mut got = vec![0; length];
                production.fill_bytes(&mut want);
                fixture.fill_bytes(&mut got);
                assert_eq!(got, want, "seed={seed}, fill={length}");
                let (domain, seed, block, bytes, position) = production.parts();
                assert_eq!(domain, Domain::Machine);
                assert_eq!(
                    serde_json::to_value(&fixture).unwrap(),
                    serde_json::json!({
                        "domain": "Machine",
                        "seed": seed,
                        "block": block,
                        "bytes": bytes,
                        "position": position,
                    })
                );
            }
        }
    }
}
