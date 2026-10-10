use crate::{PlaybackRate, PlaybackStatus};
pub use seedbox_control::Action as FaultAction;
pub use seedbox_engine_protocol::intervention::Intervention;
use serde::{Deserialize, Serialize};

/// A mutation's request ID is scoped to its simulation and retained for that
/// simulation's lifetime. Reusing it with another command is a conflict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlRequest {
    pub request_id: String,
    pub command: ControlCommand,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ControlCommand {
    SetRate { rate: PlaybackRate },
    Step,
    Checkpoint,
    InjectFault { action: FaultAction },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlReceipt {
    pub request: ControlRequest,
    /// Revision of this control receipt, independent of the simulation revision.
    pub revision: u64,
    pub state: ControlState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum ControlState {
    Accepted,
    /// The worker processed the command at this boundary. A step acknowledgement
    /// admits one round; `stepping` stays true until that round actually commits.
    Acknowledged {
        status: PlaybackStatus,
        error: Option<seedbox_engine_protocol::host_control::ControlError>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        intervention: Option<Box<Intervention>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkpoint: Option<seedbox_engine_protocol::host_control::CheckpointStatus>,
    },
    /// The worker cannot implement this protocol. No mutation was applied.
    Unsupported,
    /// Execution ended before the server durably recorded an acknowledgement.
    /// The command may have taken effect. It will never be resent to a new worker.
    Interrupted,
}
