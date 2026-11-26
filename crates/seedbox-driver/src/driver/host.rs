//! The Driver thread's execution context. Every machine runs on the thread
//! that steps the Driver: a blocking backend runs inline, and a split backend
//! keeps its run in flight in its own process while this thread polls it.

use super::Error;
use seedbox_machine::host::{
    doorbell::Doorbell,
    idle::{Idle, IdleStrategy},
};

pub(super) struct Host<C> {
    /// The context every machine call on this thread uses.
    context: C,
    /// How this thread waits for runs in flight.
    idle: Box<dyn IdleStrategy>,
    // Private unit-test doorbell, in place of the process's.
    #[cfg(test)]
    doorbell: Option<&'static Doorbell>,
}

impl<C> Host<C> {
    /// Build the context once, on the calling thread.
    pub(super) fn new<E>(context: impl FnOnce() -> Result<C, E>) -> Result<Self, Error<E>> {
        Ok(Self {
            context: context().map_err(Error::Context)?,
            idle: Idle::default().build(),
            #[cfg(test)]
            doorbell: None,
        })
    }

    /// The context for machine calls on this thread.
    pub(super) fn context(&mut self) -> &mut C {
        &mut self.context
    }

    /// Wait for runs in flight with `idle` instead.
    #[cfg(test)]
    pub(super) fn set_idle(&mut self, idle: Box<dyn IdleStrategy>) {
        self.idle = idle;
    }

    /// The doorbell that this Driver's worker processes ring, if any exists.
    pub(super) fn doorbell(&self) -> Option<&'static Doorbell> {
        #[cfg(test)]
        if self.doorbell.is_some() {
            return self.doorbell;
        }
        Doorbell::existing()
    }

    /// Use `doorbell` in place of the process's.
    #[cfg(test)]
    pub(super) fn set_doorbell(&mut self, doorbell: &'static Doorbell) {
        self.doorbell = Some(doorbell);
    }

    /// How this thread waits for runs in flight.
    pub(super) fn idle(&mut self) -> &mut dyn IdleStrategy {
        self.idle.as_mut()
    }
}
