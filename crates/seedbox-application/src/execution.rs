//! Contracts between orchestration, image preparation, and execution.
//!
//! Backends must enforce simulated/wall limits, honor cancellation, isolate their
//! output, and release any host reservation after stopping guests. A hardware
//! backend must acquire the shared host reservation *before* pinning a guest
//! to a core, and release it on normal return and catchable interruption. Neither
//! a preparer nor the orchestrator is permitted to perform these host mutations.

use crate::{config::Simulation, store::ContentStore, Result};

pub use seedbox_driver_execution::execution::*;

/// Resolves/builds images and retains their entire input closure in `store`.
/// No guest execution, CPU pinning, or host-state mutation is allowed here.
pub trait PreparationBackend: Send + Sync {
    /// Fast immutable runtime input checks, before any OCI work.
    fn validate_runtime_inputs(&self) -> Result<()> {
        Ok(())
    }
    fn prepare(&self, simulation: &Simulation, store: &ContentStore) -> Result<PreparedInputs>;
    /// Exploration preparation owns diagnostics independently of scratch inputs.
    /// Backends must explicitly support cancellation; the default fails before work.
    fn prepare_cancellable(
        &self,
        _simulation: &Simulation,
        _store: &ContentStore,
        _context: &crate::preparation::PreparationContext,
    ) -> Result<PreparedInputs> {
        Err(crate::Error::Setup(
            "preparation backend does not support exploration cancellation".into(),
        ))
    }
}
