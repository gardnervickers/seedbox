//! The Driver owns public-action compilation and engine capability for
//! execution, validation and preview. A worker first refuses actions its own
//! engine cannot carry out.
use seedbox_driver::driver::{faults::Engine, Action, FaultHandle};
use seedbox_engine_protocol::driver_world::Inventory;
use seedbox_spec::{backend::CpuBackend, definition::Architecture};

/// The engine an inventory runs: PV guests have the host's architecture,
/// and TCG emulates x86_64.
pub(super) fn engine(inventory: &Inventory) -> Engine {
    match inventory {
        Inventory::Pv { .. } => Engine {
            backend: CpuBackend::Pv,
            architecture: if cfg!(target_arch = "aarch64") {
                Architecture::Aarch64
            } else {
                Architecture::X86_64
            },
        },
        Inventory::Tcg { .. } => Engine {
            backend: CpuBackend::Tcg,
            architecture: Architecture::X86_64,
        },
    }
}

/// Refuse what `inventory`'s engine cannot carry out, then compile.
pub(super) fn translate(
    inventory: &Inventory,
    action: &seedbox_engine_protocol::control::Action,
    timeout_ns: u64,
) -> Result<(Action, Option<FaultHandle>), String> {
    seedbox_driver::driver::faults::check_engine(action, engine(inventory))?;
    seedbox_driver::driver::faults::translate(action, timeout_ns)
}
