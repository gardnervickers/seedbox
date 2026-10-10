//! The engine report: what an engine tells the CLI about a finished run.
//!
//! The x86 engine serializes [`RunReport`] and the CLI deserializes it. The
//! row types ([`MachineOutcome`], [`FaultRecord`], [`ControlRecord`]) and
//! [`WorldEvidence`] are also how the CLI reads the ARM engine's report,
//! which shares them. Witnesses hash the engine's own evidence (fault rows,
//! the control log, the network counters), never these report types.

use crate::network_counters;
use crate::world_config::CheckpointConfig;
use seedbox_control::{Plan, TriggerRecord};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt;

/// A fault row's status, serialized as its snake_case name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowStatus {
    /// Planned; its time has not come.
    Unrequested,
    /// Its time came; it waits for its guest effect.
    Requested,
    Applied,
    Failed,
    Unavailable,
}

/// How a segment ended. Reports carry its snake_case name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The run reached its duration, or only paused or down machines remain.
    Duration,
    /// No machine can act again.
    Quiescent,
    /// The machine named by `end_on` exited.
    EndMachineExited,
    /// Stopped before anything happens at the checkpoint time.
    Checkpoint,
    /// Every machine parked at the fork point.
    Barrier,
    /// A fault or control request failed the run.
    FaultFailed,
}
impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Duration => "duration",
            Self::Quiescent => "quiescent",
            Self::EndMachineExited => "end_machine_exited",
            Self::Checkpoint => "checkpoint",
            Self::Barrier => "barrier",
            Self::FaultFailed => "fault_failed",
        }
    }
}
impl fmt::Display for RunOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl PartialEq<&str> for RunOutcome {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// The only x86 report version.
pub const REPORT_VERSION: u32 = 1;

/// A whole run: one segment, or one report per checkpoint branch or per
/// forked seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunReport {
    Checkpoints {
        report_version: u32,
        parent_identity: String,
        checkpoints: Vec<EngineReport>,
    },
    Seeds {
        report_version: u32,
        boot: BootSummary,
        seeds: Vec<EngineReport>,
    },
    Single(Box<EngineReport>),
}

/// Boot timing shared by every forked seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootSummary {
    pub seed: u64,
    pub barrier_ns: u64,
    pub boot_host_seconds: f64,
    pub snapshot_host_seconds: f64,
}

/// What every engine reports about a run's outcome and its control and fault
/// evidence.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorldEvidence {
    pub outcome: RunOutcome,
    pub world_witness: String,
    pub faults: Vec<FaultRecord>,
    pub fault_error: Option<String>,
    pub control_history: Vec<ControlRecord>,
    pub control_latency_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub triggers: Option<Vec<TriggerRecord>>,
}

/// The x86 engine's report of one run segment.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EngineReport {
    pub report_version: u32,
    /// The run seed; a checkpoint branch's own seed is in `checkpoint`.
    pub seed: u64,
    pub deferred_tx: bool,
    #[serde(flatten)]
    pub evidence: WorldEvidence,
    pub sim_seconds: f64,
    pub host_seconds: f64,
    /// Simulated over host seconds; `None` when that is not finite.
    pub speed: Option<f64>,
    pub slices: u64,
    pub frames_delivered: u64,
    pub frames_dropped: u64,
    pub network_counters: network_counters::Snapshot,
    pub network_witness: String,
    pub kills: Vec<Kill>,
    pub rejected_fault_effects: Vec<Value>,
    pub management_readiness: Vec<Value>,
    pub in_flight_policy: String,
    pub machines: Vec<MachineReport>,
    /// The scheduler's performance section.
    pub scheduler: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointConfig>,
    /// How each machine's entropy was reseeded at the checkpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_reseed: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot: Option<BootReport>,
}

/// How a booted world was released for this seed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootReport {
    pub seed: u64,
    pub barrier_ns: u64,
    pub fork_at_ns: Option<u64>,
    pub release_skew_ns: u64,
    pub boot_host_seconds: f64,
    /// Restored from the boot snapshot rather than run on from the boot.
    pub forked: bool,
    pub restored_pages: u64,
    pub restore_host_seconds: f64,
    /// Per machine, how its entropy was delivered.
    pub reseed: Value,
}

/// One stop of the seeded kill/restart schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kill {
    pub member: String,
    pub stop_ns: u64,
    pub start_ns: u64,
}

/// A machine's final state as reports and inspection show it: `Running`,
/// `Halted`, `Exited(code)`, `Paused` or `Down`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MachineState {
    Running,
    Halted,
    Exited(u8),
    Paused,
    Down,
}

impl MachineState {
    pub fn exited(self) -> bool {
        matches!(self, Self::Exited(_))
    }
}

impl fmt::Display for MachineState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("Running"),
            Self::Halted => f.write_str("Halted"),
            Self::Exited(code) => write!(f, "Exited({code})"),
            Self::Paused => f.write_str("Paused"),
            Self::Down => f.write_str("Down"),
        }
    }
}

