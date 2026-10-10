use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One directed pair's counts. Fields are in sorted key order, so the
/// snapshot serializes byte for byte as the sorted-key JSON object that
/// witnesses hash. Loss and queue drops are omitted while zero, so a run that
/// loses nothing keeps the original counter keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairCounts {
    pub delivered: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub loss_dropped: u64,
    pub partition_dropped: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub queue_dropped: u64,
    pub receiver_dropped: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Counts keyed `"from->to"`.
pub type Snapshot = BTreeMap<String, PairCounts>;
