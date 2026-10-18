//! Jepsen's lock-set checks over one totally ordered history of client
//! reports (`invoke`, `acquired`, `read`, `write`, `ok`, `fail`, `info`,
//! `releasing`), each entry numbered `i` in store order.
//!
//! * lost updates: acknowledged elements missing from the final set;
//! * mutual exclusion: the clients' holds checked as a linearizable mutex
//!   with the `wgl` checker, each acquire invoked at `invoke` and completed at
//!   `acquired` (failed when the Lock RPC failed, pending when it never
//!   returned), each release invoked at `releasing` and pending (its reply is
//!   not reported). Overlapping holds in store order are listed as evidence;
//! * interleaved read-modify-writes: another client's write landed between a
//!   holder's read and write;
//! * stale grants: a Lock reply whose key was already deleted at or before
//!   the reply's revision (etcd#11456), from the cluster's revision history.
//!
//! Unguarded clients violate the check when updates are lost or the lock is
//! not a mutex; guarded (fenced) clients only when updates are lost.
use super::models::{verdict_word, MutexOf, MutexOp, STEP_BUDGET};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use wgl::{check_history, OpOutcome, Operation, Verdict};

fn text(entry: &Value, key: &str) -> Option<String> {
    entry.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn index(entry: &Value) -> usize {
    entry["i"].as_u64().unwrap_or(0) as usize
}
/// Python's `str()` of a JSON scalar, for `client/op` labels.
fn label(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        other => other.to_string(),
    }
}

struct Hold {
    client: String,
    op: Value,
    start: usize,
    end: Option<usize>,
    entry: Value,
    read: Option<Value>,
    write: Option<Value>,
}

impl Hold {
    fn name(&self) -> String {
        format!("{}/{}", self.client, label(&self.op))
    }
}

/// Lock holds in store order, from `acquired` to `releasing`.
fn acquisitions(history: &[Value]) -> Vec<Hold> {
    let mut holds: Vec<Hold> = Vec::new();
    for entry in history {
        let kind = text(entry, "type").unwrap_or_default();
        let client = text(entry, "client").unwrap_or_default();
        let op = entry.get("op").cloned().unwrap_or(Value::Null);
        match kind.as_str() {
            "acquired" => {
                let hold = Hold {
                    client,
                    op,
                    start: index(entry),
                    end: None,
                    entry: entry.clone(),
                    read: None,
                    write: None,
                };
                // A repeated (client, op) replaces its hold in place, as a dict does.
                match holds
                    .iter()
                    .position(|h| h.client == hold.client && h.op == hold.op)
                {
                    Some(slot) => holds[slot] = hold,
                    None => holds.push(hold),
                }
            }
            "read" | "write" => {
                let key = entry.get("key");
                if let Some(hold) = holds
                    .iter_mut()
                    .rev()
                    .find(|h| h.client == client && h.entry.get("key") == key)
                {
                    if kind == "read" {
                        hold.read = Some(entry.clone());
                    } else {
                        hold.write = Some(entry.clone());
                    }
                }
            }
            "releasing" => {
                if let Some(hold) = holds.iter_mut().find(|h| h.client == client && h.op == op) {
                    hold.end = Some(index(entry));
                }
            }
            _ => {}
        }
    }
    holds
}

/// The clients' acquires and releases as a `wgl` history ordered by store index.
pub fn mutex_history(history: &[Value]) -> Vec<Operation<MutexOp<String>, ()>> {
    let mut ops: Vec<Operation<MutexOp<String>, ()>> = Vec::new();
    let mut acquiring: BTreeMap<(String, String), usize> = BTreeMap::new();
    for entry in history {
        let kind = text(entry, "type").unwrap_or_default();
        let client = text(entry, "client").unwrap_or_default();
        let op = label(entry.get("op").unwrap_or(&Value::Null));
        let owner = format!("{client}/{op}");
        let at = index(entry) as u64;
        match kind.as_str() {
            "invoke" => {
                acquiring.insert((client, op), ops.len());
                ops.push(Operation {
                    op: MutexOp::Acquire(owner),
                    invoked_at: at,
                    outcome: OpOutcome::Pending,
                });
            }
            "acquired" => match acquiring.remove(&(client, op)) {
                Some(slot) => ops[slot].outcome = OpOutcome::Completed { at, ret: () },
                None => ops.push(Operation {
                    op: MutexOp::Acquire(owner),
                    invoked_at: at,
                    outcome: OpOutcome::Completed { at, ret: () },
                }),
            },
            "fail" if text(entry, "stage").as_deref() == Some("lock") => {
                if let Some(slot) = acquiring.remove(&(client, op)) {
                    ops[slot].outcome = OpOutcome::Failed { at };
                }
            }
            "releasing" => ops.push(Operation {
                op: MutexOp::Release(owner),
                invoked_at: at,
                outcome: OpOutcome::Pending,
            }),
            _ => {}
        }
    }
    ops
}

