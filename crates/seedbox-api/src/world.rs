//! The simulated world as observed by the server: machines, directed links and
//! faults. Clients discover every entity here rather than from the spec, so a
//! world whose machines change while it runs needs no client change.
use crate::{FaultAction, FaultStatus, MachineTransition, SimulationId};
pub use seedbox_engine_protocol::observation::{Frame, FrameFate};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldState {
    pub simulation: SimulationId,
    /// Changes whenever any machine, link policy or fault changes. Counters
    /// alone do not advance it.
    pub revision: u64,
    /// Simulated time of the newest observation folded in.
    pub simulated_time_ns: u64,
    pub machines: Vec<MachineState>,
    pub links: Vec<LinkState>,
    pub faults: Vec<FaultState>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MachineCondition {
    /// Configured or rebooted, not yet reported ready.
    Starting,
    Ready,
    Paused,
    Crashed,
    Exited,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineState {
    pub name: String,
    pub incarnation: u32,
    pub condition: MachineCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
    /// CPU cost scale while a throttle is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_permille: Option<u64>,
    /// The workload oracle whose exit decides the verdict.
    pub oracle: bool,
    pub last_transition: Option<MachineTransition>,
}

/// One direction of a machine pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkState {
    pub from: String,
    pub to: String,
    pub policy: LinkPolicy,
    /// Handle of the partition cutting this direction, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partitioned_by: Option<String>,
    /// Handle of the shape overriding the base policy, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shaped_by: Option<String>,
    pub counters: LinkCounters,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkPolicy {
    pub latency_ns: u64,
    pub jitter_ns: u64,
    pub loss_ppm: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bandwidth_bps: Option<u64>,
}

/// Frames observed by this server since the simulation's worker started.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LinkCounters {
    pub scheduled: u64,
    pub partition_dropped: u64,
    pub loss_dropped: u64,
    pub queue_dropped: u64,
    pub receiver_dropped: u64,
    pub bytes: u64,
    /// Simulated time of the newest frame on this link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_sent_ns: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FaultState {
    pub id: u32,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub status: FaultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_time_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<FaultAction>,
    /// An applied partition or shape whose handle has not been healed.
    pub active: bool,
}

/// One batch of routed frames on the live frame stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum FrameDelivery {
    Frames {
        sequence: u64,
        simulated_time_ns: u64,
        frames: Vec<Frame>,
    },
    Gap {
        first: u64,
        last: u64,
    },
    Closed,
}
