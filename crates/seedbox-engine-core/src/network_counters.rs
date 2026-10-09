//! Inspection counts nominal network events strictly before an admission.
//! Packet DMA may happen in an early window, so physical totals cannot be sampled
//! directly. Pending deltas are drained at every global frontier, not retained as
//! packet history. The receiver's actual acceptance outcome remains authoritative.
use serde_json::{json, Value};
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

/// Counts keyed `"from->to"`. Loss and queue drops are omitted while zero, so
/// a run that loses nothing keeps the original counter keys.
pub fn snapshot<'a>(name: impl Fn(usize) -> &'a str, counts: &LinkCounts) -> Value {
    let snapshot: BTreeMap<_, _> = counts
        .iter()
        .map(|(&(from, to), counts)| {
            let mut c = json!({
                "delivered": counts[DELIVERED],
                "partition_dropped": counts[PARTITION_DROPPED],
                "receiver_dropped": counts[RECEIVER_DROPPED],
            });
            if counts[LOSS_DROPPED] > 0 {
                c["loss_dropped"] = json!(counts[LOSS_DROPPED]);
            }
            if counts[QUEUE_DROPPED] > 0 {
                c["queue_dropped"] = json!(counts[QUEUE_DROPPED]);
            }
            (format!("{}->{}", name(from), name(to)), c)
        })
        .collect();
    json!(snapshot)
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
