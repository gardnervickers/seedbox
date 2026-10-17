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
mod submission;
pub use submission::{
    Link as SubmissionLink, Network as SubmissionNetwork, Simulator as SubmissionSimulator,
    Submission,
};
mod status;
pub use playback::{
    BoundaryPosition, PlaybackObservation, PlaybackRate, PlaybackStatus, RateMeasurement,
};
mod controls;
pub use controls::{
    CheckpointState, CheckpointStatus, ControlCommand, ControlReceipt, ControlRequest,
    ControlState, FaultAction, Intervention,
};

pub use events::{
    EventDelivery, EventKind, FaultStatus, MachineTransition, PreparationStage, SimulationEvent,
};
pub use http::{ApiError, ErrorCode, ServerInfo, SimulationList, SimulationSummary};
pub use id::{InvalidSimulationId, SimulationId};
pub use resource::{
    ApiVersion, EngineBuild, ExecutionOptions, InputSource, Metadata, ProgramInput,
    PropertyAssessment, ResourceKind, Simulation, SimulationDocument, SimulationSpec,
    SimulationStatus, SubmitRequest,
};
pub use status::{ExecutionTermination, Phase, Stage, StageTiming, TestVerdict};

/// Version of the local HTTP API, independent of retained worker formats.
pub const API_VERSION: &str = "seedbox/v1";

mod seeds;
pub use seeds::{SeedRangeRequest, SeedRangeStatus};

mod campaign;
pub use campaign::{
    AppliedFault, Campaign, CampaignAssessment, CampaignCompletion, CampaignMember, CampaignPhase,
    CampaignPlan, CampaignProperty, CampaignRequest, CampaignSummary, Encounter, EncounterPage,
    ExportState, Finding, FindingLabel, FindingQuery, MemberPlan, Moment, Shrink, ShrinkOutcome,
    ShrinkProbe, ShrinkRequest, MAX_PAGE, MAX_SHRINK_SEEDS,
};

mod analysis;
pub use analysis::{
    Analysis, AnalysisArm, AnalysisArmKind, AnalysisRequest, ExecRequest, MAX_ANALYSIS_RUNS,
    MAX_ANALYSIS_SAMPLES, MAX_ANALYSIS_TIMES,
};

mod world;
pub use world::{
    ApplicationCondition, ApplicationState, ChoiceInputState, FaultState, Frame, FrameDelivery,
    FrameFate, LinkCounters, LinkPolicy, LinkState, MachineCondition, MachineState,
    OutOfMemoryKill, OutOfMemoryProcess, TriggerState, TriggerStatus, WorldState,
    MAX_OUT_OF_MEMORY_KILLS,
};

mod timeline;
pub use timeline::{DeriveRequest, FaultTimeline, TimelineEntry, TimelineOrigin};

mod explanation;
pub use explanation::{Explanation, ExplanationEntry, ExplanationKind};
mod comparison;
pub use comparison::{ComparisonStatus, SimulationComparison};

mod capacity;
pub use capacity::{ExecutionCapacity, ExecutionOccupant, QueueReason, QueuedSimulation};

mod bundles;
mod diagnostics;
pub use bundles::*;
pub use diagnostics::{
    DiagnosticDocument, DiagnosticFile, DiagnosticFileQuery, DiagnosticFiles,
    MachineOutOfMemoryKill,
};
