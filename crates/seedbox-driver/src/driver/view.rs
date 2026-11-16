//! What the world looks like at a barrier, for a host that must decide what
//! to undo: the machines' states and the faults in force.
use super::{FaultHandle, MachineId};

/// A machine's state, as the oracle's `inspect` names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MachineState {
    /// The application runs.
    Running,
    /// The Driver withholds the machine's execution ([`Action::Pause`](super::Action::Pause)).
    Paused,
    /// The boot crashed; only a reboot runs the machine again.
    Down,
    /// The application was stopped by a command and can be restarted.
    Stopped,
    /// The application is frozen ([`Action::PauseProcess`](super::Action::PauseProcess)).
    Frozen,
    /// The application, or its machine, exited by itself with this status.
    Exited(i32),
}
impl std::fmt::Display for MachineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Running => f.write_str("Running"),
            Self::Paused => f.write_str("Paused"),
            Self::Down => f.write_str("Down"),
            Self::Stopped => f.write_str("Stopped"),
            Self::Frozen => f.write_str("Frozen"),
            Self::Exited(code) => write!(f, "Exited({code})"),
        }
    }
}

/// One machine of a [`WorldView`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineView {
    pub id: MachineId,
    /// Its declared name; a machine added through the low-level API has none.
    pub name: Option<String>,
    pub state: MachineState,
}

/// The machines in identity order and every active fault handle, whoever
/// installed it. See [`Barrier::view`](super::Barrier::view).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldView {
    pub machines: Vec<MachineView>,
    /// Handles a [`HealTarget::Handle`](super::HealTarget::Handle) can end,
    /// in the order `inspect` lists them.
    pub handles: Vec<FaultHandle>,
}
