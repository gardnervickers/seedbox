//! Tracks the passage of simulation time within the seedbox execution environment.
//!
//! A read-only view of time owned by the caller. The Driver gives each machine
//! its own view: that machine's time base, which the Driver maps to simulation
//! time (see the driver contract). Without a rate change the mapping is
//! the identity.

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
};

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
// A validated newtype shared by host actions, events and the Driver's mapping;
// a raw integer would let each owner reinterpret units and bounds.
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

/// Largest magnitude of a boot's realtime offset, in nanoseconds: the few
/// milliseconds by which NTP-disciplined hosts on one network disagree.
pub const REALTIME_OFFSET_BOUND_NS: i64 = 5_000_000;

/// The kernel argument carrying a boot's realtime offset to the guest's init.
pub const REALTIME_OFFSET_ARGUMENT: &str = "seedbox.clock_offset_ns=";

// Software that seeds randomness from the wall clock draws the same values
// on every machine when they share one; the offset gives each boot its own
// wall clock. It comes from the boot seed every backend already draws from
// the machine's stream, so it draws nothing and moves no other seeded value.
/// The realtime offset of the boot whose seed is `boot_seed`: uniform over
/// `-REALTIME_OFFSET_BOUND_NS..=REALTIME_OFFSET_BOUND_NS`. The guest's init
/// starts `CLOCK_REALTIME` at the epoch plus the machine counter plus this
/// offset; monotonic and boot clocks and the machine's time base keep it out.
pub fn boot_realtime_offset_ns(boot_seed: u64) -> i64 {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(
        [
            b"seedbox boot realtime offset v1\0".as_slice(),
            &boot_seed.to_le_bytes(),
        ]
        .concat(),
    );
    let value = u64::from_le_bytes(digest[..8].try_into().expect("eight digest bytes"));
    let span = 2 * REALTIME_OFFSET_BOUND_NS as u64 + 1;
    (value % span) as i64 - REALTIME_OFFSET_BOUND_NS
}

/// `command_line` with the realtime offset argument of the boot whose seed
/// is `boot_seed` appended.
pub fn boot_command_line(command_line: &str, boot_seed: u64) -> String {
    let offset = boot_realtime_offset_ns(boot_seed);
    if command_line.is_empty() {
        format!("{REALTIME_OFFSET_ARGUMENT}{offset}")
    } else {
        format!("{command_line} {REALTIME_OFFSET_ARGUMENT}{offset}")
    }
}

/// Shared simulation time controlled by the driver.
pub struct SimulationClock(AtomicU64);

impl SimulationClock {
    pub fn new(nanos: u64) -> Self {
        Self(AtomicU64::new(nanos))
    }

    /// Set the time after the driver has established the scheduling position.
    pub fn set(&self, nanos: u64) {
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
pub struct FixedClock(pub u64);

impl Clock for FixedClock {
    fn now(&self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests;
