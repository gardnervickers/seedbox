//! PV clock primitive with an optional external world horizon.
//!
//! Setting a horizon requires a stopped guest. Machine still runs its fixture
//! to poweroff: this primitive does not integrate a world stopping policy or
//! admit the initial boot/handshake clock against an external horizon.

use crate::{
    timebase::{Publication, Timebase},
    Result,
};
#[derive(Default, Debug)]
pub struct PvClock {
    now: u64,
    deadline: Option<u64>,
    horizon: Option<u64>,
    pending: bool,
}
impl PvClock {
    /// Set the external bound while the guest is stopped. None retains the
    /// legacy unbounded policy. u64::MAX is reserved for an unbounded publication
    /// and is not a valid finite guest clock timestamp.
    pub fn set_horizon(&mut self, horizon: Option<u64>) -> Result<()> {
        if let Some(horizon) = horizon {
            if horizon == u64::MAX {
                return Err("PV horizon timestamp overflow".into());
            }
            if horizon < self.now {
                return Err("PV horizon precedes current clock".into());
            }
        }
        self.horizon = horizon;
        Ok(())
    }
    fn bound(&self) -> Option<u64> {
        match (self.deadline, self.horizon) {
            (Some(timer), Some(horizon)) => Some(timer.min(horizon)),
            (timer, horizon) => timer.or(horizon),
        }
    }
    pub fn ack(&mut self) {
        self.pending = false;
    }
    pub fn is_pending(&self) -> bool {
        self.pending
    }
    pub fn clock_read(&mut self) -> Result<u64> {
        let next = self.now.checked_add(1).ok_or("PV clock overflow")?;
        if self.horizon.is_some_and(|horizon| next > horizon) {
            return Err("PV clock read exceeds world horizon".into());
        }
        self.now = next;
        Ok(self.now)
    }
    pub fn at_bound(&mut self) -> Result<()> {
        let bound = self.bound().ok_or("PV yield without an armed deadline")?;
        self.now = self.now.max(bound);
        Ok(())
    }
}
impl Timebase for PvClock {
    fn prepare(&self) -> Publication {
        Publication {
            now: self.now,
            bound: self.bound().unwrap_or(u64::MAX),
        }
    }
    fn read(&mut self, last: u64) -> Result<()> {
        if last == u64::MAX {
            return Err("PV guest clock overflow".into());
        }
        if self.horizon.is_some_and(|horizon| last > horizon) {
            return Err("PV guest clock exceeds world horizon".into());
        }
        self.now = self.now.max(last);
        Ok(())
    }
    fn now(&self) -> u64 {
        self.now
    }
    fn arm(&mut self, absolute_ns: u64) -> Result<()> {
        self.deadline = if absolute_ns == 0 {
            None
        } else {
            Some(absolute_ns)
        };
        Ok(())
    }
    fn judge(&mut self, idle: bool) -> Result<bool> {
        if idle && !self.pending {
            self.now = self
                .now
                .max(self.bound().ok_or("idle without an armed PV timer")?);
        }
        if self.deadline.is_some_and(|d| d <= self.now) {
            self.deadline = None;
            self.pending = true;
            return Ok(true);
        }
        Ok(false)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compute_is_free_reads_cost_one_idle_jumps() {
        let mut c = PvClock::default();
        c.arm(1_000_000_000).unwrap();
        assert!(!c.judge(false).unwrap());
        assert_eq!(c.now(), 0);
        assert_eq!(c.clock_read().unwrap(), 1);
        c.read(99).unwrap();
        assert_eq!(c.now(), 99);
        assert!(c.judge(true).unwrap());
        assert_eq!(c.now(), 1_000_000_000);
        assert!(!c.judge(true).unwrap());
        c.ack();
        assert!(c.judge(true).is_err());
    }
    #[test]
    fn expired_cancelled_and_overflow() {
        let mut c = PvClock::default();
        c.read(100).unwrap();
        c.arm(50).unwrap();
        assert!(c.judge(false).unwrap());
        c.ack();
        c.arm(200).unwrap();
        c.arm(0).unwrap();
        assert!(c.at_bound().is_err());
        c.now = u64::MAX;
        assert!(c.clock_read().is_err());
        assert!(c.read(u64::MAX).is_err());
    }
}

#[cfg(test)]
mod bounds_tests {
    use super::*;
    #[test]
    fn polling_yield_expires_once_and_stale_page_cannot_reverse_time() {
        let mut c = PvClock::default();
        c.read(123).unwrap();
        c.arm(200).unwrap();
        assert_eq!(c.prepare().bound, 200);
        c.at_bound().unwrap();
        assert!(c.judge(false).unwrap());
        assert!(!c.judge(false).unwrap());
        c.read(125).unwrap();
        assert_eq!(c.now(), 200);
        assert_eq!(c.prepare().bound, u64::MAX);
        c.ack();
        c.arm(300).unwrap();
        assert!(c.judge(true).unwrap());
        assert_eq!(c.now(), 300);
    }
}
