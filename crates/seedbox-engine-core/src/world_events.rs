//! Deterministic event queue shared by frame and management delivery. Partition
//! decisions happen at send time; queued frames are retained regardless of later
//! partition/heal actions. Equal-time events use their insertion sequence.
use std::collections::BTreeMap;

pub fn pop_due<T>(queue: &mut BTreeMap<(u64, u64), T>, now: u64) -> Option<T> {
    let key = *queue.keys().next()?;
    (key.0 <= now).then(|| queue.remove(&key).unwrap())
}

/// Preserve nominal event time when recording counters for early delivery.
pub fn pop_due_at<T>(queue: &mut BTreeMap<(u64, u64), T>, now: u64) -> Option<(u64, T)> {
    let key = *queue.keys().next()?;
    (key.0 <= now).then(|| (key.0, queue.remove(&key).unwrap()))
}

/// Remove the earliest event if it is strictly before `t`.
pub fn pop_before<T>(queue: &mut BTreeMap<(u64, u64), T>, t: u64) -> Option<(u64, T)> {
    let key = *queue.keys().next()?;
    (key.0 < t).then(|| (key.0, queue.remove(&key).unwrap()))
}

/// Frames a paused machine keeps for its resume; later arrivals are dropped.
pub const PAUSED_INGRESS_FRAMES: u64 = 256;

/// What a frame finds when it reaches a machine that may be paused or down.
#[derive(Debug, PartialEq, Eq)]
pub enum Arrival {
    /// Queued as usual: delivered when the machine next runs at or after it.
    Queue,
    /// Queued behind a pause, counting against the paused backlog.
    Held,
    /// Lost at the receiver.
    Drop,
}

/// A frame arriving at `at`, before the machine's next lifecycle request at
/// `until`, meets the machine as it is now. A down machine has no NIC. A
/// paused one keeps up to `limit` frames for its resume, `held` so far.
pub fn arrival(at: u64, until: u64, down: bool, paused: bool, held: u64, limit: u64) -> Arrival {
    if at >= until || !(down || paused) {
        Arrival::Queue
    } else if down || held >= limit {
        Arrival::Drop
    } else {
        Arrival::Held
    }
}

/// Latest arrival time a machine at `time` may take now: up to `early`
/// ahead, but never at or past its next lifecycle request at `lifecycle`.
pub fn early_bound(time: u64, early: u64, lifecycle: u64) -> u64 {
    time.saturating_add(early)
        .min(lifecycle.saturating_sub(1))
        .max(time)
}
