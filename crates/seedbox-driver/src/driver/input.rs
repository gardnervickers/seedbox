//! Host-scheduled inputs: typed fault actions admitted through the control
//! policy that guest simulation-control calls use.
//!
//! See [`Barrier::schedule`](super::Barrier::schedule) for the enqueue,
//! admission and completion contract.

use super::MachineId;
use crate::{clock::ClockRate, execution::ExecutionConfig};
use std::{fmt, num::NonZeroI64, time::Duration};

/// Caller-chosen identity of one scheduled input: 1 to 128 characters from
/// `[A-Za-z0-9_-]`.
///
/// The key is echoed by the input's [`FaultOrigin::Input`](super::FaultOrigin::Input)
/// admission or its [`EventKind::InputRejected`](super::EventKind::InputRejected)
/// event. The driver refuses a key only while it is live (see
/// [`Barrier::schedule`](super::Barrier::schedule)); uniqueness across a whole
/// run belongs to the host, which owns history.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InputKey(String);

impl InputKey {
    /// Validate a key. Invalid text is returned unchanged inside the error.
    pub fn new(key: impl Into<String>) -> Result<Self, InvalidInputKey> {
        let key = key.into();
        let valid = (1..=128).contains(&key.len())
            && key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');
        if valid {
            Ok(Self(key))
        } else {
            Err(InvalidInputKey(key))
        }
    }

    /// The validated key text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Reconstruct a key that this crate already validated.
    pub(crate) fn validated(key: String) -> Self {
        debug_assert!(Self::new(key.clone()).is_ok());
        Self(key)
    }
}

