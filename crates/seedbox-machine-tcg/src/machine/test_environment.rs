use crate::{
    clock::Clock,
    rng::{RngStream, SeededRng},
};
use seedbox_machine::machine::{Machine, StopReason, Timestamp};
use seedbox_machine::test_support::test_environment as shared;
pub(super) use shared::{
    check_deferred_startup_failure, check_lifecycle_fifo, check_queued_boot_entropy, run_requests,
};
use std::{
    ops::{Deref, DerefMut},
    path::Path,
    sync::Arc,
};
pub(super) struct Environment<M, R = SeededRng>(shared::Environment<M, R>);
impl<M, R> Deref for Environment<M, R> {
    type Target = shared::Environment<M, R>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<M, R> DerefMut for Environment<M, R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl<M: Machine, R> Machine for Environment<M, R> {
    type Context = M::Context;
    type Error = M::Error;
    type Snapshot = M::Snapshot;
    fn run(
        &mut self,
        context: &mut seedbox_machine::machine::RunContext<'_, Self::Context>,
        upto: Timestamp,
    ) -> Result<StopReason, Self::Error> {
        Machine::run(&mut self.0, context, upto)
    }
    fn snapshot(&mut self) -> Result<Self::Snapshot, Self::Error> {
        self.0.snapshot()
    }
}
#[cfg(unix)]
impl<R: RngStream> Environment<super::tcg::TcgMachine, R> {
    pub fn new(
        qemu: &Path,
        manifest: super::tcg::Manifest,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        let at = Timestamp::from_nanos(clock.now());
        let mut environment = Self::configured(qemu, manifest, rng, clock)?;
        environment.run(&mut (), at)?;
        Ok(environment)
    }
    pub fn configured(
        qemu: &Path,
        manifest: super::tcg::Manifest,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        Ok(Self(shared::Environment::configured(
            super::tcg::TcgMachine::new_raw(qemu, manifest)?,
            rng,
            clock,
        )))
    }
}
#[cfg(unix)]
impl Environment<super::tcg::TcgMachine> {
    pub fn from_snapshot(
        qemu: &Path,
        snapshot: &super::tcg::Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, super::tcg::Error> {
        Ok(Self(shared::Environment::configured(
            super::tcg::TcgMachine::from_snapshot(qemu, snapshot)?,
            SeededRng::new(0),
            clock,
        )))
    }
}

impl<M: Machine<Context = ()>, R: RngStream> Environment<M, R> {
    pub(super) fn run(&mut self, worker: &mut (), upto: Timestamp) -> Result<StopReason, M::Error> {
        self.0.run(worker, upto)
    }
    pub(super) fn request(
        &mut self,
        payload: seedbox_machine::pv::Request,
    ) -> Result<StopReason, M::Error> {
        self.0.request(payload)
    }
}
