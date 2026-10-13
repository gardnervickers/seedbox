//! Private host-side version-one oracle JSON schema, independent of byte relay.
//!
//! The guest bridge validates transport identity, session and relay sequencing
//! before passing a payload here. Identity failures are fatal; a valid identity
//! with invalid operation arguments receives a correlated operation error. This
//! module resolves no targets, allocates no handles, reads no clocks and mutates
//! no simulation. Runtime availability, causal admission, trigger budget, target
//! membership and effect completion belong to the private service core, which
//! the Driver invokes after full-round commit using its single event timeline.
//!
//! Every time in this schema is simulation time (`now_ns`, the `*_at_ns`
//! reply metadata, `sleep` durations and trigger match timeouts), never the
//! calling machine's drifted time base; see the Driver's time-base docs.
//!
//! Network operations are recognized for compatibility but explicitly deferred.
//! The legacy guest schema defines no disk RPC operation; disk policy is likewise
//! unsupported, and this parser does not invent disk operation names. Inspect
//! results use a generic typed serialization seam so the complete projection
//! (services, machines, active handles, network counters, optional triggers) can
//! be supplied by its owner without defining a misleading partial schema here.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, num::NonZeroU32};

const MAX_FRAME: usize = 16384; // Includes the public JSON frame's trailing newline.
const MAX_CLOCK_OFFSET_NS: u64 = 365 * 86_400 * 1_000_000_000;
const MAX_CLOCK_RATE_PPB: u64 = 100_000_000;
pub(crate) const PROCESS_EFFECT_TIMEOUT_NS: u64 = 5_000_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Target {
    Service(String),
    Machine(String),
    /// The owner's canonical identity key (`Facts::name`). Never on the wire.
    #[serde(skip)]
    Key(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Connection {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) src_addr: std::net::Ipv4Addr,
    pub(crate) dst_addr: std::net::Ipv4Addr,
    pub(crate) src_port: u16,
    pub(crate) dst_port: u16,
    pub(crate) incarnation: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct NetworkFaults {
    pub(crate) reorder_ppm: u64,
    pub(crate) reorder_delay_ns: u64,
    pub(crate) duplicate_ppm: u64,
    pub(crate) burst_loss_ppm: u64,
    pub(crate) burst_length: u64,
    pub(crate) latency_spike_ppm: u64,
    pub(crate) latency_spike_ns: u64,
    pub(crate) tcp_reset_ppm: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum TriggerFault {
    Stop {
        target: Target,
    },
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Operation {
    Now {},
    Spawn {
        machine: String,
    },
    Inspect {},
    Sleep {
        duration_ns: u64,
    },
    Stop {
        target: Target,
    },
    Restart {
        target: Target,
    },
    PauseProcess {
        target: Target,
    },
    ResumeProcess {
        target: Target,
    },
    Pause {
        target: Target,
    },
    Resume {
        target: Target,
    },
    Crash {
        target: Target,
    },
    Reboot {
        target: Target,
    },
    ClockSkew {
        target: Target,
        #[serde(default)]
        offset_ns: Option<i64>,
        #[serde(default)]
        max_offset_ns: Option<u64>,
    },
    ClockDrift {
        target: Target,
        #[serde(default)]
        rate_ppb: Option<i64>,
        #[serde(default)]
        max_rate_ppb: Option<u64>,
    },
    Throttle {
        target: Target,
        factor_permille: u64,
    },
    Heal {
        handle: String,
    },
    ArmTrigger {
        source: Target,
        marker: Vec<u8>,
        match_timeout_ns: u64,
        fault: TriggerFault,
    },
    CancelTrigger {
        trigger_id: u32,
    },
    // These wire shapes are parsed, but no network policy is implemented here.
    Connections {
        from: Target,
        to: Target,
        #[serde(default)]
        after: u64,
    },
    DropConnection {
        connection: Connection,
    },
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
    Shape {
        from: Vec<Target>,
        to: Vec<Target>,
        latency_ns: u64,
        jitter_ns: u64,
        loss_ppm: u64,
        #[serde(default)]
        bandwidth_bps: Option<u64>,
        #[serde(default)]
        queue_bound_ns: Option<u64>,
        #[serde(default)]
        network_faults: NetworkFaults,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrorCode {
    InvalidRequest,
    Unsupported,
    InvalidState,
    TargetUnavailable,
    // Reserved wire classification for failed backend effects.
    #[cfg_attr(not(test), allow(dead_code))]
    EffectFailed,
    ResultTooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OperationError {
    pub(crate) code: ErrorCode,
    pub(crate) message: String,
    /// Only target_unavailable errors carry a previously allocated handle.
    pub(crate) handle: Option<String>,
}
impl OperationError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: message.into(),
            handle: None,
        }
    }
    pub(crate) fn unsupported() -> Self {
        Self {
            code: ErrorCode::Unsupported,
            message: "operation is not supported by this backend".into(),
            handle: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProtocolError {
    Frame,
    Identity,
}

#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) id: NonZeroU32,
    pub(crate) operation: Result<Operation, OperationError>,
}
#[derive(Deserialize)]
struct Header {
    version: u32,
    id: NonZeroU32,
    #[serde(flatten)]
    _arguments: BTreeMap<String, Value>,
}
#[derive(Deserialize)]
struct WireRequest {
    #[serde(rename = "version")]
    _version: u32,
    #[serde(rename = "id")]
    _id: NonZeroU32,
    #[serde(flatten)]
    operation: Operation,
}

/// Parse public JSON only, excluding its newline and SBX_RPC1 envelope. A frame
/// with a valid identity always retains its ID even when arguments are refused.
pub(crate) fn parse(payload: &[u8]) -> Result<Request, ProtocolError> {
    if payload.len() >= MAX_FRAME || payload.contains(&0) || payload.contains(&b'\n') {
        return Err(ProtocolError::Frame);
    }
    let header: Header = serde_json::from_slice(payload).map_err(|_| ProtocolError::Identity)?;
    if header.version != 1 {
        return Err(ProtocolError::Identity);
    }
    let operation = serde_json::from_slice::<WireRequest>(payload)
        .map_err(|error| OperationError::invalid(format!("invalid request arguments: {error}")))
        .and_then(|wire| {
            wire.operation.validate()?;
            Ok(wire.operation)
        });
    Ok(Request {
        id: header.id,
        operation,
    })
}

impl Operation {
    fn validate(&self) -> Result<(), OperationError> {
        match self {
            Self::Connections { .. }
            | Self::DropConnection { .. }
            | Self::Partition { .. }
            | Self::Shape { .. }
            | Self::ArmTrigger {
                fault: TriggerFault::Partition { .. },
                ..
            } => {
                return Err(OperationError::unsupported());
            }
            Self::ClockSkew {
                offset_ns,
                max_offset_ns,
                ..
            } => {
                if !matches!((offset_ns, max_offset_ns), (Some(n), None) if *n != 0 && n.unsigned_abs() <= MAX_CLOCK_OFFSET_NS)
                    && !matches!((offset_ns, max_offset_ns), (None, Some(n)) if *n != 0 && *n <= MAX_CLOCK_OFFSET_NS)
                {
                    return Err(OperationError::invalid(format!(
                        "clock_skew needs exactly one of a nonzero offset_ns or a positive max_offset_ns, each at most {MAX_CLOCK_OFFSET_NS}"
                    )));
                }
            }
            Self::ClockDrift {
                rate_ppb,
                max_rate_ppb,
                ..
            } => {
                if !matches!((rate_ppb, max_rate_ppb), (Some(n), None) if *n != 0 && n.unsigned_abs() <= MAX_CLOCK_RATE_PPB)
                    && !matches!((rate_ppb, max_rate_ppb), (None, Some(n)) if *n != 0 && *n <= MAX_CLOCK_RATE_PPB)
                {
                    return Err(OperationError::invalid(format!(
                        "clock_drift needs exactly one of a nonzero rate_ppb or a positive max_rate_ppb, each at most {MAX_CLOCK_RATE_PPB}"
                    )));
                }
            }
            Self::Throttle {
                factor_permille, ..
            } if !(1000..=1_000_000).contains(factor_permille) => {
                return Err(OperationError::invalid(
                    "factor_permille must be in [1000, 1000000]",
                ));
            }
            Self::Heal { handle } if handle.is_empty() => {
                return Err(OperationError::invalid("handle must be nonempty string"));
            }
            Self::ArmTrigger {
                marker,
                match_timeout_ns,
                ..
            } if marker.is_empty() || marker.len() > 256 || *match_timeout_ns == 0 => {
                return Err(OperationError::invalid(
                    "trigger requires 1..=256 marker bytes and positive match timeout",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// Check duration/deadline arithmetic at the caller's committed admission
    /// barrier. Control adds no artificial transport delay. This neither chooses
    /// admission nor permits effects in peers' already committed past.
    /// Target resolution, current state, handles and trigger budget stay outside.
    pub(crate) fn validate_at(&self, admitted: u64) -> Result<(), OperationError> {
        self.validate()?;
        let checked = match self {
            Self::Sleep { duration_ns } => {
                admitted.checked_add(*duration_ns).ok_or("sleep overflow")
            }
            Self::ArmTrigger {
                match_timeout_ns, ..
            } => admitted
                .checked_add(*match_timeout_ns)
                .and_then(|n| n.checked_add(PROCESS_EFFECT_TIMEOUT_NS))
                .ok_or("trigger deadline overflow"),
            Self::Now {} | Self::Inspect {} | Self::CancelTrigger { .. } => Ok(admitted),
            _ => admitted
                .checked_add(PROCESS_EFFECT_TIMEOUT_NS)
                .ok_or("fault deadline overflow"),
        };
        checked.map(|_| ()).map_err(OperationError::invalid)
    }
}

/// Actual observation/admission/completion times are supplied by the dispatcher;
/// an unavailable effect may have no effect timestamp. Relay/session IDs belong
/// to the outer reply envelope and are never added to public JSON.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ReplyMetadata {
    pub(crate) requested_at_ns: u64,
    pub(crate) admitted_at_ns: u64,
    pub(crate) effect_at_ns: Option<u64>,
    pub(crate) replied_at_ns: u64,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum EncodeError {
    Json(String),
    ErrorFrameTooLarge,
}

/// Serialize a complete dispatcher-owned result. Inspect can use its complete
/// typed projection here; no incomplete network or machine state is synthesized.
/// Oversized success results become correlated result_too_large errors, matching
/// the public frame bound. Returned bytes exclude newline and transport envelope.
pub(crate) fn encode<T: Serialize>(
    id: NonZeroU32,
    metadata: ReplyMetadata,
    result: Result<&T, OperationError>,
) -> Result<Vec<u8>, EncodeError> {
    let mut reply = json!({
        "version": 1, "id": id, "ok": result.is_ok(),
        "requested_at_ns": metadata.requested_at_ns,
        "admitted_at_ns": metadata.admitted_at_ns,
        "effect_at_ns": metadata.effect_at_ns,
        "replied_at_ns": metadata.replied_at_ns,
    });
    match result {
        Ok(value) => {
            reply["result"] = serde_json::to_value(value)
                .map_err(|error| EncodeError::Json(error.to_string()))?;
        }
        Err(error) => put_error(&mut reply, error),
    }
    let mut bytes =
        serde_json::to_vec(&reply).map_err(|error| EncodeError::Json(error.to_string()))?;
    if bytes.len() >= MAX_FRAME && reply["ok"] == true {
        reply.as_object_mut().unwrap().remove("result");
        put_error(
            &mut reply,
            OperationError {
                code: ErrorCode::ResultTooLarge,
                message: "operation result exceeds response frame bound".into(),
                handle: None,
            },
        );
        bytes = serde_json::to_vec(&reply).map_err(|error| EncodeError::Json(error.to_string()))?;
    }
    if bytes.len() >= MAX_FRAME {
        return Err(EncodeError::ErrorFrameTooLarge);
    }
    Ok(bytes)
}
fn put_error(reply: &mut Value, error: OperationError) {
    reply["ok"] = json!(false);
    reply["error"] = json!({"code": error.code, "message": error.message});
    if error.code == ErrorCode::TargetUnavailable {
        if let Some(handle) = error.handle {
            reply["error"]["handle"] = json!(handle);
        }
    }
}

#[cfg(test)]
mod tests;

pub(crate) mod triggers;

mod handles;

pub(crate) mod service;

pub(crate) mod observation;
