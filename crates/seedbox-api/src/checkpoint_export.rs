use crate::{ApiError, Simulation};
use serde::{Deserialize, Serialize};

/// Request-scoped checkpoint export updates. EOF without Completed or Failed
/// is incomplete, never success. Closing the stream cancels only this export,
/// not the source simulation or checkpoint. No total-duration limit applies.
#[derive(Debug, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CheckpointExport {
    /// Monotonic byte work across hashing, copying and verification passes;
    /// this is not a file offset, percentage, or retained-object byte count.
    Progress {
        bytes_processed: u64,
    },
    Completed {
        simulation: Box<Simulation>,
    },
    Failed {
        error: ApiError,
    },
}