impl std::str::FromStr for MachineState {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Ok(match s {
            "Running" => Self::Running,
            "Halted" => Self::Halted,
            "Paused" => Self::Paused,
            "Down" => Self::Down,
            _ => {
                let code = s
                    .strip_prefix("Exited(")
                    .and_then(|c| c.strip_suffix(')'))
                    .and_then(|c| c.parse::<u8>().ok())
                    .filter(|c| s == format!("Exited({c})"))
                    .ok_or_else(|| format!("unknown machine state {s:?}"))?;
                Self::Exited(code)
            }
        })
    }
}

impl Serialize for MachineState {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MachineState {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// What every engine reports about one machine's outcome.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineOutcome {
    pub name: String,
    pub state: MachineState,
    /// Final simulated time; an exited VM's clock stops at its exit.
    pub time_ns: u64,
    /// The application's exit status, from an explicit guest exit.
    pub guest_exit_code: Option<u8>,
    /// The guest init's failure status.
    pub guest_init_error: Option<u8>,
    /// The machine failed in guest bootstrap or the VMM.
    pub infrastructure_error: bool,
    pub witness: String,
    pub state_digest: Option<String>,
    /// Hashed state parts; `events` is descriptive text.
    pub state_digest_parts: Option<BTreeMap<String, String>>,
    /// An I/O operation still owed at the end. Absent from ARM reports.
    #[serde(default)]
    pub pending_io: Option<PendingIo>,
    /// Summaries of the incarnations this machine lost to crashes.
    #[serde(default)]
    pub crashed_incarnations: Vec<CrashedIncarnation>,
}

/// A crashed incarnation's summary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CrashedIncarnation {
    pub crashed_at_ns: u64,
    /// The rest of the summary, as the coordinator recorded it.
    #[serde(flatten)]
    pub details: serde_json::Map<String, Value>,
}

/// An I/O operation still owed when the run ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingIo {
    pub remaining_ns: u64,
    pub direction: IoDirection,
    pub port: u16,
    pub width: u8,
    pub count: u32,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoDirection {
    In,
    Out,
}

