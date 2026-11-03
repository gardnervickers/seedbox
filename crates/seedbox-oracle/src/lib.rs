//! Jepsen-style scenarios for Seedbox oracles.
//!
//! You own the workload, the faults that matter and the checks; this library
//! supplies the shapes they plug into. A [`Scenario`] wires together:
//!
//! * [`Client`]s, one per concurrent process, that generate operations from
//!   the scenario's seeded [`Random`] source and perform them on the system;
//! * a [`History`] of invoke/ok/fail/info [`Record`]s with guest times;
//! * a [`Nemesis`] that applies the world's control [`Action`]s through the
//!   [`World`] while the clients run, and heals every fault still in force
//!   afterwards;
//! * a [`FinalRead`] phase that waits for recovery and reads each node;
//! * [`Checker`]s that turn the history and final reads into [`Verdict`]s.
//!
//! Each decided checker's verdict is evaluated as a guest `always` property
//! named by the checker, through `seedbox-guest`: valid holds and invalid
//! violates it, which fails the run. An unknown verdict evaluates nothing.
//! The oracle then exits with [`Report::exit_code`]: 1 when a check was
//! violated, and 3, incomplete, when one could not decide.
//!
//! The library supplies the [`run`]ner, the in-memory [`Recorder`] history,
//! the seeded [`Rng`], the guest control socket's [`GuestWorld`], Jepsen's
//! standard fault [`patterns`] and the [`queue`] checker. [`main`] runs a
//! scenario as an oracle program.
pub mod checker;
pub mod client;
pub mod history;
pub mod nemesis;
pub mod patterns;
pub mod queue;
pub mod random;
pub mod recorder;
pub mod runner;
pub mod scenario;
pub mod world;

pub use checker::{Checker, Read, Validity, Verdict};
pub use client::Client;
pub use history::{Completion, Event, History, Record};
pub use nemesis::{control, Action, Nemesis, Pace, World};
pub use random::{RandomExt, Rng};
pub use recorder::Recorder;
pub use runner::{final_reads, main, run, run_with, NoClient};
pub use scenario::{Checkers, FinalRead, Report, Scenario};
pub use world::GuestWorld;

/// A failure a scenario step reports; the caller decides whether it is fatal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Self(message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self(message.into())
    }
}

/// Seeded randomness. Under Seedbox the guest's entropy is drawn from the
/// run's seed, so a source seeded from it repeats with the seed; every choice
/// a client or nemesis makes should come from one.
pub trait Random {
    fn next_u64(&mut self) -> u64;
}
