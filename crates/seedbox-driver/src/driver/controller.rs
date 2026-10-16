//! Explicit service control authority. Omission retains the SDK contract.
use super::MachineId;
use std::time::Duration;

/// Guest controller identity and fixed admission/reply latency. This is opt-in:
/// omitted setup retains SDK behavior; an explicit absent controller disables
/// every guest caller. Host inputs keep their existing authorization and timing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControllerSetup {
    pub(super) controller: Option<MachineId>,
    pub(super) latency_ns: u64,
}
impl ControllerSetup {
    pub(super) fn new(
        controller: Option<MachineId>,
        latency: Duration,
        floor: u64,
    ) -> Result<Self, String> {
        let latency_ns = u64::try_from(latency.as_nanos())
            .map_err(|_| "control latency is not representable in nanoseconds")?;
        if latency_ns < floor {
            return Err("control latency is below the network causal bound".into());
        }
        Ok(Self {
            controller,
            latency_ns,
        })
    }
    /// The explicitly authorized machine, or disabled guest control.
    pub fn controller(self) -> Option<MachineId> {
        self.controller
    }
    /// Fixed semantic delay applied independently on admission and reply.
    pub fn latency(self) -> Duration {
        Duration::from_nanos(self.latency_ns)
    }
}
impl<M: crate::machine::Machine> super::Driver<M> {
    /// Effective guest-control authority; omission preserves the SDK contract.
    pub fn controller_setup(&self) -> Option<ControllerSetup> {
        self.control.controller
    }
}
wire_struct!(ControllerSetup {
    controller,
    latency_ns
});

#[cfg(test)]
mod tests;
#[cfg(test)]
mod timeline_tests;
