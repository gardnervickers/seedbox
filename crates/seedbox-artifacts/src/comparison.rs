//! Serializable comparison evidence shared by manifests and offline diff.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
/// Diagnostic evidence only; never participates in identity or replay verdicts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHashDiff {
    pub differing_pfns: u64,
    pub first_pfns: Vec<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RamPageReport {
    pub machines: BTreeMap<String, PageHashDiff>,
    pub limitations: Vec<String>,
}

/// One periodic sample of a machine's running exit witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessSample {
    pub time_ns: u64,
    /// Witness records hashed so far; one per trace line.
    pub records: u64,
    /// First eight witness bytes, as 16 hex digits.
    pub witness: String,
}

/// The first interval in which two runs' witness logs disagree. The first
/// differing exit lies after `last_match` and at or before the earlier of
/// the two differing samples.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    pub machine: String,
    /// One-based sample number of `a`/`b`.
    pub sample: u64,
    /// None when the first samples already differ.
    pub last_match: Option<WitnessSample>,
    /// None when that run's log ended first.
    pub a: Option<WitnessSample>,
    pub b: Option<WitnessSample>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DivergenceReport {
    pub divergence: Option<Divergence>,
    pub limitations: Vec<String>,
}
