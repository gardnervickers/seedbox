//! Resolved image launch data retained with a run.
use crate::config::VolumeMount;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub argv: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_dir: String,
    pub user: String,
    /// Image-declared anonymous volumes need explicit per-run lifetime handling
    /// in the guest preparer. They cannot silently be discarded.
    pub volumes: Vec<String>,
    /// Named volumes on the machine's retained disk, which survive a crash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk_volumes: Vec<VolumeMount>,
}
