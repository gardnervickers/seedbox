//! Live internal observations. Events describe committed changes and never
//! participate in the simulation's verdict or deterministic inputs.

use crate::{Phase, Stage};
pub use seedbox_engine_protocol::observation::MachineTransition;
pub use seedbox_engine_protocol::report::RowStatus as FaultStatus;
use serde::{Deserialize, Serialize};

/// Coarse preparation milestones; no container-engine text enters events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PreparationStage {
    Image,
    Guest,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SimulationEvent {
    pub simulation: crate::SimulationId,
    pub revision: u64,
    /// Absent before execution or when no guest position is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub simulated_time_ns: Option<u64>,
    /// Exact committed boundary when provided by the observed operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<crate::BoundaryPosition>,
    pub event: EventKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum EventKind {
    DeletionRequested,
    ExecutionTerminated {
        termination: crate::ExecutionTermination,
    },
    PreparationProgress {
        machine: String,
        stage: PreparationStage,
    },
    LifecycleChanged {
        phase: Phase,
        stage: Option<Stage>,
    },
    OracleExited {
        code: u8,
    },
    StopRequested,
    /// Status after a committed boundary. Instantaneous transitions within that
    /// boundary are coalesced; application time remains explicit.
    FaultChanged {
        id: u32,
        action: String,
        machine: Option<String>,
        status: FaultStatus,
        applied_time_ns: Option<u64>,
        /// The admitted action, so clients can tell what it affects.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<crate::FaultAction>,
    },
    MachineLifecycle {
        machine: String,
        incarnation: u32,
        transition: MachineTransition,
    },
    MachineReady {
        machine: String,
        incarnation: u32,
    },
    MachineExited {
        machine: String,
        incarnation: u32,
        code: u8,
    },
    /// Worker sequence range lost before the server received it. The range
    /// may contain application logs, engine events, or both.
    SourceGap {
        first: u64,
        last: u64,
    },
    /// A malformed observation or missing final worker watermark.
    SourceUnavailable,
    ControlChanged {
        control: Box<crate::ControlReceipt>,
    },
}

/// Delivery sequence is scoped to one live stream. Reconnection starts at the
/// current position; it does not request replay or a durable backlog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum EventDelivery {
    Event {
        sequence: u64,
        observation: SimulationEvent,
    },
    Gap {
        first: u64,
        last: u64,
    },
    Closed,
}
