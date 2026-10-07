//! Where guest progress, and so virtual time, comes from. The machine's run
//! loop asks its timebase to bound each guest run before KVM_RUN and to judge
//! the run afterwards; everything else (devices, timers, injection, the world
//! scheduler) only sees the progress made and whether the exit point is
//! deterministic. `Counter` (host PMU branch counting with exact counted
//! stops) is the only timebase so far.

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

pub trait Timebase {
    /// Before KVM_RUN, `remaining` progress before the hard stop: arrange for
    /// the run to end in time.
    fn prepare(&mut self, m: &mut Machine, remaining: u64) -> Result<(), String>;
    /// Progress so far, read before KVM_RUN.
    fn start(&self) -> u64;
    /// Progress so far, read after KVM_RUN.
    fn read(&mut self) -> u64;
    /// Judge a finished run. Called while the exit is held, so it gets only
    /// the state in `Judge`.
    fn judge(&mut self, j: Judge<'_>, run: &Run) -> Result<Verdict, String>;
}
