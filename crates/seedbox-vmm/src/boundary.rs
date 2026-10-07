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

/// Existing timer-port cost; execution never discards a charge at a boundary.
pub fn io_charge(port: u16, write: bool) -> u64 {
    if matches!(port, 0x40..=0x42 | 0x61) || (write && matches!(port, 0x43 | 0x530)) {
        1000
    } else {
        0
    }
}

/// A sampled/applied KVM I/O operation whose deterministic cost is unpaid.
/// None in Machine means KVM completion succeeded, not merely zero debt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingIo {
    pub remaining_ns: u64,
    pub port: u16,
    pub write: bool,
    pub width: u8,
    pub count: u32,
    pub bytes: Vec<u8>,
}

impl PendingIo {
    pub fn new(now: u64, port: u16, write: bool, width: u8, count: u32, bytes: Vec<u8>) -> Result<Self, String> {
        let charge = io_charge(port, write);
        if charge == 0 || !matches!(width, 1 | 2 | 4) || usize::from(width).checked_mul(count as usize) != Some(bytes.len()) {
            return Err("invalid charged I/O descriptor".into());
        }
        advance_time(now, charge, u64::MAX)?;
        Ok(Self { remaining_ns: charge, port, write, width, count, bytes })
    }

    /// Pay only to the next timer or world boundary; never run the guest here.
    /// The caller expires due timers before calling and after advancing.
    pub fn advance(&mut self, now: u64, limit: u64, deadline: Option<u64>) -> Result<u64, String> {
        let remaining = limit.checked_sub(now).ok_or("I/O delay begins beyond world limit")?;
        let timer = match deadline {
            Some(d) => d.checked_sub(now).filter(|n| *n > 0).ok_or("due timer must expire before I/O delay advances")?,
            None => u64::MAX,
        };
        let paid = self.remaining_ns.min(remaining).min(timer);
        let next = advance_time(now, paid, limit)?;
        self.remaining_ns -= paid;
        Ok(next)
    }

    pub fn canonical(&self) -> Vec<u8> {
        let mut out = vec![1, self.write as u8, self.width];
        out.extend_from_slice(&self.port.to_le_bytes());
        out.extend_from_slice(&self.count.to_le_bytes());
        out.extend_from_slice(&self.remaining_ns.to_le_bytes());
        out.extend_from_slice(&self.bytes);
        out
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

    fn pending() -> PendingIo {
        PendingIo::new(900, 0x530, true, 2, 1, vec![1, 0]).unwrap()
    }

    #[test]
    fn only_existing_timer_ports_are_charged() {
        for port in 0u16..=u16::MAX {
            assert_eq!(io_charge(port, false), if matches!(port, 0x40..=0x42 | 0x61) {1000} else {0});
            assert_eq!(io_charge(port, true), if matches!(port, 0x40..=0x43 | 0x61 | 0x530) {1000} else {0});
        }
    }

    #[test]
    fn io_cost_is_conserved_across_arbitrary_world_subdivisions() {
        // Original precharge rejected this exact boundary crossing.
        assert!(advance_time(900, 1000, 1000).is_err());
        let mut whole = pending();
        assert_eq!(whole.advance(900, 1900, None).unwrap(), 1900);
        assert_eq!(whole.remaining_ns, 0);
        for width in 1..=1000 {
            let mut p=pending();
            let mut now=900;
            while p.remaining_ns>0 {
                now=p.advance(now, now+width, None).unwrap();
                assert_eq!(now+p.remaining_ns, 1900);
            }
            assert_eq!(now,1900);
            assert_eq!(p,whole);
        }
    }

    #[test]
    fn timer_assertion_boundary_does_not_complete_or_discard_io() {
        let mut p=pending();
        assert_eq!(p.advance(900,1900,Some(1000)).unwrap(),1000);
        assert_eq!(p.remaining_ns,900);
        assert!(p.advance(1000,1900,Some(1000)).is_err());
        assert_eq!(p.advance(1000,1000,None).unwrap(),1000);
        assert_eq!(p.remaining_ns,900);
        assert_eq!(p.advance(1000,1900,Some(2000)).unwrap(),1900);
    }

    #[test]
    fn pending_state_distinguishes_operation_and_uncompleted_zero_debt() {
        let p=pending();
        for field in 0..6 {
            let mut other=p.clone();
            match field {
                0=>other.remaining_ns-=1,
                1=>other.write=false,
                2=>other.port+=1,
                3=>other.bytes[0]+=1,
                4=>{other.width=1;other.count=2;},
                _=>other.remaining_ns=0,
            }
            assert_ne!(other.canonical(),p.canonical());
            assert_ne!(other.canonical(),vec![0]);
        }
        assert!(PendingIo::new(u64::MAX-999,0x530,true,2,1,vec![1,0]).is_err());
        assert!(PendingIo::new(0,0x530,true,1,1,vec![1,0]).is_err());
        assert!(pending().advance(900,899,None).is_err());
    }

}