impl fmt::Display for InputKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Text that is not a valid [`InputKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidInputKey(pub String);

impl fmt::Display for InvalidInputKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "input key {:?} must be 1 to 128 characters from [A-Za-z0-9_-]",
            self.0
        )
    }
}

impl std::error::Error for InvalidInputKey {}

/// Opaque identity of an installed fault that a later heal can undo, such as
/// a throttle. Its text form (`Display`) is the handle guest controls use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FaultHandle(pub(crate) String);

impl FaultHandle {
    /// Validate a caller-chosen fault name: text must be nonempty. There is no
    /// semantic length/alphabet bound; import allocation uses DecodeLimits.
    /// Named host faults share the active namespace with guest pN handles, but
    /// do not advance that allocator. Admission and apply-time checks are distinct.
    // Host aliases live in the Driver so guest heals, inspect and FIFO
    // collisions see them with exact admission races; a service-side map
    // installed after commit could not.
    pub fn named(text: impl Into<String>) -> Result<Self, String> {
        let text = text.into();
        if text.is_empty() {
            Err("fault handle must be nonempty".into())
        } else {
            Ok(Self(text))
        }
    }
}

impl fmt::Display for FaultHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The machine an action applies to.
///
/// Both forms are resolved when the input is admitted, through the same
/// control-policy view that guest calls use. `Name` is a machine name, which
/// is also its service name. A name must identify exactly one machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Machine(MachineId),
    Name(String),
}

/// A typed fault scheduled by the host.
///
/// Actions express admission policy, not raw machine work: machine requests
/// cannot represent Driver-owned pause/rate changes or target binding, and
/// this boundary lets host and guest controls share IDs and settlement rules.
///
/// Every action is admitted by the control policy at its scheduled time, with
/// the same availability preflight and state validation as an ordinary guest
/// control call, and settles like one. Admission binds the target's current
/// boot. See [`Barrier::schedule`](super::Barrier::schedule).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Action {
    /// Withhold the machine's execution, including ordinary wakeups and
    /// timers, while retaining its state and pending input. Admitted only for
    /// a running, unpaused machine; a paused or physically exited target is
    /// rejected.
    Pause(Target),
    /// Resume a paused machine at the effect's time. A zero-budget run adopts
    /// elapsed time and delivers overdue timers without replaying paused CPU
    /// work. Admitted only for a paused machine.
    Resume(Target),
    /// Crash the current boot without graceful shutdown, including while
    /// paused or after it shut down or exited by itself. Rejected for an
    /// already crashed boot, whether an applied Crash or a confirmed machine
    /// exit with [`ExitReason::Crashed`](crate::machine::ExitReason::Crashed)
    /// crashed it. Pending process commands of that boot settle Unavailable.
    Crash(Target),
    // The boot's confirmed state, not the cause or origin
    // of its crash, owns reboot eligibility. A confirmed self-crash is already
    // down; demanding a synthetic Crash would keep two contradictory views.
    /// Start a fresh boot of a crashed machine, clearing its driver pause and
    /// old blocked/timer state. Admitted once the current boot is confirmed
    /// crashed: by an applied Crash, or by a machine exit with
    /// [`ExitReason::Crashed`](crate::machine::ExitReason::Crashed) committed
    /// before admission. A boot that shut down or whose program exited must be
    /// crashed first. Later inputs bind to the new boot when they are admitted.
    Reboot(Target),
    /// Latch a graceful shutdown request for the current boot. Admitted for a
    /// live machine, including while paused; a physically exited or crashed
    /// target is rejected. Applied proves the request was delivered for
    /// admission, not that shutdown completes. The machine receives a
    /// zero-budget admission run; paused machines stay paused.
    Shutdown(Target),
    /// Stop the prepared application. Settled by the guest's acknowledgment.
    Stop { target: Target, deadline: Duration },
    /// Start the prepared application again. Settled by acknowledgment.
    Restart { target: Target, deadline: Duration },
    /// Freeze the prepared application. Settled by acknowledgment.
    PauseProcess { target: Target, deadline: Duration },
    /// Thaw a frozen application. Settled by acknowledgment.
    ResumeProcess { target: Target, deadline: Duration },
    /// Install an execution configuration and allocate a [`FaultHandle`].
    /// At most one throttle per machine is active or pending. Once applied, the
    /// setting survives later crashes and reboots, applies while paused or
    /// exited, and does not execute or wake the machine. Already charged
    /// partial work keeps its original cost. Like every effect, the pending
    /// throttle is bound to the boot current at admission: if that boot ends
    /// before the effect's turn, it settles Unavailable and installs nothing.
    Throttle {
        target: Target,
        config: ExecutionConfig,
    },
    /// Restore nominal execution cost for an active handle. The pending heal is
    /// bound to the target's boot at admission; if that boot ends first, the
    /// heal settles Unavailable and the handle stays active.
    Heal(HealTarget),
    /// Create the declared spawnable machine with this definition name,
    /// through the same admission path as a guest `spawn` call. Each name is
    /// admitted at most once from any origin; a name that is unknown or already
    /// admitted is rejected. The new identity is reserved at admission and the
    /// machine is built at its FIFO effect turn.
    Spawn { name: String },
    // Host clock faults share the guest control policy, handles and events
    // (`Transition::ClockRate`, `FaultAction::{Drift, StepRealtime}`); a raw
    // Driver setter would bypass admission, keys and settlement.
    /// Set the rate of the machine's time base from the effect's barrier and
    /// allocate a [`FaultHandle`]. The time-base mapping keeps the machine time
    /// already reached, so the offset accrued so far is kept. At most one drift
    /// per machine is active or pending. Once applied, the rate survives later
    /// crashes and reboots and does not execute or wake the machine; its
    /// pending timers are re-converted. Heal restores the nominal rate and
    /// keeps the accrued offset. Like every effect, the pending drift is bound
    /// to the boot current at admission.
    Drift { target: Target, rate: ClockRate },
    /// Draw a drift uniformly from the inclusive signed range bounded by
    /// `max_rate_ppb`, using the target's existing Driver-owned clock stream at
    /// admission. The bound must be positive and satisfy guest-control limits.
    /// Preparation projects on cloned policy/RNG state; only commit consumes
    /// the admitted draw. Fixed drift and rejected state checks draw nothing.
    // Host bounds enter the same admission transaction as guest bounds rather
    // than being resolved by a service-side RNG or alias policy.
    DriftBounded { target: Target, max_rate_ppb: u64 },
    /// Step the guest's `CLOCK_REALTIME` by `nanoseconds`, performed by the
    /// guest as a [`Request::StepRealtime`](crate::pv::Request::StepRealtime)
    /// and settled by its acknowledgment; allocates a [`FaultHandle`] whose heal
    /// sends the inverse step. |`nanoseconds`| must be at most 365 days, which
    /// also excludes `i64::MIN`; anything else is refused at enqueue. Like a
    /// process command it waits for the guest's readiness, with the default
    /// control deadline (5 s of simulation time from admission) bounding
    /// dispatch and acknowledgment. Linux refuses a step that would put
    /// realtime below `CLOCK_MONOTONIC`. The step ends with its boot; healing
    /// it afterwards changes nothing.
    StepRealtime {
        target: Target,
        nanoseconds: NonZeroI64,
    },
    /// Draw a realtime step from the inclusive signed range bounded by
    /// `max_offset_ns`, through the same target clock stream and state checks
    /// as a guest clock-skew call. A positive bound is at most 365 days; the
    /// resolved value may be zero. Handle and settlement semantics match
    /// `StepRealtime`, including the ordinary guest acknowledgment deadline.
    // The requested bound is retained until Driver admission so proposal
    // retries cannot consume or change the eventual committed draw.
    StepRealtimeBounded { target: Target, max_offset_ns: u64 },
    // A partition is admitted like any fault (keys, handles,
    // settlement) but acts on links between attached machines and outlives
    // their boots, so it names member sets rather than one boot-bound target.
    /// Cut both directions of every link between a `left` and a `right` member
    /// once the effect takes its FIFO turn, and allocate a [`FaultHandle`]. A
    /// frame is cut when it is routed after that turn, before loss or jitter
    /// is drawn; a frame stamped at the effect's time by the round ending
    /// there routes first and is not cut. Frames already in flight still arrive.
    /// Partitions of the same links stack, and healing one keeps the others. Sides must be nonempty and disjoint without repeats, and
    /// every member an admitted machine with a network interface in any
    /// lifecycle state; a declared machine not yet spawned is unavailable. The
    /// partition binds to no boot: crash, reboot and exit neither cancel nor
    /// end it. Heal restores only its links.
    Partition {
        left: Vec<Target>,
        right: Vec<Target>,
    },
    /// Install a directed from × to policy layer, validated against the global
    /// latency floor. The highest fault ID containing a pair wins. Healing
    /// removes only this layer/burst state and leaves queued frames untouched.
    // Separate from Partition because it is directed; callers supply canonical
    // validated policy data, not scheduler policy.
    Shape {
        from: Vec<Target>,
        to: Vec<Target>,
        policy: super::NetworkPolicy,
        network_faults: super::NetworkFaults,
    },
    /// Select one observed open TCP incarnation/direction. Argument errors
    /// precede handle allocation; unavailable/stale/capacity state rejection
    /// consumes an allocated pN, and apply rechecks the session before effects.
    // An incarnation selector belongs to network state, not machine lifecycle
    // or a guessed transport connection in the service worker.
    DropConnection { connection: super::Connection },
    // Disk and scheduler faults act inside the target machine, so their
    // effect is a machine request, not Driver state; the handle owns them.
    /// Install a disk or scheduler fault on the machine and allocate a
    /// [`FaultHandle`]. It is delivered as a
    /// [`Request::InstallFault`](crate::pv::Request::InstallFault) with a
    /// seed derived from the machine and the fault ID, survives crashes,
    /// reboots and device resets, and applies to every boot until healed.
    /// At most one preempt fault per machine is active or pending; disk
    /// faults stack. Heal settles with the fault's counts.
    DeviceFault {
        target: Target,
        fault: crate::pv::DeviceFault,
    },
}

/// The installed fault a [`Action::Heal`] undoes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealTarget {
    /// A handle reported in a [`FaultRecord`](super::FaultRecord), from any origin.
    Handle(FaultHandle),
    /// The active handle created by this key's admitted input. Rejected when
    /// the key's input is not admitted, created no handle, or its handle is
    /// not active.
    Input(InputKey),
}

/// Why the control policy refused a host input at its due time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputRejection {
    pub reason: RejectionReason,
    /// Human-readable diagnostic; not a stable interface.
    pub detail: String,
}

/// Classification of an [`InputRejection`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectionReason {
    /// The target's current state does not permit the action, or a heal names
    /// no active handle.
    InvalidState,
    /// The target cannot receive the action, for example because it exited.
    TargetUnavailable,
    /// The target cannot be resolved or the arguments are not admissible.
    InvalidRequest,
    /// The action is not supported.
    Unsupported,
}

impl Action {
    pub(super) fn bounded_clock_operation(&self) -> Option<crate::oracle::Operation> {
        let wire = |target: &Target| match target {
            Target::Machine(id) => crate::oracle::Target::Key(super::control::Control::source(*id)),
            Target::Name(name) => crate::oracle::Target::Machine(name.clone()),
        };
        match self {
            Self::DriftBounded {
                target,
                max_rate_ppb,
            } => Some(crate::oracle::Operation::ClockDrift {
                target: wire(target),
                rate_ppb: None,
                max_rate_ppb: Some(*max_rate_ppb),
            }),
            Self::StepRealtimeBounded {
                target,
                max_offset_ns,
            } => Some(crate::oracle::Operation::ClockSkew {
                target: wire(target),
                offset_ns: None,
                max_offset_ns: Some(*max_offset_ns),
            }),
            _ => None,
        }
    }
    pub(super) fn installs_handle(&self) -> bool {
        matches!(
            self,
            Self::Throttle { .. }
                | Self::Drift { .. }
                | Self::DriftBounded { .. }
                | Self::StepRealtime { .. }
                | Self::StepRealtimeBounded { .. }
                | Self::Partition { .. }
                | Self::Shape { .. }
                | Self::DropConnection { .. }
                | Self::DeviceFault { .. }
        )
    }
    pub(super) fn target(&self) -> Option<&Target> {
        match self {
            Self::Pause(target)
            | Self::Resume(target)
            | Self::Crash(target)
            | Self::Reboot(target)
            | Self::Shutdown(target)
            | Self::Stop { target, .. }
            | Self::Restart { target, .. }
            | Self::PauseProcess { target, .. }
            | Self::ResumeProcess { target, .. }
            | Self::Throttle { target, .. }
            | Self::Drift { target, .. }
            | Self::DriftBounded { target, .. }
            | Self::StepRealtime { target, .. }
            | Self::StepRealtimeBounded { target, .. }
            | Self::DeviceFault { target, .. } => Some(target),
            Self::Heal(_)
            | Self::Spawn { .. }
            | Self::Partition { .. }
            | Self::Shape { .. }
            | Self::DropConnection { .. } => None,
        }
    }

    pub(super) fn deadline(&self) -> Option<Duration> {
        match self {
            Self::Stop { deadline, .. }
            | Self::Restart { deadline, .. }
            | Self::PauseProcess { deadline, .. }
            | Self::ResumeProcess { deadline, .. } => Some(*deadline),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;

mod persist;
