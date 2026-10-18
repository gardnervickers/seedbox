use super::*;

#[test]
fn rate_bounds_are_exact() {
    for ppb in [-100_000_000, -1, 0, 1, 100_000_000] {
        assert_eq!(ClockRate::new(ppb).unwrap().ppb() as i64, ppb);
    }
    for ppb in [-100_000_001, 100_000_001, i64::MIN, i64::MAX] {
        assert_eq!(ClockRate::new(ppb), Err(InvalidClockRate { ppb }));
    }
    assert_eq!(ClockRate::default(), ClockRate::NOMINAL);
}
