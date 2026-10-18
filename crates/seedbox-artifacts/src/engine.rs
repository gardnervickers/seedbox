//! Recorded Driver executable dispatch; descriptive metadata is not admission.
use crate::{execution::ExecutionProfile, Error, Result};
use serde::{Deserialize, Serialize};
pub const DRIVER_WORLD_V1: &str = seedbox_engine_protocol::driver_world::CONTRACT;
pub const SIMULATOR_VERSION: &str = env!("CARGO_PKG_VERSION");
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutableKind {
    #[serde(rename = "driver-world-v1")]
    DriverWorld,
}
pub fn recorded_launch(profile: &ExecutionProfile) -> Result<ExecutableKind> {
    profile.semantic_parameters.launch().ok_or_else(|| {
        Error::Integrity("execution profile has no Driver executable descriptor".into())
    })
}