pub struct Checked {
    pub result: Value,
    pub details: Value,
    pub violation: bool,
}

pub fn check(
    history: &[Value],
    final_set: &[String],
    guard: &str,
    lock_events: Option<&[Value]>,
) -> Checked {
    let acked: Vec<String> = history
        .iter()
        .filter(|e| e["type"] == "ok")
        .filter_map(|e| text(e, "element"))
        .collect();
    let final_set: BTreeSet<&String> = final_set.iter().collect();
    let lost: Vec<&String> = acked.iter().filter(|e| !final_set.contains(e)).collect();
    let mut holds = acquisitions(history);
    for hold in &mut holds {
        hold.end.get_or_insert(history.len());
    }
    let mut overlaps: Vec<[String; 2]> = Vec::new();
    for (position, hold) in holds.iter().enumerate() {
        for other in &holds[position + 1..] {
            if other.start >= hold.end.unwrap_or(usize::MAX) {
                continue;
            }
            if other.client != hold.client {
                overlaps.push([hold.name(), other.name()]);
            }
        }
    }
    let in_overlap: BTreeSet<&String> = overlaps.iter().flatten().collect();
    let writes: Vec<&Value> = history
        .iter()
        .filter(|e| e["type"] == "write" && e["ok"].as_bool().unwrap_or(false))
        .collect();
    let mut interleaved = Vec::new();
    for hold in &holds {
        if let (Some(read), Some(write)) = (&hold.read, &hold.write) {
            if !write["ok"].as_bool().unwrap_or(false) {
                continue;
            }
            let (from, to) = (index(read), index(write));
            let between: Vec<String> = writes
                .iter()
                .filter(|w| {
                    from < index(w)
                        && index(w) < to
                        && text(w, "client").as_deref() != Some(&hold.client)
                })
                .map(|w| {
                    format!(
                        "{}/{}",
                        text(w, "client").unwrap_or_default(),
                        label(&w["element"])
                    )
                })
                .collect();
            if !between.is_empty() {
                interleaved.push(json!({"hold": hold.name(), "by": between}));
            }
        }
    }
    let mut deleted: BTreeMap<String, i64> = BTreeMap::new();
    let mut created: BTreeMap<String, i64> = BTreeMap::new();
    for event in lock_events.unwrap_or(&[]) {
        let Some(key) = text(event, "key").filter(|k| k.starts_with("lock/")) else {
            continue;
        };
        if event["type"] == "DELETE" {
            deleted
                .entry(key)
                .or_insert(event["mod_revision"].as_i64().unwrap_or(0));
        } else {
            created
                .entry(key)
                .or_insert(event["create_revision"].as_i64().unwrap_or(0));
        }
    }
    let mut stale = Vec::new();
    for hold in &holds {
        let key = text(&hold.entry, "key").unwrap_or_default();
        let granted = hold.entry["granted_revision"].as_i64().unwrap_or(0);
        if let Some(&gone) = deleted.get(&key) {
            if gone <= granted {
                stale.push(
                    json!({"hold": hold.name(), "key": key, "deleted_revision": gone,
                                  "granted_revision": hold.entry["granted_revision"],
                                  "create_revision": created.get(&key)}),
                );
            }
        }
    }
    let fenced = history
        .iter()
        .filter(|e| {
            e["fenced"].as_bool().unwrap_or(false)
                || (e["type"] == "fail" && label(&e["stage"]).contains("fenced"))
        })
        .count();
    let keepalive_expired = history
        .iter()
        .filter(|e| {
            e["type"] == "releasing" && e["keepalives"]["expired"].as_i64().unwrap_or(0) != 0
        })
        .count();
    let mutex = mutex_history(history);
    let verdict = check_history(&MutexOf::<String>::default(), &mutex, STEP_BUDGET);
    let (blocked_on, linearized) = match &verdict {
        Verdict::Linearizable => (Value::Null, mutex.len()),
        Verdict::NotLinearizable(e) | Verdict::BudgetExhausted(e) => (
            e.blocked_on
                .map(|i| json!(format!("{:?}", mutex[i].op)))
                .unwrap_or(Value::Null),
            e.longest_prefix.len(),
        ),
    };
    let count = |kind: &str| history.iter().filter(|e| e["type"] == kind).count();
    let result = json!({
        "guard": guard,
        "acked": acked.len(),
        "lost": lost.len(),
        "lost_pct": if acked.is_empty() { Value::Null } else {
            json!((10000.0 * lost.len() as f64 / acked.len() as f64).round() / 100.0)
        },
        "final_size": final_set.len(),
        "holds": holds.len(),
        "overlap_pairs": overlaps.len(),
        "holds_in_overlap": in_overlap.len(),
        "interleaved_rmw": interleaved.len(),
        "stale_grants": stale.len(),
        "key_missing_at_grant": holds.iter().filter(|h| h.entry["key_present"] == false).count(),
        "fenced": fenced,
        "failed": count("fail"),
        "indeterminate": count("info"),
        "holds_with_expired_keepalive": keepalive_expired,
        "lock_keys_created": created.len(),
        "lock_keys_deleted": deleted.len(),
        "mutex": verdict_word(&verdict),
    });
    let details = json!({
        "lost_elements": lost,
        "overlaps": overlaps.iter().take(50).collect::<Vec<_>>(),
        "interleaved": interleaved.iter().take(50).collect::<Vec<_>>(),
        "stale": stale.iter().take(50).collect::<Vec<_>>(),
        "mutex": {"verdict": verdict_word(&verdict), "operations": mutex.len(),
                  "longest_prefix": linearized, "blocked_on": blocked_on},
    });
    let exclusive = matches!(verdict, Verdict::Linearizable);
    let violation = !lost.is_empty() || (guard == "none" && !exclusive);
    Checked {
        result,
        details,
        violation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(steps: &[(&str, Option<i64>, &str, Value)]) -> Vec<Value> {
        steps
            .iter()
            .enumerate()
            .map(|(i, (client, op, kind, extra))| {
                let mut entry = extra.as_object().cloned().unwrap_or_default();
                entry.insert("client".into(), json!(client));
                entry.insert("op".into(), json!(op));
                entry.insert("type".into(), json!(kind));
                entry.insert("i".into(), json!(i));
                Value::Object(entry)
            })
            .collect()
    }

    fn rmw(
        client: &'static str,
        op: i64,
        key: &str,
        element: &str,
        granted: i64,
    ) -> Vec<(&'static str, Option<i64>, &'static str, Value)> {
        vec![
            (client, Some(op), "invoke", json!({"element": element})),
            (
                client,
                Some(op),
                "acquired",
                json!({"key": key, "granted_revision": granted, "key_present": true}),
            ),
            (client, None, "read", json!({"key": key, "ok": true})),
            (
                client,
                None,
                "write",
                json!({"key": key, "ok": true, "element": element}),
            ),
            (client, Some(op), "ok", json!({"element": element})),
            (client, Some(op), "releasing", json!({"key": key})),
        ]
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn serial_holds_lose_nothing() {
        let mut steps = rmw("c1", 1, "lock/a", "c1-1", 10);
        steps.extend(rmw("c2", 1, "lock/b", "c2-1", 10));
        let checked = check(
            &history(&steps),
            &strings(&["c1-1", "c2-1"]),
            "none",
            Some(&[]),
        );
        assert!(!checked.violation);
        assert_eq!(
            [
                &checked.result["acked"],
                &checked.result["lost"],
                &checked.result["overlap_pairs"]
            ],
            [&json!(2), &json!(0), &json!(0)]
        );
        assert_eq!(checked.result["mutex"], "linearizable");
    }

    #[test]
    fn paused_holder_overwrites_the_next_holder() {
        let (a, b) = (
            rmw("c1", 1, "lock/a", "c1-1", 10),
            rmw("c2", 1, "lock/b", "c2-1", 10),
        );
        // c1 reads and is paused; c2 acquires, reads, writes; c1 writes its stale set.
        let steps: Vec<_> = [
            &a[0], &a[1], &a[2], &b[0], &b[1], &b[2], &b[3], &b[4], &a[3], &a[4], &a[5], &b[5],
        ]
        .into_iter()
        .cloned()
        .collect();
        let checked = check(&history(&steps), &strings(&["c1-1"]), "none", Some(&[]));
        assert!(checked.violation);
        assert_eq!(
            [
                &checked.result["lost"],
                &checked.result["overlap_pairs"],
                &checked.result["interleaved_rmw"]
            ],
            [&json!(1), &json!(1), &json!(1)]
        );
        assert_eq!(checked.details["lost_elements"], json!(["c2-1"]));
        assert_eq!(checked.result["mutex"], "not-linearizable");
    }

    #[test]
    fn guarded_overlap_without_loss_is_not_a_violation() {
        let (a, b) = (
            rmw("c1", 1, "lock/a", "c1-1", 10),
            rmw("c2", 1, "lock/b", "c2-1", 10),
        );
        let fenced = (
            "c1",
            None,
            "write",
            json!({"key": "lock/a", "ok": false, "fenced": true, "element": "c1-1"}),
        );
        let steps: Vec<_> = vec![
            a[0].clone(),
            a[1].clone(),
            a[2].clone(),
            b[0].clone(),
            b[1].clone(),
            b[2].clone(),
            b[3].clone(),
            b[4].clone(),
            fenced,
            a[5].clone(),
            b[5].clone(),
        ];
        let checked = check(&history(&steps), &strings(&["c2-1"]), "token", Some(&[]));
        assert!(!checked.violation);
        assert_eq!(
            [
                &checked.result["lost"],
                &checked.result["overlap_pairs"],
                &checked.result["fenced"]
            ],
            [&json!(0), &json!(1), &json!(1)]
        );
        assert_eq!(checked.result["mutex"], "not-linearizable");
    }

    #[test]
    fn grant_after_key_deletion_is_stale() {
        let steps = rmw("c1", 1, "lock/a", "c1-1", 12);
        let events = [
            json!({"type": "PUT", "key": "lock/a", "mod_revision": 5, "create_revision": 5}),
            json!({"type": "DELETE", "key": "lock/a", "mod_revision": 9, "create_revision": 0}),
        ];
        let checked = check(&history(&steps), &strings(&["c1-1"]), "none", Some(&events));
        assert_eq!(checked.result["stale_grants"], 1);
        assert_eq!(checked.details["stale"][0]["deleted_revision"], 9);
    }

    #[test]
    fn waiting_acquires_may_be_granted_after_a_release() {
        // c2 invokes while c1 holds, and is granted after c1 releases: a mutex.
        let steps = vec![
            ("c1", Some(1), "invoke", json!({})),
            ("c1", Some(1), "acquired", json!({"key": "lock/a"})),
            ("c2", Some(1), "invoke", json!({})),
            ("c1", Some(1), "releasing", json!({"key": "lock/a"})),
            ("c2", Some(1), "acquired", json!({"key": "lock/b"})),
            ("c3", Some(1), "invoke", json!({})),
            ("c3", Some(1), "fail", json!({"stage": "lock"})),
            ("c2", Some(1), "releasing", json!({"key": "lock/b"})),
        ];
        let checked = check(&history(&steps), &[], "none", None);
        assert_eq!(checked.result["mutex"], "linearizable");
        assert_eq!(checked.result["overlap_pairs"], 0);
        assert!(!checked.violation);
    }
}
