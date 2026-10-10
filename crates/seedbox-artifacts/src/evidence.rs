//! Backend-neutral retained evidence readers and paths.
use crate::{store::ContentDigest, Error, Result};
use seedbox_engine_protocol::report::MachineOutcome;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// State parts emitted by `Machine::state_digest`: `events` is descriptive
/// text, while every other part is a lowercase SHA-256 digest. Keep unknown
/// part names digest-checked too, so new hashed parts remain safe to compare.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(try_from = "BTreeMap<String, String>")]
pub struct StateDigestParts(BTreeMap<String, String>);

impl StateDigestParts {
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
        for (name, value) in &parts {
            if name != "events" {
                ContentDigest::try_from(value.clone())?;
            }
        }
        Ok(Self(parts))
    }
}

/// The machine rows and world witness of either engine's report.
#[derive(Deserialize)]
pub struct ReportMachines {
    pub world_witness: ContentDigest,
    pub machines: Vec<MachineOutcome>,
}

/// VMM `--out` directory relative to run output; recursive retention keeps this prefix.
pub const MACHINE_OUTPUT_DIR: &str = "machines";

/// Manifest-relative page evidence, beside the VMM's per-machine traces.
pub fn page_hash_log(machine: &str) -> String {
    format!("{MACHINE_OUTPUT_DIR}/{machine}.pages")
}
