//! Retention capacity is an observer scalar: neither a decoded allocation nor
//! continuing checkpoint data. Preserve the small-capture bd-40a regression.
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
fn pending_network_authority_bytes_exclude_frame_capacity_facts_and_omission_cursor() {
    let mut expected = None;
    for workers in [1, 4] {
        for capacity in [0, 1, usize::MAX] {
            let mut source = net(
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
                setup(capacity),
                workers,
            );
            advance(&mut source, 3);
            let (bytes, store) = artifact(source.checkpoint().unwrap());
            if let Some(expected) = &expected {
                assert_eq!(&bytes, expected);
            } else {
                expected = Some(bytes.clone());
            }
            let retained = source.drain_frames();
            let kept = capacity.min(3);
            assert_eq!(retained.facts.len(), kept);
            assert_eq!(
                retained.omitted,
                if capacity == 0 { 0 } else { (3 - kept) as u64 }
            );
            assert_eq!(artifact(source.checkpoint().unwrap()).0, bytes);
            let checkpoint = decode(&bytes, &store);
            advance(&mut source, 80);
            let finished = states(&source);
            for restored_workers in [1, 4] {
                let mut branch = restore(&checkpoint, restored_workers);
                assert_eq!(branch.drain_frames(), FrameFacts::default());
                assert_eq!(artifact(branch.checkpoint().unwrap()).0, bytes);
                advance(&mut branch, 80);
                assert_eq!(states(&branch), finished);
            }
        }
    }
}

#[test]
fn unused_frame_capacity_larger_than_capture_roundtrips_with_tight_limits() {
    for capacity in [1024, usize::MAX] {
        let mut driver = build(vec![spec(0, true, vec![])], vec![], true);
        let allocation = driver.observations.frames.frames.capacity();
        let authority = artifact(driver.checkpoint().unwrap()).0;
        driver.set_frame_capacity(capacity);
        assert_eq!(
            driver.observations.frames.frames.capacity(),
            allocation,
            "unused observer retention must not reserve configured capacity"
        );
        let (bytes, _) = artifact(driver.checkpoint().unwrap());
        assert_eq!(bytes, authority, "retention scalar cannot reach the codec");
        assert!(bytes.len() < capacity, "small service capture regression");
        let checkpoint = tight_roundtrip(&mut driver);
        reference_observer_session(&mut driver);
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
        reference_observer_session(&mut driver);
        let expected = advance(&mut driver, 80);
        let expected_states = states(&driver);
        for workers in [1, 4] {
            let mut restored = restore(&checkpoint, workers);
            assert_eq!(restored.drain_frames(), FrameFacts::default());
            assert_eq!(advance(&mut restored, 80), expected);
            assert_eq!(states(&restored), expected_states);
        }
    }
}
