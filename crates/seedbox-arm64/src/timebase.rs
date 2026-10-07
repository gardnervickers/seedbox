//! A minimal prepare/read/judge seam, independent of machine and host APIs.
use crate::Result;
#[derive(Clone, Copy, Debug)]
pub struct Publication {
    pub now: u64,
    pub bound: u64,
}
pub trait Timebase {
    fn prepare(&self) -> Publication;
    /// Reconcile guest-side reads at a deterministic exit.
    fn read(&mut self, last: u64) -> Result<()>;
    fn now(&self) -> u64;
    fn arm(&mut self, absolute_ns: u64) -> Result<()>;
    /// Idle may jump; ordinary compute between exits is free.
    fn judge(&mut self, idle: bool) -> Result<bool>;
}
