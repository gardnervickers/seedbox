//! Retention capacity is a scalar, not a decoded collection allocation.
use super::*;

fn tight_roundtrip(driver: &mut Driver<Mock>) -> Checkpoint<Saved> {
    let (bytes, store) = artifact(driver.checkpoint().unwrap());
    let mut bounds = limits();
    // Match the service: resource counts cannot exceed the retained byte size.
    bounds.encoded_bytes = bytes.len() as u64;
    bounds.collection_items = bounds.encoded_bytes;
    let checkpoint = Checkpoint::read_from(&mut &bytes[..], bounds).unwrap();
    let mut encoded = Vec::new();
    checkpoint.write_to(&mut encoded).unwrap();
    assert_eq!(encoded, bytes);
    decode(&encoded, &store)
}

#[test]
fn unused_frame_capacity_larger_than_capture_roundtrips_with_tight_limits() {
    for capacity in [1024, usize::MAX] {
        let mut driver = build(vec![spec(0, true, vec![])], vec![], true);
        driver.set_frame_capacity(capacity);
        let (bytes, _) = artifact(driver.checkpoint().unwrap());
        assert!(bytes.len() < capacity, "small service capture regression");
        let checkpoint = tight_roundtrip(&mut driver);
        let expected = advance(&mut driver, 60);
        let expected_states = states(&driver);
        for workers in [1, 4] {
            let mut restored = restore(&checkpoint, workers);
            assert_eq!(restored.drain_frames(), FrameFacts::default());
            assert_eq!(advance(&mut restored, 60), expected);
            assert_eq!(states(&restored), expected_states);
        }
    }
}

#[test]
fn buffered_frames_shrunk_cleared_or_with_unused_capacity_roundtrip() {
    for capacity in [0, 1, usize::MAX] {
        let mut driver = net(
            vec![
                spec(
                    0,
                    true,
                    vec![
                        (1, frame(mac(1), 7)),
                        (2, frame(mac(1), 8)),
                        (3, frame(mac(1), 9)),
                    ],
                ),
                spec(1, true, vec![]),
            ],
            setup(8),
            1,
        );
        advance(&mut driver, 50);
        driver.set_frame_capacity(capacity);
        let checkpoint = tight_roundtrip(&mut driver);
        let retained = driver.drain_frames();
        let kept = capacity.min(3);
        assert_eq!(retained.facts.len(), kept);
        assert_eq!(retained.omitted, (3 - kept) as u64);
        let expected = advance(&mut driver, 80);
        let expected_states = states(&driver);
        for workers in [1, 4] {
            let mut restored = restore(&checkpoint, workers);
            assert_eq!(restored.drain_frames(), retained);
            assert_eq!(advance(&mut restored, 80), expected);
            assert_eq!(states(&restored), expected_states);
        }
    }
}
