//! Serializable comparison evidence shared by manifests and offline diff.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// Diagnostic evidence only; never participates in identity or replay verdicts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHashDiff {
    pub differing_pfns: u64,
    /// Guest physical frame numbers, never sparse-vector indices.
    pub first_pfns: Vec<u64>,
    /// Guest physical addresses of the sampled pages.
    pub first_gpas: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamPageReport {
    pub machines: BTreeMap<String, PageHashDiff>,
    pub limitations: Vec<String>,
}
