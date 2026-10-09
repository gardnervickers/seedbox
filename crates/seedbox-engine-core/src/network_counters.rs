//! Inspection counts nominal network events strictly before an admission.
//! Packet DMA may happen in an early window, so physical totals cannot be sampled
//! directly. Pending deltas are drained at every global frontier, not retained as
//! packet history. The receiver's actual acceptance outcome remains authoritative.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Indices of a directed pair's `[u64; OUTCOMES]` link counts.
pub const DELIVERED: usize = 0;
pub const PARTITION_DROPPED: usize = 1;
pub const RECEIVER_DROPPED: usize = 2;
pub const LOSS_DROPPED: usize = 3;
pub const QUEUE_DROPPED: usize = 4;
pub const OUTCOMES: usize = 5;

/// Per directed (from, to) machine pair, counts indexed by outcome.
pub type LinkCounts = BTreeMap<(usize, usize), [u64; OUTCOMES]>;

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

/// The named snapshot of `counts`.
pub fn snapshot<'a>(name: impl Fn(usize) -> &'a str, counts: &LinkCounts) -> Snapshot {
    counts
        .iter()
        .map(|(&(from, to), counts)| {
            let c = PairCounts {
                delivered: counts[DELIVERED],
                loss_dropped: counts[LOSS_DROPPED],
                partition_dropped: counts[PARTITION_DROPPED],
                queue_dropped: counts[QUEUE_DROPPED],
                receiver_dropped: counts[RECEIVER_DROPPED],
            };
            (format!("{}->{}", name(from), name(to)), c)
        })
        .collect()
}

/// [`snapshot`] as a JSON value, for control replies and fault evidence.
pub fn snapshot_value<'a>(name: impl Fn(usize) -> &'a str, counts: &LinkCounts) -> Value {
    serde_json::to_value(snapshot(name, counts)).expect("counts are plain JSON")
}

#[derive(Default, Clone)]
pub struct NetworkCounters {
    totals: LinkCounts,
    pending: BTreeMap<u64, Vec<(usize, usize, usize)>>,
    frontier: u64,
}
impl NetworkCounters {
    pub fn record(&mut self, at: u64, from: usize, to: usize, outcome: usize) {
        if at < self.frontier {
            self.totals.entry((from, to)).or_default()[outcome] += 1;
        } else {
            self.pending
                .entry(at)
                .or_default()
                .push((from, to, outcome));
        }
    }
    pub fn advance(&mut self, before: u64) {
        assert!(
            before >= self.frontier,
            "network counter frontier moved backwards"
        );
        self.frontier = before;
        while let Some(due) = self.pending.first_entry().filter(|e| *e.key() < before) {
            for (from, to, outcome) in due.remove() {
                self.totals.entry((from, to)).or_default()[outcome] += 1;
            }
        }
    }
    pub fn counts(&self) -> &LinkCounts {
        &self.totals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Witnesses hash the serialized snapshot: it must keep the bytes of the
    /// sorted-key object it replaced.
    #[test]
    fn snapshot_serializes_as_the_sorted_key_object() {
        let names = ["a", "b"];
        let mut counts = LinkCounts::new();
        counts.insert((0, 1), [5, 1, 2, 0, 0]);
        counts.insert((1, 0), [7, 0, 0, 3, 4]);
        let typed = snapshot(|m| names[m], &counts);
        let mut legacy = json!({
            "a->b": {"delivered": 5, "partition_dropped": 1, "receiver_dropped": 2},
            "b->a": {"delivered": 7, "partition_dropped": 0, "receiver_dropped": 0},
        });
        legacy["b->a"]["loss_dropped"] = json!(3);
        legacy["b->a"]["queue_dropped"] = json!(4);
        assert_eq!(
            serde_json::to_vec(&typed).unwrap(),
            serde_json::to_vec(&legacy).unwrap()
        );
        assert_eq!(snapshot_value(|m| names[m], &counts), legacy);
        let back: Snapshot = serde_json::from_value(legacy).unwrap();
        assert_eq!(back, typed);
    }
}
