//! Faults over the oracle's world control API.
//!
//! The fault vocabulary is main's [`Action`], the same type the world's fault
//! plans and timeline overrides use, with its validation
//! ([`control::validate_clock_skew`], [`control::validate_clock_drift`],
//! [`control::NetworkFaults::validate`]) and limits. A [`Profile`] is a
//! nemesis written as data.
use crate::{Error, Random};
pub use seedbox_engine_protocol::control::{self, Action};
use std::time::Duration;

pub mod profile;
pub use profile::Profile;

/// The world control API the oracle holds. Calls block until the world
/// confirms the effect; other machines keep running meanwhile.
///
/// The caller names the handle of every action that installs one
/// (`partition`, `shape`, `drop_connection`, `clock_skew`, `clock_drift`,
/// `throttle`) and ends it with [`Action::Heal`] of that handle. Lifecycle
/// actions end with their counterpart: `restart` after `stop`,
/// `resume_process` after `pause_process`, `resume` after `pause`, `reboot`
/// after `crash`.
pub trait World {
    /// Simulated time.
    fn now_ns(&mut self) -> Result<u64, Error>;
    /// Wait `duration` of simulated time.
    fn sleep(&mut self, duration: Duration) -> Result<(), Error>;
    fn apply(&mut self, action: &Action) -> Result<(), Error>;
}

/// How a fault pattern waits between faults: the world's simulated sleep,
/// or a loop that serves an oracle's connections while time passes.
pub trait Pace {
    fn wait(&mut self, world: &mut dyn World, duration: Duration) -> Result<(), Error>;
}

/// A fault schedule. [`Nemesis::run`] covers the whole workload phase: the
/// clients run until it returns, so the schedule's own waits (warm-up,
/// holds, a quiet tail) decide how long the workload lasts.
pub trait Nemesis {
    /// Inject faults, drawing every choice from `random`.
    fn run(&mut self, world: &mut dyn World, random: &mut dyn Random) -> Result<(), Error>;
    /// End every fault this nemesis still has in force, as Jepsen's final
    /// generator does before the final reads. Try every fault even when one
    /// fails, then report the first failure.
    fn heal(&mut self, world: &mut dyn World) -> Result<(), Error>;
}
