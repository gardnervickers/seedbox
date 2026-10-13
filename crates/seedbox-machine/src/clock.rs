//! Tracks the passage of simulation time within the seedbox execution environment.
//!
//! A read-only view of time owned by the caller. The Driver gives each machine
//! its own view: that machine's time base, which the Driver maps to simulation
//! time (see [`crate::driver::Driver`]). Without a rate change the mapping is
//! the identity.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

mod projection;
pub(crate) use projection::Projection;

pub trait Clock {
    /// Returns the current time in nanoseconds of the observing machine's time
    /// base. For a machine run by the Driver this is its mapped simulation time.
    ///
    /// The owner advances time monotonically at simulation barriers. Machines
    /// observe this clock; their execution progress does not advance it.
    fn now(&self) -> u64;
}

/// Shared simulation time controlled by the driver.
pub(crate) struct SimulationClock(AtomicU64);

impl SimulationClock {
    pub(crate) fn new(nanos: u64) -> Self {
        Self(AtomicU64::new(nanos))
    }

    /// Set the time after the driver has established the scheduling position.
    pub(crate) fn set(&self, nanos: u64) {
        self.0.store(nanos, Ordering::Relaxed);
    }
}

impl Clock for SimulationClock {
    fn now(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// One machine's view of a fixed barrier, already mapped to its time base.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FixedClock(pub(crate) u64);

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

/// Rate error of a machine's time base relative to simulation time, in parts
/// per billion: positive runs fast, negative slow. The magnitude is at most
/// 100_000_000 (ten percent). Zero is nominal.
///
/// The rate scales everything the machine experiences as time, including its
/// counters, timers and the CPU work it completes per simulation nanosecond
/// (one crystal for the whole machine). Execution pricing is not compensated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClockRate(i32);

impl ClockRate {
    /// The nominal rate: machine time equals simulation time elapsed.
    pub const NOMINAL: Self = Self(0);

    /// Construct a rate with |ppb| <= 100_000_000.
    pub fn new(ppb: i64) -> Result<Self, InvalidClockRate> {
        if ppb.unsigned_abs() <= 100_000_000 {
            Ok(Self(ppb as i32))
        } else {
            Err(InvalidClockRate { ppb })
        }
    }

    /// Return the rate error in parts per billion.
    pub fn ppb(self) -> i32 {
        self.0
    }
}

/// A requested clock rate outside -100_000_000..=100_000_000 ppb.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidClockRate {
    pub ppb: i64,
}

impl fmt::Display for InvalidClockRate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "clock rate {} ppb is outside -100000000..=100000000",
            self.ppb
        )
    }
}
impl std::error::Error for InvalidClockRate {}

#[cfg(test)]
mod tests {
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
}
