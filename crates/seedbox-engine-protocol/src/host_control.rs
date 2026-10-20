//! Datagrams of the host-control socket between the CLI and a running engine.
//! The engine acknowledges each request at a world-slice boundary.

use serde::{Deserialize, Serialize};

/// An observation of committed scheduler state, separate from stream delivery
/// sequences and catalogue revisions. A segment transition need not run a round.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BoundaryPosition {
    /// Monotonic within a timeline, including boundaries that execute no round.
    #[serde(default)]
    pub boundary_index: u64,
    pub completed_rounds: u64,
    pub simulated_time_ns: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Boundary {
    pub position: BoundaryPosition,
    pub terminal: bool,
    /// Every machine has reported application readiness at least once.
    pub application_ready: bool,
}

/// Simulated seconds per host second. Rational rates avoid floating point
/// ambiguity; zero speed is represented by Paused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
pub enum PlaybackRate {
    Paused,
    Limited {
        numerator: std::num::NonZeroU32,
        denominator: std::num::NonZeroU32,
    },
    #[default]
    Unlimited,
}

/// Measured progress since the current rate was selected (or the first boundary).
/// Dividing simulated time by wall time gives achieved simulated seconds per
/// wall second. Keep the exact interval instead of rounding a floating-point rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RateMeasurement {
    pub simulated_time_ns: u64,
    pub wall_time_ns: std::num::NonZeroU64,
}

/// Pause at the next slice boundary; acknowledged with `PAUSED`.
pub const PAUSE: &[u8] = b"pause";
/// Leave a pause; acknowledged with `RUNNING`.
pub const RESUME: &[u8] = b"resume";
pub const PAUSED: &[u8] = b"paused";
pub const RUNNING: &[u8] = b"running";
/// Reply to any other request.
pub const INVALID: &[u8] = b"invalid";
/// Largest datagram either side sends or reads.
pub const MAX_DATAGRAM: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    /// Monotonically increasing within one worker. The server serializes
    /// requests; retries of the latest identical request return its first reply.
    pub id: u64,
    pub command: Command,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum Command {
    Status,
    SetRate {
        rate: PlaybackRate,
    },
    /// One-shot playback stop before the next fault, without changing inputs.
    PauseBeforeNextFault {
        rate: PlaybackRate,
    },
    Step,
    /// Request a stopped-world capture; acceptance is not capture completion.
    Checkpoint {
        request_id: String,
    },
    /// Prepare at the current committed horizon and hold execution until commit.
    PrepareFault {
        request_id: String,
        action: crate::control::Action,
        timeout_ns: u64,
    },
    /// The server has durably retained the exact prepared intervention.
    CommitFault {
        request_id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Reply {
    pub id: u64,
    pub position: BoundaryPosition,
    pub terminal: bool,
    /// Every machine has reported application readiness at least once.
    #[serde(default)]
    pub application_ready: bool,
    pub rate: PlaybackRate,
    #[serde(default)]
    pub achieved_rate: Option<RateMeasurement>,
    /// Step admission is acknowledged immediately. Status becomes false only
    /// after a real round commits (or a later rate command cancels the step).
    pub stepping: bool,
    /// A one-shot pause remains armed for the next fault effect.
    #[serde(default)]
    pub before_next_fault: bool,
    pub error: Option<ControlError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intervention: Option<crate::intervention::Intervention>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CheckpointStatus {
    pub request_id: String,
    pub position: BoundaryPosition,
    pub state: CheckpointState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CheckpointState {
    Requested,
    Captured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ControlError {
    Completed,
    StaleRequest,
    RequestConflict,
    NotPaused,
    InvalidRate,
    StepPending,
    FaultPending,
    NoFaultPrepared,
    NotReady,
    Unsupported,
    InvalidFault,
    CheckpointPending,
    InvalidCheckpoint,
}
