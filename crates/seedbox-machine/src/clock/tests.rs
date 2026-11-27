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

#[test]
fn boot_realtime_offsets_are_bounded_deterministic_and_spread() {
    let offsets: Vec<i64> = (0..4096u64).map(boot_realtime_offset_ns).collect();
    assert_eq!(
        offsets,
        (0..4096u64)
            .map(boot_realtime_offset_ns)
            .collect::<Vec<_>>()
    );
    assert!(offsets.iter().all(|o| o.abs() <= REALTIME_OFFSET_BOUND_NS));
    let distinct: std::collections::BTreeSet<_> = offsets.iter().collect();
    assert!(distinct.len() > 4000, "{} distinct", distinct.len());
    // Both signs, and spread across the whole range.
    assert!(offsets
        .iter()
        .any(|&o| o < -REALTIME_OFFSET_BOUND_NS * 9 / 10));
    assert!(offsets
        .iter()
        .any(|&o| o > REALTIME_OFFSET_BOUND_NS * 9 / 10));
    // Neighbouring boot seeds, as consecutive draws often are, still differ.
    assert_ne!(boot_realtime_offset_ns(1), boot_realtime_offset_ns(2));
}

#[test]
fn boot_command_line_appends_one_offset_argument() {
    let offset = boot_realtime_offset_ns(7);
    assert_eq!(
        boot_command_line("console=ttyAMA0 quiet", 7),
        format!("console=ttyAMA0 quiet seedbox.clock_offset_ns={offset}")
    );
    assert_eq!(
        boot_command_line("", 7),
        format!("seedbox.clock_offset_ns={offset}")
    );
}

#[test]
fn a_base_line_setting_the_offset_is_recognised() {
    assert!(sets_realtime_offset("quiet seedbox.clock_offset_ns=0"));
    assert!(!sets_realtime_offset("quiet noseedbox.clock_offset_ns=0"));
    assert!(!sets_realtime_offset(""));
}

#[test]
fn the_reserved_width_covers_the_widest_offset_argument() {
    assert_eq!(
        format!(" {REALTIME_OFFSET_ARGUMENT}{}", -REALTIME_OFFSET_BOUND_NS).len(),
        REALTIME_OFFSET_ARGUMENT_BYTES
    );
    for seed in 0..4096 {
        assert!(boot_command_line("x", seed).len() <= 1 + REALTIME_OFFSET_ARGUMENT_BYTES);
    }
}
