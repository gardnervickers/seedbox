//! Tracks the passage of simulation time within the seedbox execution environment.
//!
//! A read-only view of time owned by the caller. The Driver gives each machine
//! its own view: that machine's time base, which the Driver maps to simulation
//! time (see the driver contract). Without a rate change the mapping is
//! the identity.

use std::fmt;

pub trait Clock {
    /// Returns the current time in nanoseconds of the observing machine's time
    /// base. For a machine run by the Driver this is its mapped simulation time.
    ///
    /// The owner advances time monotonically at simulation barriers. Machines
    /// observe this clock; their execution progress does not advance it.
    fn now(&self) -> u64;
}

/// Rate error of a machine's time base relative to simulation time, in parts
/// per billion: positive runs fast, negative slow. The magnitude is at most
/// 100_000_000 (ten percent). Zero is nominal.
///
/// The rate scales everything the machine experiences as time, including its
/// counters, timers and the CPU work it completes per simulation nanosecond
/// (one crystal for the whole machine). Execution pricing is not compensated.
// TODO: Re-evaluate: drift needs a validated, backend-neutral value shared by
// host actions, events and the Driver's mapping; a raw integer would let each
// owner reinterpret units and bounds.
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
mod tests;

#[cfg(any(test, feature = "test-support"))]
pub use crate::test_support::SimulationClock;
