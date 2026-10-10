//! Project engine state into the shared report contract.
use crate::{faults::FaultRow, world::State};
pub use seedbox_engine_protocol::report::*;

/// A machine's reported state: down and paused take precedence over its
/// vCPU state.
pub fn machine_state(state: State, paused: bool, down: bool) -> MachineState {
    match state {
        _ if down => MachineState::Down,
        _ if paused => MachineState::Paused,
        State::Running => MachineState::Running,
        State::Halted => MachineState::Halted,
        State::Exited(code) => MachineState::Exited(code),
    }
}

pub fn fault_record(row: &FaultRow) -> Result<FaultRecord, String> {
    serde_json::from_value(row.json()).map_err(|e| format!("fault row {}: {e}", row.id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lifecycle_projection_prioritizes_down_then_paused() {
        assert_eq!(
            machine_state(State::Exited(3), true, true),
            MachineState::Down
        );
        assert_eq!(
            machine_state(State::Exited(3), true, false),
            MachineState::Paused
        );
        assert_eq!(
            machine_state(State::Exited(3), false, false),
            MachineState::Exited(3)
        );
        assert_eq!(
            State::Exited(3).to_string(),
            MachineState::Exited(3).to_string()
        );
    }
}
