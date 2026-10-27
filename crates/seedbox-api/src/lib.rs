//! Public control-plane contracts. This crate has no executor, transport, host
//! discovery, or artifact storage dependencies.

mod fault_catalogue;
pub use fault_catalogue::{
    FaultCatalogue, FaultCategory, FaultDescriptor, FaultPreview, FaultPreviewLink,
    FaultPreviewRequest,
};
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
mod checkpoint_export;
pub use checkpoint_export::CheckpointExport;
mod controls;
pub use controls::{
    CheckpointState, CheckpointStatus, ControlCommand, ControlReceipt, ControlRequest,
    ControlState, FaultAction, Intervention,
};

pub use events::{
    EventDelivery, EventKind, FaultCounts, FaultStatus, MachineTransition, PreparationStage,
    SimulationEvent,
};
pub use http::{ApiError, ErrorCode, ServerInfo, SimulationList, SimulationSummary};
pub use id::{InvalidSimulationId, SimulationId};
pub use resource::{
    ApiVersion, ExecutionOptions, InputSource, Metadata, ResourceKind, Simulation,
    SimulationDocument, SimulationSpec, SimulationStatus, SubmitRequest,
};
pub use status::{ExecutionTermination, Phase, Stage, StageTiming, TestVerdict};

mod seeds;
pub use seeds::{SeedRangeRequest, SeedRangeStatus};

pub use seedbox_engine_protocol::out_of_memory::Kill as OutOfMemoryVictim;
mod properties;
pub use properties::{
    LostPropertyEvidence, PropertyAssessment, PropertyAssessmentStatus, PropertyExample,
    PropertyKind, PropertyOrigin, PropertyViolation,
};
mod world;
pub use world::{
    ApplicationCondition, ApplicationState, FaultState, Frame, FrameDelivery, FrameFate,
    LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState, OutOfMemoryKill,
    TriggerState, TriggerStatus, WorldState,
};

mod timeline;
pub use timeline::{DeriveRequest, FaultTimeline, TimelineEntry, TimelineOrigin};

mod comparison;
pub use comparison::{ComparisonStatus, SimulationComparison};

mod capacity;
pub use capacity::{ExecutionCapacity, ExecutionOccupant, QueueReason, QueuedSimulation};

mod bundles;
mod diagnostics;
pub use bundles::*;
pub use diagnostics::{
    DiagnosticDocument, DiagnosticFile, DiagnosticFileQuery, DiagnosticFiles, OutOfMemoryDiagnostic,
};
