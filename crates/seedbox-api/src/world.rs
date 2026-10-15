//! The simulated world as observed by the server: machines, directed links and
//! faults. Clients discover every entity here rather than from the spec, so a
//! world whose machines change while it runs needs no client change.
use crate::{FaultStatus, MachineTransition, SimulationId};
pub use seedbox_engine_protocol::observation::{Frame, FrameFate};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorldState {
    /// Rejected external override keys and their reasons, independent of runtime fault IDs.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub override_rejections: std::collections::BTreeMap<String, String>,
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

/// Process state is independent of whole-machine pause/crash state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ApplicationCondition {
    #[default]
    Unknown,
    Starting,
    Running,
    Stopped,
    Paused,
    Exited,
    Unavailable,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ApplicationState {
    pub condition: ApplicationCondition,
    pub changed_at_ns: Option<u64>,
    pub exit_code: Option<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MachineState {
    pub name: String,
    pub incarnation: u32,
    pub condition: MachineCondition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<u8>,
    /// Configured nominal CPU speed; absent for machines not in the definition.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_speed_hz: Option<u64>,
    /// Effective nominal CPU speed in Hz after throttle rounding and clamping.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub throttle_speed_hz: Option<u64>,
    /// The workload oracle whose exit decides the verdict.
    pub oracle: bool,
    pub last_transition: Option<MachineTransition>,
    #[serde(default)]
    pub last_transition_at_ns: Option<u64>,
    #[serde(default)]
    pub condition_since_ns: Option<u64>,
    #[serde(default)]
    pub application: ApplicationState,
    /// Guest kernel out-of-memory kills on this machine, oldest first, across
    /// incarnations. Only the most recent [`MAX_OUT_OF_MEMORY_KILLS`] are kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub out_of_memory_kills: Vec<OutOfMemoryKill>,
}

/// Kills retained per machine; each needs the guest to exhaust its memory.
pub const MAX_OUT_OF_MEMORY_KILLS: usize = 64;

/// The process the guest kernel's out-of-memory killer chose, from its log.
pub use seedbox_engine_protocol::out_of_memory::Process as OutOfMemoryProcess;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OutOfMemoryKill {
    pub incarnation: u32,
    pub simulated_time_ns: u64,
    /// Absent when the kernel counted the kill but its log record was lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<OutOfMemoryProcess>,
}

impl MachineState {
    /// The kill that ended the current incarnation's application, if it exited.
    pub fn exited_out_of_memory(&self) -> Option<&OutOfMemoryKill> {
        if self.condition != MachineCondition::Exited {
            return None;
        }
        self.out_of_memory_kills
            .last()
            .filter(|kill| kill.incarnation == self.incarnation)
    }
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

/// Frames sampled by this server since the simulation's worker started.
/// Best-effort visualization can omit batches; these are not exhaustive
/// engine traffic counters.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_key: Option<String>,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    pub status: FaultStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_time_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<serde_json::Value>,
    /// An applied effect that persists until healed, reversed, or ended by lifecycle.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn world_accepts_unknown_action_payloads() {
        let document = serde_json::json!({
            "simulation":"k7m2p9qx","revision":1,"simulatedTimeNs":42,
            "machines":[],"links":[],"faults":[{
                "id":1,"inputKey":"future-input","action":"future_action",
                "status":"applied","plan":{"action":"future_action","novel":{"value":7}},"active":true
            }]
        });
        let world: WorldState = serde_json::from_value(document).unwrap();
        assert_eq!(world.faults[0].input_key.as_deref(), Some("future-input"));
        assert_eq!(world.faults[0].plan.as_ref().unwrap()["novel"]["value"], 7);
    }
}
