//! Where guest progress, and so virtual time, comes from. The machine's run
//! loop asks its timebase to bound each guest run before KVM_RUN and to judge
//! the run afterwards; everything else (devices, timers, injection, the world
//! scheduler) only sees the progress made and whether the exit point is
//! deterministic. Two timebases:
//!
//! - `Counter` (`machine/counted.rs`): host PMU branch counting with exact
//!   counted stops. Needs a qualified bare-metal CPU and MSR access.
//! - `PvClock` (`machine/pv.rs`): no counter. Compute costs no virtual time;
//!   time moves only at exits, syscalls and guest clock reads. Runs wherever
//!   KVM does, nested KVM included. A spin with no exit is caught by a
//!   watchdog and, when it is a provable state cycle, treated as idle.
//!
//! The timebase is part of a run's identity: the same seed gives different
//! runs under each, and their witnesses never match.

use crate::machine::Machine;
use crate::profile::CounterProfile;

/// One guest run as the run loop saw it.
pub struct Run {
    /// Progress made by the run.
    pub delta: u64,
    /// Progress left to the hard stop when the run started.
    pub remaining: u64,
    /// Progress left to the soft stop when the run started.
    pub rem_soft: u64,
    pub windowed: bool,
    pub window: bool,
    /// KVM_RUN returned EINTR (an overflow signal).
    pub interrupted: bool,
    /// The exit was a debug (breakpoint) exit.
    pub debug: bool,
    pub run_ns: u128,
    /// The guest made a syscall during the run.
    pub syscalled: bool,
}

/// Machine state a timebase may read or update while the exit is still held.
pub struct Judge<'a> {
    pub name: &'a str,
    pub stepping: bool,
    pub pv: bool,
    pub undo_valid: bool,
    pub fm: &'a mut u64,
    pub margin: &'a mut u64,
    pub adaptive_margin: bool,
    pub armed_wake: u64,
    pub profile: &'static CounterProfile,
    pub stats: &'a mut crate::machine::Stats,
}

pub enum Verdict {
    /// Accept the run. `legal`: its end is a deterministic point.
    Advance { legal: bool },
    /// Roll back to the held checkpoint and run again.
    Rollback,
    /// The run passed its stop and cannot be undone.
    Overshoot,
}

pub enum Prepared {
    /// Enter the guest.
    Run,
    /// Don't: the guest is spinning in a state cycle and is idle, like HLT.
    Idle,
}

pub trait Timebase {
    /// Progress is retired guest branches (also counted in `Machine::branches`).
    const BRANCHES: bool;
    /// Before KVM_RUN, `remaining` progress before the hard stop, and the
    /// soft stop's virtual time: arrange for the run to end in time.
    fn prepare(&mut self, m: &mut Machine, remaining: u64, soft: u64) -> Result<Prepared, String>;
    /// Progress so far, read before KVM_RUN; `syscalls` and `crossings`
    /// are the guest's counts.
    fn start(&mut self, syscalls: u64, crossings: u64) -> u64;
    /// Progress so far, read after KVM_RUN.
    fn read(&mut self, syscalls: u64, crossings: u64) -> u64;
    /// Pending interrupts are injected only when the hard stop is further
    /// away than this. Part of the execution semantics.
    fn inject_guard(&self) -> u64;
    /// Judge a finished run. Called while the exit is held, so it gets only
    /// the state in `Judge`.
    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String>;
}
