use super::*;
fn hex(b: &[u8]) -> String {
    b.iter().map(|v| format!("{v:02x}")).collect()
}
#[test]
fn canonical_record_read_and_successor_golden_vectors() {
    let h0 = chain::initial();
    assert_eq!(
        hex(&h0),
        "c2e5a8555ad36cf11fe81749aaac6c14897cefbeaeaaac525a6628add49d99b0"
    );
    let h = chain::record(&h0, b'w', 4, 123, 0x0b01_0020, 0x1122_3344);
    assert_eq!(
        hex(&h),
        "e45af213d61d286d545a619fd321c2a22bc57baa603596864aadde9f113abffd"
    );
    let r = chain::observe(&h, &[0, 1, 255]);
    assert_eq!(
        hex(&r),
        "9593674b03604b7e2a8460597c2d5086d4bbf57784c603ce9116abf6b9ada009"
    );
    assert_eq!(
        hex(&chain::successor(&r, chain::CRASH, 456).unwrap()),
        "93b23986012485c3ef98ff3f88c544342bf13f474368300d0d4d181cf60cd5f0"
    );
}
#[test]
fn every_canonical_field_and_mark_contributes() {
    let h = chain::initial();
    let base = chain::record(&h, b'w', 4, 123, 17, 19);
    for changed in [
        chain::record(&h, b'r', 4, 123, 17, 19),
        chain::record(&h, b'w', 8, 123, 17, 19),
        chain::record(&h, b'w', 4, 124, 17, 19),
        chain::record(&h, b'w', 4, 123, 18, 19),
        chain::record(&h, b'w', 4, 123, 17, 20),
    ] {
        assert_ne!(changed, base);
    }
    assert_ne!(chain::observe(&base, &[1]), chain::observe(&base, &[1, 0]));
    assert_ne!(
        chain::successor(&base, 1, 0).unwrap(),
        chain::successor(&base, 2, 0).unwrap()
    );
    assert_ne!(
        chain::successor(&base, 2, 0).unwrap(),
        chain::successor(&base, 3, 0).unwrap()
    );
    assert_ne!(
        chain::successor(&base, 1, 0).unwrap(),
        chain::successor(&base, 1, 1).unwrap()
    );
    assert!(chain::successor(&base, 0, 0).is_err());
}
#[test]
fn observing_and_restoring_saved_chain_is_nondestructive() {
    let mut h = chain::initial();
    for t in 0..4 {
        h = chain::record(&h, b'I', 0, t, 33, 0);
    }
    let saved = h;
    let one = chain::observe(&h, b"ordered pending bytes");
    assert_eq!(one, chain::observe(&h, b"ordered pending bytes"));
    assert_eq!(h, saved);
    let a = chain::record(&h, b'H', 0, 20, 0, 0);
    let b = chain::record(&saved, b'H', 0, 20, 0, 0);
    assert_eq!(a, b);
    assert_ne!(a, chain::record(&one, b'H', 0, 20, 0, 0));
}
#[test]
fn specific_shape_rejects_noncanonical_hashes_without_requiring_ram() {
    let mut e = ExecutionEvidence {
        boot: 0,
        disposition: EvidenceDisposition::Crashed,
        witness: hex(&chain::initial()),
        disk: None,
    };
    assert_eq!(e.validate(), Ok(()));
    e.disk = Some("f".repeat(64));
    assert_eq!(e.validate(), Ok(()));
    e.witness = "A".repeat(64);
    assert!(e.validate().is_err());
    e.witness = "a".repeat(63);
    assert!(e.validate().is_err());
    e.witness = "a".repeat(64);
    e.disk = Some("not a digest".into());
    assert!(e.validate().is_err());
}

/// Host model only: saved chain/pending bytes, not a native snapshot claim.
#[test]
fn saved_model_continuation_is_identical_with_one_or_four_workers() {
    fn advance(states: &mut [[u8; 32]], workers: usize, from: u64, to: u64) {
        for batch in states.chunks_mut(workers) {
            std::thread::scope(|scope| {
                for h in batch {
                    scope.spawn(move || {
                        for t in from..to {
                            *h = chain::record(h, b'I', 0, t, 33, 0);
                        }
                    });
                }
            });
        }
    }
    let mut baseline = vec![chain::initial(); 4];
    advance(&mut baseline, 1, 0, 4);
    let saved = baseline.clone();
    advance(&mut baseline, 1, 4, 9);
    for workers in [1, 4] {
        let mut restored = saved.clone();
        advance(&mut restored, workers, 4, 9);
        assert_eq!(restored, baseline);
        for h in restored {
            assert_eq!(
                chain::observe(&h, b"pending"),
                chain::observe(&baseline[0], b"pending")
            );
        }
    }
}