/// One x86 machine: its outcome and performance diagnostics.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MachineReport {
    #[serde(flatten)]
    pub outcome: MachineOutcome,
    pub incarnation: u32,
    pub booted_at_ns: u64,
    pub exits: u64,
    pub pio: u64,
    /// Port I/O exits of every incarnation.
    pub pio_total: u64,
    pub mmio: u64,
    pub halts: u64,
    pub irqs: u64,
    pub window_exits: u64,
    pub eintr: u64,
    pub run_ms: f64,
    /// Watchdog observations of a run with no exit and no guest progress.
    pub stalls: u64,
    pub yields: u64,
    pub yield_stops: u64,
    /// Guest RAM pages copied to and from the fork-point shadow.
    pub shadow: ShadowReport,
    pub kvm_ms_by_exit: BTreeMap<String, f64>,
    pub kvm_n_by_exit: Vec<u64>,
    pub control_channel: Option<ControlChannelReport>,
    /// The network device's diagnostics.
    pub net: Option<Value>,
    pub disk: Option<DiskReport>,
    pub timer: TimerReport,
    pub syscalls: u64,
    pub syscall_ns: u64,
    pub time_ns_by_cause: BTreeMap<String, u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowReport {
    pub synced_pages: u64,
    pub restored_pages: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlChannelReport {
    pub transport: String,
    pub tx_bytes: u64,
    pub rx_dma_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskReport {
    pub fault_model: crate::disk::DiskFaultModel,
    pub capacity_sectors: u64,
    pub base_sha256: Option<String>,
    pub digest: String,
    pub stable_sectors: u64,
    pub volatile_sectors: u64,
    pub reads: u64,
    pub writes: u64,
    pub flushes: u64,
    pub sectors_read: u64,
    pub sectors_written: u64,
    pub sectors_flushed: u64,
    pub errors: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerReport {
    pub writes: u64,
    pub overdue: u64,
    pub overdue_max_ns: u64,
    pub overdue_syscalls_max: u64,
    /// Guest clock against VMM time, by site.
    pub guest_clock_skew: BTreeMap<String, SkewReport>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkewReport {
    pub n: u64,
    pub ahead: u64,
    pub ahead_max_ns: u64,
    pub behind: u64,
    pub behind_max_ns: u64,
}

/// One fault's evidence row as a report carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaultRecord {
    pub action: String,
    pub applied_at_ns: Option<u64>,
    pub detail: String,
    pub id: u32,
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_counters: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_fault_counts: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_counts: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_ppb: Option<i64>,
    /// The plan as requested.
    pub request: Plan,
    pub requested_at_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_counts: Option<Value>,
    pub status: RowStatus,
}

/// One runtime control request in the control log: a reply, a request cut
/// off by the end of the run, or a frame rejected before it was a request.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ControlRecord {
    /// `rejected` or `cut_off`; absent for a delivered reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ControlStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<u64>,
    /// The request as the guest sent it.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub request: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ControlReply>,
    /// Timing of a request cut off before its reply.
    #[serde(flatten)]
    pub timing: ControlTiming,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_done: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_off_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_id: Option<u64>,
    /// The planned fault of a cut-off request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fatal: Option<bool>,
    /// A cut-off trigger request's result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// A rejected frame's machine, time and text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlStatus {
    Rejected,
    CutOff,
}

/// When a control request was made, admitted, took effect and was answered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlTiming {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admitted_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_at_ns: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replied_at_ns: Option<u64>,
}

/// A control reply as the controller received it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlReply {
    pub version: u64,
    /// The request's id.
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReplyError>,
    #[serde(flatten)]
    pub timing: ControlTiming,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplyError {
    pub code: String,
    pub message: String,
    /// The handle an unavailable effect still allocated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handle: Option<String>,
}

impl ControlRecord {
    /// The report form of an engine control log row.
    pub fn of(row: &Value) -> Result<Self, String> {
        Self::deserialize(row).map_err(|e| format!("control log row: {e}"))
    }
    /// The reply's timing, or for a request without one, the row's own.
    pub fn timing(&self) -> ControlTiming {
        self.response.as_ref().map_or(self.timing, |r| r.timing)
    }
    /// The operation's result: the reply's, or a cut-off trigger request's.
    pub fn result(&self) -> Option<&Value> {
        match &self.response {
            Some(reply) => reply.result.as_ref(),
            None => self.result.as_ref(),
        }
    }
    /// The handle of the fault this request planned: from the reply's
    /// result, or for a cut-off request, from its plan.
    pub fn planned_handle(&self) -> Option<&str> {
        match &self.response {
            Some(reply) => reply.result.as_ref()?.get("handle")?.as_str(),
            None => self.plan.as_ref()?.get("handle")?.as_str(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn machine_state_round_trips_its_display_text() {
        for (state, text) in [
            (MachineState::Running, "Running"),
            (MachineState::Halted, "Halted"),
            (MachineState::Exited(255), "Exited(255)"),
            (MachineState::Paused, "Paused"),
            (MachineState::Down, "Down"),
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), json!(text));
            assert_eq!(
                serde_json::from_value::<MachineState>(json!(text)).unwrap(),
                state
            );
        }
        for bad in [
            "Exited(256)",
            "Exited(01)",
            "Exited()",
            "running",
            "Exited(1",
        ] {
            assert!(bad.parse::<MachineState>().is_err(), "{bad}");
        }
    }

    #[test]
    fn control_rows_of_every_shape_read_as_records() {
        let reply = json!({"relay_id":1,"session_id":1,"fault_id":null,
            "request":{"version":1,"id":1,"op":"now"},
            "response":{"version":1,"id":1,"ok":true,"result":{"now_ns":5},
                "requested_at_ns":1,"admitted_at_ns":5,"effect_at_ns":5,"replied_at_ns":9},
            "delivered_at_ns":9});
        let record = ControlRecord::of(&reply).unwrap();
        assert_eq!(record.timing().replied_at_ns, Some(9));
        assert_eq!(record.fault_id, None);
        let failed = json!({"relay_id":2,"session_id":1,"fault_id":3,
            "request":{"version":1,"id":2,"op":"stop","target":{"service":"s"}},
            "response":{"version":1,"id":2,"ok":false,
                "error":{"code":"target_unavailable","message":"gone","handle":"h1"},
                "requested_at_ns":1,"admitted_at_ns":5,"effect_at_ns":6,"replied_at_ns":10},
            "status":"cut_off","cut_off_at_ns":10});
        let record = ControlRecord::of(&failed).unwrap();
        assert_eq!(record.status, Some(ControlStatus::CutOff));
        assert_eq!(
            record.response.unwrap().error.unwrap().handle.as_deref(),
            Some("h1")
        );
        let cut = json!({"relay_id":3,"session_id":2,"request":{"version":1,"id":1,"op":"now"},
            "status":"cut_off","requested_at_ns":1,"admitted_at_ns":5,"admitted_done":false,
            "effect_at_ns":null,"replied_at_ns":null,"cut_off_at_ns":4,"fault_id":null,
            "plan":null,"error":null,"fatal":false,"result":{}});
        let record = ControlRecord::of(&cut).unwrap();
        assert_eq!(record.timing().admitted_at_ns, Some(5));
        assert_eq!(record.admitted_done, Some(false));
        let rejected =
            json!({"status":"rejected","machine":"a","at_ns":3,"frame":"x","error":"bad"});
        let record = ControlRecord::of(&rejected).unwrap();
        assert_eq!(record.status, Some(ControlStatus::Rejected));
        assert!(record.request.is_null());
    }
}
