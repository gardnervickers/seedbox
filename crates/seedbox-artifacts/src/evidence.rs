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
        let ram_changed = ["ram", "ram_pages"].into_iter().any(|part| {
            self.0
                .get(part)
                .zip(other.0.get(part))
                .is_some_and(|(a, b)| a != b)
        });
        let non_ram = self
            .0
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "ram" | "ram_pages"));
        let other_non_ram = other
            .0
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "ram" | "ram_pages"));
        ram_changed && non_ram.eq(other_non_ram)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_only_difference_allows_derived_page_hashes_but_no_other_state_change() {
        let parts = |ram: &[u8], pages: &[u8], cpu: &[u8]| {
            StateDigestParts::try_from(BTreeMap::from([
                ("ram".into(), ContentDigest::of(ram).hex().into()),
                ("ram_pages".into(), ContentDigest::of(pages).hex().into()),
                ("cpu".into(), ContentDigest::of(cpu).hex().into()),
            ]))
            .unwrap()
        };
        let baseline = parts(b"ram A", b"pages A", b"cpu A");
        assert!(baseline.ram_only_difference(&parts(b"ram B", b"pages B", b"cpu A")));
        assert!(!baseline.ram_only_difference(&parts(b"ram B", b"pages B", b"cpu B")));
        assert!(baseline.ram_only_difference(&parts(b"ram A", b"pages B", b"cpu A")));
        assert!(!baseline.ram_only_difference(&parts(b"ram A", b"pages A", b"cpu A")));
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
