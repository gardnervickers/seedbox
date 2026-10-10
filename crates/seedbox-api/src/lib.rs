//! Public control-plane contracts. This crate has no executor, transport, host
//! discovery, or artifact storage dependencies.

mod events;
mod logs;
pub use logs::{LogDelivery, LogEntry, LogStream};
mod http;
mod id;
mod playback;
mod resource;
mod status;
pub use playback::{
    BoundaryPosition, PlaybackObservation, PlaybackRate, PlaybackStatus, RateMeasurement,
};
mod controls;
pub use controls::{
    ControlCommand, ControlReceipt, ControlRequest, ControlState, FaultAction, Intervention,
};

pub use events::{
    EventDelivery, EventKind, FaultStatus, MachineTransition, PreparationStage, SimulationEvent,
};
pub use http::{ApiError, ErrorCode, ServerInfo, SimulationList, SimulationSummary};
pub use id::{InvalidSimulationId, SimulationId};
pub use resource::{
    ApiVersion, InputSource, Metadata, ResourceKind, Simulation, SimulationDocument,
    SimulationSpec, SimulationStatus, SubmitRequest,
};
pub use status::{ExecutionTermination, Phase, Stage, StageTiming, TestVerdict};

/// Version of the local HTTP API, independent of retained worker formats.
pub const API_VERSION: &str = "seedbox/v1";

mod seeds;
pub use seeds::{SeedRangeRequest, SeedRangeStatus};

mod world;
pub use world::{
    FaultState, Frame, FrameDelivery, FrameFate, LinkCounters, LinkPolicy, LinkState,
    MachineCondition, MachineState, WorldState,
};

mod comparison;
pub use comparison::{ComparisonStatus, SimulationComparison};
