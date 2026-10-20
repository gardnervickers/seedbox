//! Continuing bytes cannot reach an observer session, even across restore.
use super::*;
fn policy_bytes(d: &mut Driver<Mock>) -> Vec<u8> {
    let mut bytes = Vec::new();
    d.with_barrier(|b| {
        b.write_policy_evidence(&mut bytes).unwrap();
        Ok(())
    })
    .unwrap();
    bytes
}
#[test]
fn frame_retention_resize_overflow_and_drain_leave_continuing_bytes_identical() {
    let mut expected = None;
    for workers in [1, 4] {
        for (frame_capacity, drain, resize) in [
            (0, false, false),
            (1, false, false),
            (1024, false, false),
            (1024, true, false),
            (1, true, true),
        ] {
            let mut d = build_network_workers(
                vec![
                    spec(
                        0,
                        true,
                        vec![
                            (0, Response::Ready),
                            (0, frame([255; 6], 1)),
                            (1, output(b"source-log")),
                            (2, frame([255; 6], 2)),
                        ],
                    ),
                    spec(1, true, vec![(0, Response::Ready)]),
                ],
                vec![],
                true,
                true,
                workers,
            );
            d.set_frame_capacity(frame_capacity);
            for end in [1, 2, 3, 4] {
                d.step(at(end)).unwrap();
                if resize {
                    d.set_frame_capacity(end as usize % 2);
                }
                if drain {
                    d.drain_observations();
                }
            }
            let checkpoint = d.checkpoint().unwrap();
            let before = (artifact(d.checkpoint().unwrap()).0, policy_bytes(&mut d));
            d.drain_observations();
            assert_eq!(
                (artifact(d.checkpoint().unwrap()).0, policy_bytes(&mut d)),
                before
            );
            let mut branch = restore(&checkpoint, 5 - workers);
            let fresh = branch.drain_observations();
            assert!(fresh.events.is_empty());
            assert!(fresh.frames.facts.is_empty());
            assert_eq!(fresh.frames.omitted, 0);
            assert_eq!(artifact(branch.checkpoint().unwrap()).0, before.0);
            assert_eq!(policy_bytes(&mut branch), before.1);
            advance(&mut d, 30);
            advance(&mut branch, 30);
            assert_eq!(states(&d), states(&branch));
            assert_eq!(d.position(), branch.position());
            let continued = (artifact(d.checkpoint().unwrap()).0, policy_bytes(&mut d));
            assert_eq!(
                continued,
                (
                    artifact(branch.checkpoint().unwrap()).0,
                    policy_bytes(&mut branch)
                )
            );
            if let Some(reference) = &expected {
                assert_eq!(&(before, continued), reference);
            } else {
                expected = Some((before, continued));
            }
        }
    }
}
