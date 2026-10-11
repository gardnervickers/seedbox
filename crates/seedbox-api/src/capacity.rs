//! Host execution admission observations, never simulation inputs.
use crate::{Phase, SimulationId, Stage};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionCapacity {
    pub slots: usize,
    /// Reservations include preparation and final worker cleanup.
    pub occupying: Vec<ExecutionOccupant>,
    pub paused: usize,
    pub queued: Vec<QueuedSimulation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExecutionOccupant {
    pub simulation: SimulationId,
    /// Absent if the catalogue entry was deleted after completion but before join.
    pub phase: Option<Phase>,
    pub stage: Option<Stage>,
    pub paused: bool,
    /// Configured guest RAM, including the oracle; not measured host residency.
    pub guest_memory_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QueuedSimulation {
    pub simulation: SimulationId,
    pub reason: QueueReason,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum QueueReason {
    /// All resident slots are reserved. Pausing retains the reservation.
    ResidentCapacity,
    /// A slot is available; the execution owner has not dispatched this ID yet.
    AwaitingDispatch,
}
