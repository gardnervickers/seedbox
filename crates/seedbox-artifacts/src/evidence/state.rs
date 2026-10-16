//! Retained stopped-state metadata. Native state production belongs to backends.
use crate::{store::ContentDigest, Error, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Retained report beside the native page-hash files. Its presence supplies
/// physical addressing; it never changes the interpretation of legacy reports.
pub const STATE_REPORT: &str = "driver-state.json";

/// Actual mapped RAM, excluding guest physical address holes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RamRegion {
    pub base: u64,
    pub length: u64,
}
/// The physical boot whose stopped state was captured. Ended evidence is not
/// successor RAM and remains meaningful after its native worker is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateDisposition {
    Live,
    Crashed,
    Shutdown,
    Exited { code: i32 },
}
/// Backend-produced stopped-state identity and digests, not a snapshot hash.
/// The backend defines its combined-part encoding and computes every part from
/// real stopped state. `ram_regions` is SHA-256 of consecutive little-endian
/// `(base, length)` u64 pairs; both that part and `ram` enter the combined digest.
/// Page files contain one legacy eight-byte hash per mapped 4096-byte chunk,
/// concatenated by region; a region's final partial chunk is not padded.
// TODO: Re-evaluate: retained readers cannot depend on native Machine types;
// this data-only boundary preserves physical addressing and boot disposition.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateCapture {
    pub boot: u64,
    pub disposition: StateDisposition,
    pub digest: ContentDigest,
    pub parts: BTreeMap<String, ContentDigest>,
    pub regions: Vec<RamRegion>,
    /// Relative retained log path; independent of arbitrary machine-name text.
    pub page_log: String,
}
impl StateCapture {
    /// Check addressing, required parts and the region-part digest, returning
    /// the exact expected page-file byte length. This validates metadata, not
    /// the backend's CPU projection or the provenance of actual RAM bytes.
    pub fn page_bytes(&self) -> Result<u64> {
        if self.page_log.is_empty()
            || std::path::Path::new(&self.page_log)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(Error::Integrity(
                "invalid stopped-state page log path".into(),
            ));
        }
        if !self.parts.contains_key("ram") || self.regions.is_empty() {
            return Err(Error::Integrity("state capture lacks RAM evidence".into()));
        }
        let (digest, bytes) = region_layout(&self.regions)?;
        if self.parts.get("ram_regions") != Some(&digest) {
            return Err(Error::Integrity(
                "state capture RAM-region digest mismatch".into(),
            ));
        }
        Ok(bytes)
    }
}
pub(crate) fn region_layout(regions: &[RamRegion]) -> Result<(ContentDigest, u64)> {
    if regions.is_empty() {
        return Err(Error::Integrity("missing physical RAM regions".into()));
    }
    let mut encoded = vec![];
    let mut end = 0;
    let mut pages = 0u64;
    for region in regions {
        if region.length == 0 || !region.base.is_multiple_of(4096) || region.base < end {
            return Err(Error::Integrity("invalid state capture RAM regions".into()));
        }
        end = region
            .base
            .checked_add(region.length)
            .ok_or_else(|| Error::Integrity("state capture physical address overflow".into()))?;
        pages = pages
            .checked_add(region.length.div_ceil(4096))
            .ok_or_else(|| Error::Integrity("state capture page count overflow".into()))?;
        encoded.extend_from_slice(&region.base.to_le_bytes());
        encoded.extend_from_slice(&region.length.to_le_bytes());
    }
    let bytes = pages
        .checked_mul(8)
        .ok_or_else(|| Error::Integrity("state capture page count overflow".into()))?;
    Ok((ContentDigest::of(&encoded), bytes))
}
/// Full-state captures keyed by machine name. Missing machines are a producer
/// error, not successful empty evidence. Version denotes this data format only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateReport {
    pub version: u32,
    pub machines: BTreeMap<String, StateCapture>,
}
impl StateReport {
    /// Validate this retained format and each capture before publishing/reading.
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 || self.machines.is_empty() {
            return Err(Error::Integrity(
                "invalid stopped-state report version or empty machine inventory".into(),
            ));
        }
        let mut page_logs = BTreeSet::new();
        for capture in self.machines.values() {
            capture.page_bytes()?;
            if !page_logs.insert(std::path::Path::new(&capture.page_log)) {
                return Err(Error::Integrity(
                    "duplicate stopped-state page log path".into(),
                ));
            }
        }
        Ok(())
    }
}
