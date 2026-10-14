//! Public host event stream published by [`Driver`](super::Driver).
//!
//! See [`Driver::drain_events`](super::Driver::drain_events) for the ordering,
//! failure and retention contract shared by every event.
//!
//! TODO: Re-evaluate: committed outputs and control settlements need a drainable stream.
//! Polling current state cannot reconstruct their order or a prefix preceding failure.
//! Events report owned facts; they do not claim transport delivery or a complete world view.

use super::{FaultHandle, InputKey, InputRejection, MachineId, MachineResponse, Position};
use crate::{
    clock::ClockRate, execution::ExecutionConfig, machine::Timestamp, pv::ApplicationCommand,
    pv::SimulationControlId,
};

/// One fact committed by the driver, in publication order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Dense publication index, starting at zero for the first event of the
    /// driver and increasing by one for every published event.
    pub sequence: u64,
    /// Committed position of the barrier that published this event. Facts
    /// confirmed by a barrier commit carry that commit's position. Facts
    /// confirmed by a failing step before its next commit carry the most recent
    /// committed position.
    pub position: Position,
    /// Simulation time of the fact itself. For machine outputs this is the
    /// occurrence time, which can precede `position.at`; for Driver-applied
    /// changes and control facts it is the time the fact took effect.
    pub at: Timestamp,
    /// What happened.
    pub kind: EventKind,
}

/// Kinds of published facts. Publication is evidence that the driver confirmed
/// the fact; it never reports enqueued, scheduled or speculative work.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EventKind {
    /// A committed machine response. Boot association is fixed at emission,
    /// including across reboots. Simulation-control calls are driver-owned:
    /// observing one transfers no dispatch or reply responsibility, and calls
    /// in the final oracle batch remain observable although terminal cutoff
    /// prevents their admission.
    Output(MachineResponse),
    /// A machine joined the simulation. Construction publishes one event per
    /// initial machine, oracle first, with [`Cause::Initial`];
    /// [`Barrier::add`](super::Barrier::add) publishes [`Cause::Host`]. Guest
    /// spawn publishes [`Cause::Fault`] before Applied settlement and reply
    /// admission, and before the new machine executes in the next round.
    #[non_exhaustive]
    MachineAdded { machine: MachineId, cause: Cause },
    /// A Driver-applied machine change took effect at `at` for `boot`, the
    /// boot current when it took effect. Only changes that took effect are
    /// published: a pause of an already paused or exited machine, or a resume
    /// of an active machine, publishes nothing.
    Transition {
        machine: MachineId,
        boot: u64,
        change: Transition,
        cause: Cause,
    },
    /// Control policy admitted a fault. Admission is not success; a matching
    /// [`EventKind::FaultSettled`] reports its outcome. A host input's
    /// admission carries [`FaultOrigin::Input`] with its key.
    FaultAdmitted(FaultRecord),
    /// Control policy refused a host input when it became due. `at` is its
    /// due time. No fault ID or handle was allocated, nothing was applied and
    /// the key is no longer live.
    InputRejected {
        key: InputKey,
        rejection: InputRejection,
    },
    /// The final outcome of an admitted fault. `at` is when the settlement was
    /// committed; `outcome_at` is when the outcome occurred, if known. Only
    /// [`SettlementStatus::Applied`] proves the change happened.
    FaultSettled {
        record: FaultRecord,
        status: SettlementStatus,
        outcome_at: Option<Timestamp>,
        detail: String,
    },
    /// A guest call armed an output trigger at `at`. Bytes observed at or
    /// after `deadline` never match.
    TriggerArmed {
        trigger: u32,
        source: MachineId,
        target: MachineId,
        deadline: Timestamp,
    },
    /// A trigger reached its final state. A fired trigger names the fault it
    /// admitted; firing proves admission, not the fault's outcome.
    TriggerResolved {
        trigger: u32,
        result: TriggerResult,
        fault: Option<FaultId>,
    },
    /// A simulation-control reply was admitted into the inbox of the calling
    /// machine's current boot at its timeline time `at`. This equals the legacy
    /// `delivered_at_ns`, but it proves neither transport completion nor that
    /// the program read the reply. Replies discarded because the machine exited
    /// or because the call belongs to an earlier boot publish nothing. A machine
    /// can still silently discard a reply to a session it retired within the
    /// same boot; that discard is not visible to the driver.
    ReplyAdmitted {
        machine: MachineId,
        boot: u64,
        id: SimulationControlId,
    },
}

