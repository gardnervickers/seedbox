//! Best-effort live worker observations, separate from execution reports and
//! deterministic evidence. A sequence is consumed even when delivery is lost.
use crate::application_log::Chunk;
use serde::{Deserialize, Serialize};

pub const MAX_DATAGRAM_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Observation {
    pub sequence: u64,
    pub machine: String,
    pub incarnation: u32,
    pub simulated_time_ns: u64,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Kind {
    /// Final sequence watermark; missing closure means terminal delivery is uncertain.
    Closed,
    Log(Chunk),
    MalformedLog,
    MachineReady,
    FaultChanged {
        id: u32,
        action: String,
        machine: Option<String>,
        status: crate::report::RowStatus,
        applied_time_ns: Option<u64>,
    },
    MachineLifecycle {
        transition: MachineTransition,
    },
    MachineExited {
        code: u8,
    },
    /// Records omitted by a bounded machine/round observation buffer.
    LogGap {
        records: u64,
    },
}

/// A successfully applied physical machine transition. Crash identifies the
/// ended incarnation; reboot identifies the newly started incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum MachineTransition {
    Paused,
    Resumed,
    Crashed,
    Rebooted,
    Throttled { factor_permille: u64 },
}
