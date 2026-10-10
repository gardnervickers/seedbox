//! Execution-profile parameters recorded in a run's identity.
//!
//! The settings are typed (`SemanticParameters`), so these accessors cannot
//! fail on a decoded profile; they keep their `Result` for existing callers.

use super::*;

/// Runtime checkpoint time and branch seed, when the run forks.
pub fn recorded_checkpoint(profile: &ExecutionProfile) -> Result<Option<(u64, u64)>> {
    Ok(profile
        .semantic_parameters
        .checkpoint
        .map(|checkpoint| (checkpoint.at_ns, checkpoint.branch_seed)))
}

/// The boot seed of a forked exploration attempt, if any.
pub fn recorded_boot_seed(profile: &ExecutionProfile) -> Option<u64> {
    profile.semantic_parameters.boot_seed
}

pub fn recorded_page_timer(profile: &ExecutionProfile) -> bool {
    profile.semantic_parameters.page_timer
}

pub fn recorded_deferred_tx(profile: &ExecutionProfile) -> Result<bool> {
    Ok(profile.semantic_parameters.deferred_tx)
}