/// A machine change applied by the driver at a barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transition {
    /// Positive execution became withheld.
    Paused,
    /// A paused machine became eligible for execution again.
    Resumed,
    /// A crash request was admitted. This also applies to an already exited
    /// machine, whose boot is logically terminated again.
    Crashed,
    /// A reboot request was admitted; `new_boot` is the boot that started.
    /// The event's `boot` is the boot that ended.
    Rebooted { new_boot: u64 },
    /// A graceful shutdown request was admitted to a live boot. This does not
    /// prove that shutdown completes. Each admitted request publishes again;
    /// requests to an exited boot publish nothing.
    ShutdownRequested,
    /// The execution configuration used by subsequent runs.
    ExecutionSet(ExecutionConfig),
    /// The rate of the machine's time base from `at` on. The mapping keeps
    /// the machine time already reached at `at`, so accrued offset is kept;
    /// the mapping is a pure function of the machine's addition time and
    /// these events. It survives crash and reboot.
    ClockRate(ClockRate),
}

/// Why the driver applied a change or added a machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Cause {
    /// Driver construction.
    Initial,
    /// A host barrier operation, such as [`Barrier::add`](super::Barrier::add).
    Host,
    /// Dispatch of an admitted fault from any origin, including host inputs
    /// scheduled with [`Barrier::schedule`](super::Barrier::schedule).
    Fault(FaultId),
}

/// Driver-assigned identity of an admitted fault, unique within one driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FaultId(pub(crate) u32);

/// Immutable description of an admitted fault, repeated in its settlement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultRecord {
    pub fault: FaultId,
    pub origin: FaultOrigin,
    pub target: MachineId,
    /// Target boot bound at admission. Spawn binds its reserved new identity
    /// at boot zero, including a subsequent construction failure.
    pub boot: u64,
    pub action: FaultAction,
    /// Handle that can later heal this fault, if it installs one; for a heal,
    /// the handle it restores.
    pub handle: Option<FaultHandle>,
    /// When the requester asked for the fault; for a guest call, the call's
    /// observation time, which can precede admission.
    pub requested_at: Timestamp,
}

/// Who requested a fault.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultOrigin {
    /// A guest simulation-control call from `machine`.
    Control {
        machine: MachineId,
        call: SimulationControlId,
    },
    /// A matched output trigger.
    Trigger { trigger: u32 },
    /// A host input scheduled with [`Barrier::schedule`](super::Barrier::schedule).
    /// Its `requested_at` is its admission time.
    Input { key: InputKey },
}

/// Requested fault effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum FaultAction {
    /// Create a declared machine, targeting its admission-order identity at boot zero.
    Spawn,
    Pause,
    Resume,
    Crash,
    Reboot,
    /// Latch graceful shutdown of the target boot (host inputs only).
    Shutdown,
    /// Install an execution configuration (throttle).
    SetExecution(ExecutionConfig),
    /// Operate on the prepared application.
    Application(ApplicationCommand),
    /// Set the rate of the machine's time base, resolved from any seeded bound.
    Drift(ClockRate),
    /// Step the guest's realtime clock by these nanoseconds, resolved from any
    /// seeded bound. Zero (a possible seeded draw) applies without a guest step.
    StepRealtime(i64),
    /// Restore the configuration replaced by an earlier fault's handle: nominal
    /// execution cost, nominal clock rate (keeping accrued offset), or the
    /// inverse realtime step. A step whose boot ended applies without effect.
    Heal,
}

/// Fault outcome. Unavailable and Failed never prove a mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettlementStatus {
    /// The change was confirmed.
    Applied,
    /// The target could not receive the change, for example because it ended.
    Unavailable,
    /// The target state rejected the change when it became due.
    Rejected,
    /// The outcome is uncertain; the driver fails with this settlement.
    Failed,
}

/// Final trigger state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerResult {
    /// The marker matched and the trigger's fault was admitted.
    Fired,
    /// The marker matched but control policy rejected the fault.
    Rejected,
    /// A guest call cancelled the trigger.
    Cancelled,
    /// The match deadline passed without a match.
    Expired,
    /// Oracle completion ended the trigger before it resolved.
    CutOff,
}
