//! Translation between verified application inputs and native engine contracts.
//! Both platform executors use this boundary; it owns no hypervisor resources.
use crate::{execution::*, store::ContentDigest, Error, Result};
use seedbox_engine_core::checkpoint;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

// Preserve the public adapter paths while shared callers move to their owners.
pub use crate::execution::{
    page_hash_log, MACHINE_OUTPUT_DIR, VMM_FAILURE_MESSAGE, VOLUME_DISK_MIB,
};
pub use crate::fault_inputs::{fault_action_targets, validate_input_program};

/// The retained executable and its recorded dispatch, without consulting the host.
/// Run settings follow in its launch request (`worker_process`).
pub(crate) fn engine_command(
    engine: &Path,
    profile: &ExecutionProfile,
) -> Result<std::process::Command> {
    let mut command = std::process::Command::new(engine);
    match crate::engine::recorded_launch(profile)? {
        crate::engine::ExecutableKind::Standalone => {}
        crate::engine::ExecutableKind::InternalWorker => {
            command.args(["__engine", crate::engine::INTERNAL_WORKER_V1]);
        }
        crate::engine::ExecutableKind::ArmWorld => {
            command.args(["__engine", crate::engine::ARM_WORLD_V1]);
        }
        crate::engine::ExecutableKind::DriverWorld => {
            command.args(["__engine", crate::engine::DRIVER_WORLD_V1]);
        }
    }
    Ok(command)
}

/// The run-shaping machine features a profile records, as the x86 worker takes them.
pub(crate) fn recorded_features(
    profile: &ExecutionProfile,
) -> Result<seedbox_engine_protocol::launch::MachineFeatures> {
    Ok(seedbox_engine_protocol::launch::MachineFeatures {
        page_timer: recorded_page_timer(profile),
        deferred_tx: recorded_deferred_tx(profile)?,
    })
}

mod profile;
pub use profile::*;
mod report;
pub use report::*;
mod world;
pub use world::*;
