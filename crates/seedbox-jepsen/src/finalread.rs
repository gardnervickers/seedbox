//! The final-read discipline Jepsen's final generator follows: heal every
//! fault still in force (cuts, pauses, crashes, stops), start any node whose
//! process exited on its own (an OOM kill, say), wait for readiness, let the
//! replicas converge, and only then read from every node.
use crate::kit::{evidence, pause, Ctl};
use crate::nemesis;
use serde_json::json;
use std::time::{Duration, Instant};

/// Heal everything, then wait for `ready`; when that fails, restart the
/// nodes `exited` reports and wait again. Returns whether the nodes are ready.
pub fn recover(
    ctl: &mut Ctl,
    nodes: &[&str],
    mut ready: impl FnMut(&str) -> bool,
    exited: impl Fn(&str) -> bool,
) -> bool {
    ctl.heal_everything();
    if ready("final") {
        return true;
    }
    let exited: Vec<&str> = nodes.iter().copied().filter(|n| exited(n)).collect();
    for node in &exited {
        if let Err(error) = nemesis::step(ctl, "restart", &[node]) {
            evidence("restart-failed", json!({"node": node, "error": error.0}));
        }
    }
    !exited.is_empty() && ready("final-after-restart")
}

/// Poll every node's `view` once a second until their `key`s agree for
/// `agree_for` seconds, or `seconds` pass regardless; the last views and
/// whether they agreed.
pub fn settle<V, K: PartialEq>(
    nodes: &[&str],
    seconds: f64,
    agree_for: f64,
    view: impl Fn(&str) -> V,
    key: impl Fn(&V) -> K,
) -> (Vec<V>, bool) {
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let mut since: Option<Instant> = None;
    let mut views: Vec<V> = Vec::new();
    while Instant::now() < deadline {
        views = nodes.iter().map(|n| view(n)).collect();
        if views.windows(2).all(|pair| key(&pair[0]) == key(&pair[1])) {
            if since.get_or_insert_with(Instant::now).elapsed()
                >= Duration::from_secs_f64(agree_for)
            {
                return (views, true);
            }
        } else {
            since = None;
        }
        pause(1.0);
    }
    (views, false)
}

/// Read every node in order.
pub fn read_each<R>(nodes: &[&str], mut read: impl FnMut(&str) -> R) -> Vec<(String, R)> {
    nodes.iter().map(|n| (n.to_string(), read(n))).collect()
}
