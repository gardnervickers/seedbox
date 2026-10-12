//! One logical precharge per block. Zero is not due. The stable gateway pays
//! negative debt, refunds the old block and re-enters lookup, which charges the
//! *current* block. A parked old generation never licenses executing old bytes.
use crate::Error;

pub const DEFAULT_BUDGET: u32 = 50_000;
pub const PROGRESS_MARK: u64 = 0x5eed_b0c5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Debited { instructions: u32 },
    Paid { instructions: u32 },
    Running { instructions: u32 },
}

#[derive(Debug)]
pub struct Meter {
    quantum: u32,
    left: i64,
    phase: Phase,
    payments: u64,
}

impl Meter {
    pub fn new(quantum: u32) -> Result<Self, Error> {
        if quantum == 0 {
            return Err(Error::Range);
        }
        Ok(Self {
            quantum,
            left: quantum.into(),
            phase: Phase::Ready,
            payments: 0,
        })
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn left(&self) -> i64 {
        self.left
    }
    pub fn payments(&self) -> u64 {
        self.payments
    }
    pub fn precharge(&mut self, instructions: u32) -> Result<bool, Error> {
        if self.phase != Phase::Ready || instructions == 0 {
            return Err(Error::ChargePhase);
        }
        self.left -= i64::from(instructions);
        self.phase = Phase::Debited { instructions };
        Ok(self.left < 0)
    }
    /// Call after a successful marked syscall. Context is parked in the stable
    /// gateway throughout the call; no asynchronous application delivery is
    /// supported inside this diagnostic transaction.
    pub fn paid_one(&mut self) -> Result<bool, Error> {
        let Phase::Debited { instructions } = self.phase else {
            return Err(Error::ChargePhase);
        };
        if self.left >= 0 {
            return Err(Error::ChargePhase);
        }
        let payments = self.payments.checked_add(1).ok_or(Error::Range)?;
        self.left += i64::from(self.quantum);
        self.payments = payments;
        if self.left >= 0 {
            self.phase = Phase::Paid { instructions };
        }
        Ok(self.left < 0)
    }
    pub fn refund_for_dispatch(&mut self) -> Result<(), Error> {
        let Phase::Paid { instructions } = self.phase else {
            return Err(Error::ChargePhase);
        };
        self.left += i64::from(instructions);
        self.phase = Phase::Ready;
        Ok(())
    }
    /// Commit permission to execute this block. A mid-block application fault
    /// retains the full precharge; it is never refunded as if unexecuted.
    pub fn enter(&mut self) -> Result<(), Error> {
        let Phase::Debited { instructions } = self.phase else {
            return Err(Error::ChargePhase);
        };
        if self.left < 0 {
            return Err(Error::ChargePhase);
        }
        self.phase = Phase::Running { instructions };
        Ok(())
    }
    pub fn complete_block(&mut self) -> Result<(), Error> {
        if !matches!(self.phase, Phase::Running { .. }) {
            return Err(Error::ChargePhase);
        }
        self.phase = Phase::Ready;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sign_negative_and_refund_reentry() {
        let mut m = Meter::new(3).unwrap();
        assert!(!m.precharge(3).unwrap());
        m.enter().unwrap();
        assert_eq!(m.left(), 0);
        m.complete_block().unwrap();
        assert!(m.precharge(8).unwrap());
        assert!(m.paid_one().unwrap());
        assert!(m.paid_one().unwrap());
        assert!(!m.paid_one().unwrap());
        assert_eq!(m.left(), 1);
        m.refund_for_dispatch().unwrap();
        assert!(!m.precharge(8).unwrap());
        m.enter().unwrap();
        assert_eq!((m.left(), m.payments()), (1, 3));
    }
    #[test]
    fn invalidated_block_refunds_old_length_charges_new_length() {
        let mut m = Meter::new(2).unwrap();
        assert!(m.precharge(3).unwrap());
        assert!(!m.paid_one().unwrap());
        m.refund_for_dispatch().unwrap();
        assert!(!m.precharge(1).unwrap());
        m.enter().unwrap();
        assert_eq!(m.left(), 3);
    }
    #[test]
    fn fault_retains_running_precharge_and_cannot_refund_or_recharge() {
        let mut m = Meter::new(50_000).unwrap();
        m.precharge(256).unwrap();
        m.enter().unwrap();
        assert_eq!(m.phase(), Phase::Running { instructions: 256 });
        assert_eq!(m.left(), 49_744);
        assert_eq!(m.refund_for_dispatch(), Err(Error::ChargePhase));
        assert_eq!(m.precharge(256), Err(Error::ChargePhase));
        m.complete_block().unwrap();
        assert_eq!(m.complete_block(), Err(Error::ChargePhase));
    }
    #[test]
    fn rejects_duplicate_or_missing_phase_transitions() {
        let mut m = Meter::new(1).unwrap();
        assert_eq!(m.enter(), Err(Error::ChargePhase));
        assert_eq!(m.paid_one(), Err(Error::ChargePhase));
        assert_eq!(m.refund_for_dispatch(), Err(Error::ChargePhase));
        m.precharge(2).unwrap();
        assert_eq!(m.precharge(2), Err(Error::ChargePhase));
        assert_eq!(m.enter(), Err(Error::ChargePhase));
        m.paid_one().unwrap();
        assert_eq!(m.paid_one(), Err(Error::ChargePhase));
        m.refund_for_dispatch().unwrap();
        assert_eq!(m.refund_for_dispatch(), Err(Error::ChargePhase));
    }
}
