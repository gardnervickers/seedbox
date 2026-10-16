#[test]
fn bounds_are_inclusive() {
    for (length, valid) in [(13, false), (14, true), (1514, true), (1515, false)] {
        assert_eq!(super::frame_in_bounds(&vec![0; length]), valid);
    }
}
