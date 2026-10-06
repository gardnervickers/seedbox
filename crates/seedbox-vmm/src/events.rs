//! Deterministic event queue shared by frame and management delivery. Partition
//! decisions happen at send time; queued frames are retained regardless of later
//! partition/heal actions. Equal-time events use their insertion sequence.
use std::collections::BTreeMap;

pub fn pop_due<T>(queue: &mut BTreeMap<(u64, u64), T>, now: u64) -> Option<T> {
    let key = *queue.keys().next()?;
    (key.0 <= now).then(|| queue.remove(&key).unwrap())
}
