//! Stop-boundary rules independent of KVM and the host PMU.

pub fn hard_limit(limit: u64, deadline: Option<u64>, exact: bool, tolerance: u64) -> u64 {
    let pit = deadline.unwrap_or(u64::MAX);
    limit.min(if exact {
        pit
    } else {
        pit.saturating_add(tolerance)
    })
}

/// Deterministic clock costs must fit in full; never clamp away guest time.
/// This error is deliberately distinct from retryable PMU "overshoot" errors.
pub fn advance_time(now: u64, ticks: u64, limit: u64) -> Result<u64, String> {
    match now.checked_add(ticks) {
        Some(next) if next <= limit => Ok(next),
        _ => Err(format!(
            "clock bound exceeded: time {now} + {ticks} exceeds hard stop {limit}"
        )),
    }
}

pub fn missed_soft_stop(
    windowed: bool,
    window: bool,
    delta: u64,
    remaining: u64,
    interrupted: bool,
    stepping: bool,
) -> bool {
    // With exit_req closed, the syscall-hook branch can skip its OUT at
    // exactly the soft count. An overflow landing there cannot establish
    // whether the first requested yield was missed; opening the window now
    // cannot recover it. Treat equality like the hard-stop EINTR guard.
    windowed && !window && (delta > remaining || (interrupted && !stepping && delta == remaining))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_io_cost_cannot_cross_horizon_or_duration() {
        let err = advance_time(900, 1000, 1000).unwrap_err();
        assert!(err.starts_with("clock bound exceeded:"));
        assert!(!err.contains("overshoot"));
        assert_eq!(advance_time(0, 1000, 1000).unwrap(), 1000);
        assert_eq!(advance_time(1000, 0, 1000).unwrap(), 1000);
    }

    #[test]
    fn timer_cost_obeys_exact_and_tolerated_deadlines() {
        let exact = hard_limit(2000, Some(1000), true, 500);
        assert!(advance_time(900, 101, exact).is_err());
        assert_eq!(advance_time(900, 100, exact).unwrap(), 1000);
        let late = hard_limit(2000, Some(1000), false, 500);
        assert_eq!(advance_time(900, 600, late).unwrap(), 1500);
        assert!(advance_time(900, 601, late).is_err());
        assert_eq!(hard_limit(1200, Some(1000), false, 500), 1200);
    }

    #[test]
    fn time_page_and_time_port_cannot_escape_the_boundary() {
        assert!(advance_time(950, 60, 1000).is_err());
        assert_eq!(advance_time(950, 50, 1000).unwrap(), 1000);
        assert!(advance_time(1000, 1, 1000).is_err());
        assert!(advance_time(u64::MAX, 1, u64::MAX).is_err());
        assert_eq!(hard_limit(1000, None, false, u64::MAX), 1000);
    }

    #[test]
    fn closed_window_overflow_at_hook_branch_is_ambiguous() {
        assert!(missed_soft_stop(true, false, 16_384, 16_384, true, false));
        assert!(missed_soft_stop(true, false, 16_385, 16_384, true, false));
        assert!(!missed_soft_stop(true, false, 16_383, 16_384, true, false));
    }

    #[test]
    fn deterministic_equality_and_open_windows_remain_legal() {
        assert!(!missed_soft_stop(true, false, 16_384, 16_384, false, false));
        assert!(!missed_soft_stop(true, false, 16_384, 16_384, true, true));
        assert!(!missed_soft_stop(true, true, 16_384, 16_384, true, false));
        assert!(!missed_soft_stop(false, false, 16_384, 16_384, true, false));
    }
}
