use serde::{Deserialize, Serialize};

/// Crash-loss policy for unflushed sectors. These fields are part of the disk
/// digest and retained run identity.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DiskFaultModel {
    pub sector_bytes: usize,
    pub survival_probability: f64,
}
