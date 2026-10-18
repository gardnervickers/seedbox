//! Backend-neutral retained evidence readers and paths.
use crate::{store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
mod state;
pub(crate) use state::region_layout;
pub use state::{RamRegion, StateCapture, StateDisposition, StateReport, STATE_REPORT};
/// Hashed state parts from a stopped capture. Unknown names remain digest-checked.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub struct StateDigestParts(BTreeMap<String, String>);

impl StateDigestParts {
    pub(crate) fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
    pub fn ram_only_difference(&self, other: &Self) -> bool {
        self.0
            .get("ram")
            .zip(other.0.get("ram"))
            .is_some_and(|(a, b)| a != b)
            && self
                .0
                .iter()
                .filter(|(k, _)| k.as_str() != "ram")
                .eq(other.0.iter().filter(|(k, _)| k.as_str() != "ram"))
    }
}

impl TryFrom<BTreeMap<String, String>> for StateDigestParts {
    type Error = Error;

    fn try_from(parts: BTreeMap<String, String>) -> Result<Self> {
        for value in parts.values() {
            ContentDigest::try_from(value.clone())?;
        }
        Ok(Self(parts))
    }
}

/// Per-machine output directory relative to retained run output.
pub const MACHINE_OUTPUT_DIR: &str = "machines";

/// Manifest-relative page evidence, beside per-machine traces.
pub fn page_hash_log(machine: &str) -> String {
    format!("{MACHINE_OUTPUT_DIR}/{machine}.pages")
}
