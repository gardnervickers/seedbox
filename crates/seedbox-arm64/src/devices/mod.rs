//! Device effects are applied by Machine only at stopped-vCPU boundaries.
pub mod pl011;
// Shared management framing retains a legacy UART helper for its portable
// tests. Application machines use PL011 and virtio console exclusively.
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../../seedbox-vmm/src/devices.rs"]
mod legacy;
use crate::{pv::PvClock, timebase::Timebase, Result};
pub(crate) use legacy::Uart;
#[derive(Debug, PartialEq, Eq)]
pub enum Effect {
    None,
    Idle,
    Ack,
    Yield,
    Ready(u64),
    Arm(u64),
}
pub trait MmioDevice {
    fn read(&mut self, offset: u64, len: u8) -> Result<u64>;
    fn write(&mut self, offset: u64, len: u8, value: u64) -> Result<Effect>;
}
/// The page itself is RAM, not a faulting MMIO region. This view defines the
/// same register ABI for publication and portable tests. Guest owns `last`.
pub struct TimePage {
    pub now: u64,
    pub last: u64,
    pub bound: u64,
    pub deferred_irq_pending: bool,
}
impl TimePage {
    pub fn from_clock(c: &PvClock, last: u64, deferred_irq_pending: bool) -> Self {
        let p = c.prepare();
        Self {
            now: p.now,
            last,
            bound: p.bound,
            deferred_irq_pending,
        }
    }
}
impl MmioDevice for TimePage {
    fn read(&mut self, offset: u64, len: u8) -> Result<u64> {
        if len != 8 {
            return Err("time page requires 64-bit access".into());
        }
        match offset {
            0 => Ok(self.now),
            8 => Ok(self.last),
            16 => Ok(self.bound),
            24 => Ok(crate::ABI),
            32 => Ok(u64::from(self.deferred_irq_pending)),
            _ => Err(format!("unknown time-page offset {offset:#x}")),
        }
    }
    fn write(&mut self, offset: u64, len: u8, value: u64) -> Result<Effect> {
        if offset != 8 || len != 8 {
            return Err("guest may write only time-page last".into());
        }
        self.last = value;
        Ok(Effect::None)
    }
}
#[derive(Default)]
pub struct ClockEvent;
impl MmioDevice for ClockEvent {
    fn read(&mut self, _: u64, _: u8) -> Result<u64> {
        Err("clockevent is write-only".into())
    }
    fn write(&mut self, offset: u64, len: u8, value: u64) -> Result<Effect> {
        if len != 8 {
            return Err("clockevent requires 64-bit writes".into());
        }
        match offset {
            0 => Ok(Effect::Arm(value)),
            8 => Ok(Effect::Ack),
            16 => Ok(Effect::Idle),
            24 => Ok(Effect::Yield),
            32 => Ok(Effect::Ready(value)),
            _ => Err(format!("unknown clockevent offset {offset:#x}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn time_page_ownership_and_event_abi() {
        let mut page = TimePage {
            now: 42,
            last: 40,
            bound: 100,
            deferred_irq_pending: true,
        };
        assert_eq!(page.read(24, 8).unwrap(), crate::ABI);
        assert_eq!(page.read(16, 8).unwrap(), 100);
        assert_eq!(page.read(32, 8).unwrap(), 1);
        assert!(page.write(32, 8, 0).is_err());
        assert!(page.write(0, 8, 99).is_err());
        page.write(8, 8, 43).unwrap();
        assert_eq!(page.last, 43);
        let mut event = ClockEvent;
        assert_eq!(event.write(0, 8, 90).unwrap(), Effect::Arm(90));
        assert_eq!(event.write(16, 8, 1).unwrap(), Effect::Idle);
        assert_eq!(event.write(24, 8, 1).unwrap(), Effect::Yield);
        assert_eq!(event.write(32, 8, 1).unwrap(), Effect::Ready(1));
        assert!(event.write(0, 4, 90).is_err());
        assert!(event.read(0, 8).is_err());
    }
}
