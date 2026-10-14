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
/// TODO: Re-evaluate: actions express admission policy, not raw machine work.
/// Machine requests cannot represent Driver-owned pause/rate changes or target binding.
/// Keeping this boundary makes host and guest controls share IDs and settlement rules.
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
    /// paused or after it exited by itself. Rejected for an already crashed
    /// machine. Pending process commands of that boot settle Unavailable.
    Crash(Target),
    /// Start a fresh boot of a crashed machine, clearing its driver pause and
    /// old blocked/timer state. Admitted only after a crash has been applied;
    /// crash a machine that exited by itself before rebooting it. Later
    /// inputs bind to the new boot when they are admitted.
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
    // TODO: Re-evaluate: host clock faults must share the guest control
    // policy, handles and events (`Transition::ClockRate`,
    // `FaultAction::{Drift, StepRealtime}`); a raw Driver setter would bypass
    // admission, keys and settlement. Steps carry no deadline: the default applies.
    /// Set the rate of the machine's time base from the effect's barrier and
    /// allocate a [`FaultHandle`]. The time-base mapping keeps the machine time
    /// already reached, so the offset accrued so far is kept. At most one drift
    /// per machine is active or pending. Once applied, the rate survives later
    /// crashes and reboots and does not execute or wake the machine; its
    /// pending timers are re-converted. Heal restores the nominal rate and
    /// keeps the accrued offset. Like every effect, the pending drift is bound
    /// to the boot current at admission.
    Drift { target: Target, rate: ClockRate },
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
            | Self::StepRealtime { target, .. } => Some(target),
            Self::Heal(_) | Self::Spawn { .. } => None,
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
mod tests {
    use super::*;
    #[test]
    fn keys_accept_only_the_documented_alphabet_and_length() {
        for valid in ["a", "A-z_0", &"k".repeat(128)] {
            assert_eq!(InputKey::new(valid).unwrap().as_str(), valid);
        }
        for invalid in ["", " ", "a.b", "é", "a/b", &"k".repeat(129)] {
            assert_eq!(
                InputKey::new(invalid),
                Err(InvalidInputKey(invalid.to_owned()))
            );
        }
    }
}
