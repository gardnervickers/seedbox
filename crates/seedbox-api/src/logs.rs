//! Application byte streams. Delivery gaps describe observation loss and have
//! no relationship to the oracle's result.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LogStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum LogEntry {
    Bytes {
        machine: String,
        incarnation: u32,
        simulated_time_ns: u64,
        stream: LogStream,
        bytes: Vec<u8>,
    },
    /// Records omitted before worker publication, attributed to one machine.
    MachineGap {
        machine: String,
        incarnation: u32,
        simulated_time_ns: u64,
        records: u64,
    },
    /// Sequence range lost between the worker and server, across all machines.
    SourceGap { first: u64, last: u64 },
    /// Malformed transport or a worker without live output support.
    SourceUnavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum LogDelivery {
    Entry {
        sequence: u64,
        entry: LogEntry,
    },
    /// Machine-stream sequence range lost by this client.
    Gap {
        first: u64,
        last: u64,
    },
    Closed,
}
