pub use seedbox_engine_protocol::host_control::{BoundaryPosition, PlaybackRate, RateMeasurement};
use serde::{Deserialize, Serialize};

/// A fresh worker observation. Querying it never advances the simulation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaybackStatus {
    pub position: BoundaryPosition,
    pub rate: PlaybackRate,
    #[serde(default)]
    pub achieved_rate: Option<RateMeasurement>,
    pub stepping: bool,
    pub terminal: bool,
    /// All initial applications have reported readiness at a committed boundary.
    #[serde(default)]
    pub application_ready: bool,
}

/// A cached live observation. Its age is measured when the resource is read;
/// a stalled or disconnected worker cannot masquerade as a fresh sample.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlaybackObservation {
    pub state: PlaybackStatus,
    pub age_ns: u64,
}
