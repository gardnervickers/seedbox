use crate::{
    clock::Clock,
    rng::{RngStream, SeededRng},
};
use seedbox_machine::machine::{Machine, StopReason, Timestamp};
use seedbox_machine::test_support::test_environment as shared;
pub(super) use shared::{
    check_deferred_startup_failure, check_lifecycle_fifo, check_queued_boot_entropy,
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
#[cfg(seedbox_pv)]
impl<R: RngStream> Environment<super::pv::PVMachine, R> {
    pub fn new(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        let at = Timestamp::from_nanos(clock.now());
        let mut environment = Self::configured(executable, kernel, initrd, rng, clock)?;
        environment.run(&mut (), at)?;
        Ok(environment)
    }
    pub fn configured(
        executable: &Path,
        kernel: Vec<u8>,
        initrd: Vec<u8>,
        rng: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, std::io::Error> {
        Ok(Self(shared::Environment::configured(
            super::pv::PVMachine::from_boot_inputs(executable, kernel, initrd)?,
            rng,
            clock,
        )))
    }
}
#[cfg(seedbox_pv)]
impl Environment<super::pv::PVMachine> {
    pub fn from_snapshot(
        executable: &Path,
        snapshot: &super::pv::Snapshot,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Result<Self, String> {
        Ok(Self(shared::Environment::configured(
            super::pv::PVMachine::from_snapshot(executable, snapshot)?,
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
