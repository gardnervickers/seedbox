//! Best-effort live worker observations, separate from execution reports and
//! deterministic evidence. Logs and events consume a sequence even when lost;
//! optional frame batches consume one only when accepted by the socket.
use crate::application_log::Chunk;
use serde::{Deserialize, Serialize};

pub const MAX_DATAGRAM_BYTES: usize = 16 * 1024;

/// Size sockets explicitly: Darwin's default datagram send limit is 2048
/// bytes, below this protocol's maximum record. The bounded receive buffer
/// absorbs short bursts without blocking a worker on observer progress.
#[cfg(unix)]
pub fn configure_socket(socket: &std::os::unix::net::UnixDatagram) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    for (option, bytes) in [
        (libc::SO_SNDBUF, 64 * 1024i32),
        (libc::SO_RCVBUF, 256 * 1024i32),
    ] {
        // SAFETY: the descriptor is live and the value points to one initialized
        // integer with the size expected by these SOL_SOCKET options.
        let result = unsafe {
            libc::setsockopt(
                socket.as_raw_fd(),
                libc::SOL_SOCKET,
                option,
                (&bytes as *const i32).cast(),
                std::mem::size_of_val(&bytes) as libc::socklen_t,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        input_key: Option<String>,
        id: u32,
        action: String,
        machine: Option<String>,
        status: crate::report::RowStatus,
        applied_time_ns: Option<u64>,
        /// The admitted plan's action, so observers can tell which links or
        /// machines it affects. Absent from workers that predate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<Box<seedbox_control::Action>>,
        /// What the fault matched and did, once known: disk error, packet
        /// selection, network impairment or queue counts, by kind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        counts: Option<serde_json::Value>,
    },
    OverrideRejected {
        input_key: String,
        at_ns: u64,
        reason: String,
    },
    /// Frames routed since the previous batch, in routing order. Published
    /// only to an observation socket; never part of evidence.
    Frames {
        frames: Vec<Frame>,
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

/// One routed frame copy. `due_ns` is when a scheduled copy reaches its
/// receiver; dropped copies have none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Frame {
    pub from: String,
    pub to: String,
    pub sent_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ns: Option<u64>,
    pub bytes: u32,
    pub fate: FrameFate,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FrameFate {
    /// Queued for delivery at `due_ns`.
    Scheduled,
    PartitionDropped,
    LossDropped,
    QueueDropped,
    /// The receiver was down, or its paused-ingress backlog was full.
    ReceiverDropped,
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
    /// Effective nominal speed after throttle rounding and clamping; None is healed.
    Throttled {
        speed_hz: Option<u64>,
    },
}
