use super::*;

fn exact(projection: Projection, at: u64) -> i128 {
    projection.adjusted_at(at).unwrap()
}

#[test]
fn identity_until_a_rate_change() {
    let projection = Projection::new(7);
    for at in [7, 8, 1_000, u64::MAX] {
        assert_eq!(projection.floor(at), Ok(at));
        assert_eq!(projection.deadline(7, at), Ok(at.max(7)));
    }
    assert_eq!(projection.floor(6), Err(Error::PastAnchor));
}

#[test]
fn positive_and_negative_drift_scale_elapsed_time_exactly() {
    for rate in [-100_000_000, -1, 0, 1, 100_000_000] {
        let mut projection = Projection::new(7);
        projection.set_drift(7, rate).unwrap();
        assert_eq!(
            exact(projection, 17),
            17 * DENOMINATOR + 10 * i128::from(rate)
        );
        // Floor of the exact value, including negative fractional parts.
        assert_eq!(
            projection.floor(17).unwrap() as i128,
            (17 * DENOMINATOR + 10 * i128::from(rate)).div_euclid(DENOMINATOR)
        );
    }
    let mut fast = Projection::new(0);
    fast.set_drift(0, 100_000_000).unwrap();
    assert_eq!(fast.floor(1_000_000_000), Ok(1_100_000_000));
    assert_eq!(fast.floor(9), Ok(9));
    assert_eq!(fast.floor(10), Ok(11));
    let mut slow = Projection::new(0);
    slow.set_drift(0, -100_000_000).unwrap();
    assert_eq!(slow.floor(1_000_000_000), Ok(900_000_000));
    assert_eq!(slow.floor(19), Ok(17));
}

#[test]
fn reanchor_is_continuous_and_keeps_fractional_offset() {
    let mut projection = Projection::new(0);
    projection.set_drift(0, 1).unwrap();
    projection.set_drift(7, -2).unwrap();
    projection.set_drift(10, 0).unwrap();
    // Seven billionths minus six billionths is retained after heal.
    assert_eq!(exact(projection, 10), 10 * DENOMINATOR + 1);
    assert_eq!(exact(projection, 100), 100 * DENOMINATOR + 1);
    let mut split = Projection::new(0);
    split.set_drift(0, 1).unwrap();
    for at in 1..=7 {
        split.set_drift(at, 1).unwrap();
    }
    split.set_drift(7, -2).unwrap();
    for at in 8..=10 {
        split.set_drift(at, -2).unwrap();
    }
    split.set_drift(10, 0).unwrap();
    assert_eq!(split, projection);
    // Every reanchor preserves the mapped value at its anchor.
    let mut drifting = Projection::new(3);
    for (at, rate) in [(5, 100_000_000), (1_000_003, -100_000_000), (2_000_009, 7)] {
        let before = exact(drifting, at);
        drifting.set_drift(at, rate).unwrap();
        assert_eq!(exact(drifting, at), before);
        assert_eq!(drifting.anchor(), at);
    }
}

#[test]
fn inverse_is_earliest_and_never_early() {
    for rate in [-100_000_000, -1, 0, 1, 99_999_999, 100_000_000] {
        let mut projection = Projection::new(3);
        projection.set_drift(3, rate).unwrap();
        for not_before in [3, 4, 50] {
            for machine in 0..200 {
                let at = projection.deadline(not_before, machine).unwrap();
                assert!(at >= not_before);
                assert!(projection.floor(at).unwrap() >= machine);
                assert!(exact(projection, at) >= i128::from(machine) * DENOMINATOR);
                if at > not_before {
                    assert!(projection.floor(at - 1).unwrap() < machine);
                }
            }
        }
    }
}

#[test]
fn inverse_observes_fractional_adjustment_and_already_due_deadlines() {
    let mut projection = Projection::new(0);
    projection.set_drift(0, -1).unwrap();
    assert_eq!(projection.deadline(0, 1), Ok(2));
    projection.set_drift(1, 0).unwrap();
    assert_eq!(projection.deadline(1, 1), Ok(2));
    assert_eq!(projection.deadline(7, 1), Ok(7));
}

#[test]
fn full_range_and_checked_errors_leave_state_unchanged() {
    let mut projection = Projection::new(0);
    projection.set_drift(0, 100_000_000).unwrap();
    assert_eq!(
        exact(projection, u64::MAX),
        i128::from(u64::MAX) * 1_100_000_000
    );
    // The fast mapping of the largest simulation time is not a u64.
    assert_eq!(projection.floor(u64::MAX), Err(Error::Overflow));
    assert_eq!(projection.deadline(u64::MAX, 1), Ok(u64::MAX));
    let before = projection;
    assert_eq!(
        projection.set_drift(0, -1_000_000_000),
        Err(Error::NonIncreasingRate)
    );
    assert_eq!(projection, before);
    let mut slow = Projection::new(0);
    slow.set_drift(0, -100_000_000).unwrap();
    // No representable simulation time reaches the largest machine time.
    assert_eq!(slow.deadline(0, u64::MAX), Err(Error::Overflow));
    let mut fast = Projection::new(0);
    fast.set_drift(0, i64::MAX).unwrap();
    let fast_before = fast;
    assert_eq!(fast.floor(u64::MAX), Err(Error::Overflow));
    assert_eq!(fast, fast_before);
    projection.set_drift(11, 0).unwrap();
    let before = projection;
    assert_eq!(projection.set_drift(10, 1), Err(Error::PastAnchor));
    assert_eq!(projection.floor(10), Err(Error::PastAnchor));
    assert_eq!(projection.deadline(10, 0), Err(Error::PastAnchor));
    assert_eq!(projection, before);
}
