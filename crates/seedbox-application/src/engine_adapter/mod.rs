//! Translation between verified application inputs and native engine contracts.
//! Both platform executors use this boundary; it owns no hypervisor resources.
use crate::{execution::*, store::ContentDigest, Error, Result};
use seedbox_engine_core::checkpoint;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

/// Execution failure whose VMM stderr may be excerpted by the CLI.
pub const VMM_FAILURE_MESSAGE: &str = "VMM failed; see vmm.stderr";

pub use seedbox_artifacts::evidence::{page_hash_log, MACHINE_OUTPUT_DIR};

/// The engine executable with the worker entry point for `kind`. Run settings
/// follow in its launch request (`worker_process`).
pub(crate) fn engine_command(
    engine: &Path,
    kind: crate::engine::ExecutableKind,
) -> std::process::Command {
    let mut command = std::process::Command::new(engine);
    command.args(["__engine", kind.contract()]);
    command
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
