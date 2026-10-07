//! Clock-only world bounds; Machine integration and boot admission are separate.
use seedbox_arm64::{devices::TimePage, pv::PvClock, timebase::Timebase};

fn state(clock: &PvClock) -> (u64, u64, bool) {
    let publication = clock.prepare();
    (publication.now, publication.bound, clock.is_pending())
}

#[test]
fn later_timer_survives_horizon_yield_and_idle_without_an_interrupt() {
    for idle in [false, true] {
        let mut clock = PvClock::default();
        clock.read(10).unwrap();
        clock.arm(100).unwrap();
        clock.set_horizon(Some(40)).unwrap();
        assert_eq!(state(&clock), (10, 40, false));
        if !idle {
            clock.at_bound().unwrap();
        }
        assert!(!clock.judge(idle).unwrap());
        assert_eq!(state(&clock), (40, 40, false));
        clock.at_bound().unwrap();
        assert!(!clock.judge(true).unwrap());
        assert!(clock.clock_read().unwrap_err().contains("horizon"));
        assert_eq!(clock.now(), 40);
        clock.set_horizon(None).unwrap();
        assert_eq!(clock.prepare().bound, 100);
        assert!(clock.judge(true).unwrap());
        assert_eq!(state(&clock), (100, u64::MAX, true));
    }
}

#[test]
fn earlier_and_equal_timers_expire_exactly_once() {
    for timer in [20, 40] {
        for idle in [false, true] {
            let mut clock = PvClock::default();
            clock.arm(timer).unwrap();
            clock.set_horizon(Some(40)).unwrap();
            assert_eq!(clock.prepare().bound, timer);
            if !idle {
                clock.at_bound().unwrap();
            }
            assert!(clock.judge(idle).unwrap());
            assert_eq!(state(&clock), (timer, 40, true));
            assert!(!clock.judge(true).unwrap());
            assert_eq!(clock.now(), timer);
            clock.ack();
            assert!(!clock.judge(true).unwrap());
            assert_eq!(state(&clock), (40, 40, false));
        }
    }
}

#[test]
fn horizon_without_a_timer_handles_idle_yield_and_timer_cancellation() {
    for cancelled in [false, true] {
        let mut clock = PvClock::default();
        clock.set_horizon(Some(50)).unwrap();
        if cancelled {
            clock.arm(20).unwrap();
            clock.arm(0).unwrap();
        }
        assert_eq!(clock.prepare().bound, 50);
        assert!(!clock.judge(true).unwrap());
        clock.at_bound().unwrap();
        assert_eq!(state(&clock), (50, 50, false));
        clock.set_horizon(None).unwrap();
        assert!(clock.judge(true).is_err());
        assert!(clock.at_bound().is_err());
        assert_eq!(state(&clock), (50, u64::MAX, false));
    }
}

#[test]
fn pending_irq_and_deferred_publication_keep_their_existing_policy() {
    let mut clock = PvClock::default();
    clock.arm(10).unwrap();
    assert!(clock.judge(true).unwrap());
    clock.arm(100).unwrap();
    clock.set_horizon(Some(40)).unwrap();
    assert!(!clock.judge(true).unwrap());
    assert_eq!(state(&clock), (10, 40, true));
    let page = TimePage::from_clock(&clock, 9, true);
    assert_eq!((page.now, page.last, page.bound), (10, 9, 40));
    assert!(page.deferred_irq_pending);
    clock.at_bound().unwrap();
    assert!(!clock.judge(false).unwrap());
    assert_eq!(state(&clock), (40, 40, true));
    clock.ack();
    assert!(!clock.judge(true).unwrap());
    assert!(!clock.is_pending());
    clock.set_horizon(None).unwrap();
    assert!(clock.judge(true).unwrap());
    assert_eq!(clock.now(), 100);
}

#[test]
fn reconciliation_cannot_cross_horizon_or_reverse_time() {
    let mut clock = PvClock::default();
    clock.read(10).unwrap();
    clock.arm(100).unwrap();
    clock.set_horizon(Some(40)).unwrap();
    clock.read(39).unwrap();
    clock.read(5).unwrap();
    assert_eq!(clock.clock_read().unwrap(), 40);
    let before = state(&clock);
    for last in [41, 100, u64::MAX] {
        assert!(clock.read(last).is_err());
        assert_eq!(state(&clock), before);
    }
    assert!(clock.clock_read().is_err());
    assert_eq!(state(&clock), before);
    clock.read(0).unwrap();
    assert_eq!(state(&clock), before);
    clock.set_horizon(None).unwrap();
    assert_eq!(clock.prepare().bound, 100);
}

#[test]
fn invalid_horizons_and_overflow_are_transactional() {
    let mut clock = PvClock::default();
    clock.read(10).unwrap();
    clock.arm(100).unwrap();
    clock.set_horizon(Some(40)).unwrap();
    let before = state(&clock);
    for horizon in [9, u64::MAX] {
        assert!(clock.set_horizon(Some(horizon)).is_err());
        assert_eq!(state(&clock), before);
    }
    clock.set_horizon(Some(10)).unwrap();
    assert!(!clock.judge(true).unwrap());
    assert_eq!(clock.now(), 10);

    let mut edge = PvClock::default();
    edge.read(u64::MAX - 2).unwrap();
    edge.set_horizon(Some(u64::MAX - 1)).unwrap();
    assert_eq!(edge.clock_read().unwrap(), u64::MAX - 1);
    let before = state(&edge);
    assert!(edge.clock_read().is_err());
    assert!(edge.read(u64::MAX).is_err());
    assert!(edge.set_horizon(Some(u64::MAX)).is_err());
    assert_eq!(state(&edge), before);
    edge.set_horizon(None).unwrap();
    assert_eq!(edge.clock_read().unwrap(), u64::MAX);
    let before = state(&edge);
    assert!(edge.clock_read().is_err());
    assert_eq!(state(&edge), before);
}

#[test]
fn explicit_none_preserves_the_legacy_clock_sequence() {
    let mut clock = PvClock::default();
    clock.set_horizon(None).unwrap();
    assert_eq!(state(&clock), (0, u64::MAX, false));
    assert!(clock.at_bound().is_err());
    assert!(clock.judge(true).is_err());
    clock.arm(1_000_000_000).unwrap();
    assert!(!clock.judge(false).unwrap());
    assert_eq!(clock.clock_read().unwrap(), 1);
    clock.read(99).unwrap();
    assert!(clock.judge(true).unwrap());
    assert_eq!(state(&clock), (1_000_000_000, u64::MAX, true));
    assert!(!clock.judge(true).unwrap());
    clock.ack();
    assert!(clock.judge(true).is_err());
}
