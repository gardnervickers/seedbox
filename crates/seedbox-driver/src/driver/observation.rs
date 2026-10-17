//! A single local handoff for committed bookkeeping and optional descriptions.
use super::*;
#[cfg(test)]
mod tests;

/// A required committed change, emitted in the same order as its FIFO turn.
/// It is never capacity-limited and is not an observation transcript or replay
/// input. Consumers apply it once before publishing optional descriptions.
// TODO: Re-evaluate: service continuation cannot depend on optional Event copies;
// a typed local handoff preserves committed input/status accounting without a
// subscriber registry, a second scheduler, or a serialized history sidecar.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committed {
    pub position: Position,
    pub at: Timestamp,
    pub change: CommittedChange,
}

/// Continuation and result bookkeeping independent of observation retention.
// TODO: Re-evaluate: these are the existing commit facts required by the service,
// not callbacks or a separately admitted stream of simulation operations.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommittedChange {
    MachineAdded {
        machine: MachineId,
        cause: Cause,
    },
    Ready {
        machine: MachineId,
        boot: u64,
    },
    ApplicationOutcome {
        machine: MachineId,
        boot: u64,
        code: u8,
    },
    FaultAdmitted(FaultRecord),
    InputRejected {
        key: InputKey,
        rejection: InputRejection,
    },
    FaultSettled {
        record: FaultRecord,
        status: SettlementStatus,
        outcome_at: Option<Timestamp>,
        detail: String,
        network: Option<NetworkSettlement>,
    },
}

/// One drain of committed changes, retained events and bounded frame facts.
/// `omitted_events` counts actual discarded Event copies, and
/// `next_event_sequence` is the next observer-session sequence even if no copy
/// remains. Neither counter is execution authority or checkpoint state.
// TODO: Re-evaluate: one handoff prevents callers from advancing causal cursors
// using an optional event buffer, while preserving explicit loss watermarks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushResults {
    pub position: Position,
    pub changes: Vec<Committed>,
    pub events: Vec<Event>,
    pub frames: FrameFacts,
    pub omitted_events: u64,
    pub next_event_sequence: u64,
}

impl CommittedChange {
    pub(super) fn from_event(event: &EventKind) -> Option<Self> {
        Some(match event {
            EventKind::MachineAdded { machine, cause } => Self::MachineAdded {
                machine: *machine,
                cause: *cause,
            },
            EventKind::Output(MachineResponse {
                machine,
                boot,
                payload: Response::Ready,
                ..
            }) => Self::Ready {
                machine: *machine,
                boot: *boot,
            },
            EventKind::Output(MachineResponse {
                machine,
                boot,
                payload: Response::ApplicationOutcome { code },
                ..
            }) => Self::ApplicationOutcome {
                machine: *machine,
                boot: *boot,
                code: *code,
            },
            EventKind::FaultAdmitted(record) => Self::FaultAdmitted(record.clone()),
            EventKind::InputRejected { key, rejection } => Self::InputRejected {
                key: key.clone(),
                rejection: rejection.clone(),
            },
            EventKind::FaultSettled {
                record,
                status,
                outcome_at,
                detail,
                network,
            } => Self::FaultSettled {
                record: record.clone(),
                status: *status,
                outcome_at: *outcome_at,
                detail: detail.clone(),
                network: network.clone(),
            },
            _ => return None,
        })
    }
}
