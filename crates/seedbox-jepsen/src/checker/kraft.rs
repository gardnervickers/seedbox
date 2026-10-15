//! Checks for the KRaft metadata workloads (see `systems::kraft`).
//!
//! History entries are the oracle's `H` lines: `{"i", "p", "type", "t0",
//! "t1", "s0", "s1", "value", "error", "node", "bound"}`, where `s0`/`s1` are
//! positions in the client's event stream (a real-time order) and `node` is
//! the node that served the request.
//!
//! `append` (list-append over CLIENT_METRICS `metrics` lists):
//! * Elle's list-append analysis under serializability, with the final reads
//!   added as committed reads. Reads come from any node's metadata image, so
//!   they may be stale, but every node applies one committed log, so any
//!   cycle, incompatible order, duplicate or garbage element is a bug.
//! * `lost`: an acknowledged append missing from the converged final value.
//! * `divergent`: nodes whose final values still differ after the
//!   convergence window.
//! * `nonmonotonic`: a node's read of a key that is not an extension of an
//!   earlier read of that key from the same node (a metadata image going
//!   backwards).
//! * `stale` (reported, not a violation: Kafka does not promise read-your-
//!   writes for metadata across nodes): reads that began after an
//!   acknowledged append and missed it, split by the serving node's role.
//!
//! `topics` (existence register per topic name): linearizability through
//! `wgl` of create (created / already exists), delete (deleted / unknown)
//! and probe (validate-only create: absent / exists); plus unique topic IDs
//! across successful creates.
use super::elle;
use super::models::{verdict_word, STEP_BUDGET};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use wgl::{check_history, History, SequentialModel, Verdict};

pub struct Checked {
    pub violation: bool,
    pub result: Value,
    pub details: Value,
}

fn s(entry: &Value, key: &str) -> u64 {
    entry[key].as_u64().unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------- append

/// `final_values`: node -> key -> list (None when the node never answered).
pub fn check_append(
    entries: &[Value],
    final_values: &BTreeMap<String, BTreeMap<i64, Vec<i64>>>,
    controllers: &[String],
) -> Checked {
    let mut details = Map::new();
    let mut result = Map::new();
    let mut violation = false;
    // The converged value: the longest list any node holds for the key
    // (divergence is checked separately).
    let mut converged: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for values in final_values.values() {
        for (k, list) in values {
            let slot = converged.entry(*k).or_default();
            if list.len() > slot.len() {
                *slot = list.clone();
            }
        }
    }
    // Divergence: every node must hold exactly the converged value.
    let mut divergent = Vec::new();
    for (node, values) in final_values {
        for (k, want) in &converged {
            let got = values.get(k);
            if got != Some(want) {
                divergent.push(json!({"node": node, "key": k, "len": got.map(Vec::len),
                                      "converged_len": want.len(),
                                      "prefix": got.map(|g| want.starts_with(g))}));
            }
        }
    }
    result.insert("divergent".into(), divergent.len().into());
    if !divergent.is_empty() {
        violation = true;
        details.insert(
            "divergent".into(),
            Value::Array(divergent.into_iter().take(50).collect()),
        );
    }
    // Elle, with one final read per key.
    let mut ops = Vec::new();
    let mut parse_errors = 0;
    for entry in entries {
        let value: Vec<elle::Mop> = entry["value"]
            .as_array()
            .map(|a| a.iter().filter_map(elle::Mop::from_json).collect())
            .unwrap_or_default();
        if value.is_empty() {
            parse_errors += 1;
            continue;
        }
        let t = |key: &str| entry[key].as_f64().map(elle::seconds_to_ns).unwrap_or(0);
        match elle::txn(
            elle::Atom::from_json(&entry["i"]).unwrap_or(elle::Atom::Int(-1)),
            entry["type"].as_str().unwrap_or("info"),
            value,
            elle::Atom::from_json(&entry["p"]),
            t("t0"),
            t("t1"),
            entry["error"].as_str().map(str::to_owned),
        ) {
            Ok(op) => ops.push(op),
            Err(_) => parse_errors += 1,
        }
    }
    let end = ops
        .iter()
        .map(|o| o.invoked_at)
        .max()
        .unwrap_or(0)
        .saturating_add(1_000_000_000);
    for (index, (k, list)) in converged.iter().enumerate() {
        if let Ok(op) = elle::txn(
            elle::Atom::Str(format!("final-{index}")),
            "ok",
            vec![elle::Mop::Read(elle::Atom::Int(*k), Some(list.clone()))],
            Some(elle::Atom::Str("final".into())),
            end,
            end + 1,
            None,
        ) {
            ops.push(op);
        }
    }
    match elle::check(&ops, "serializable") {
        Ok(checked) => {
            if !checked.valid {
                violation = true;
            }
            result.insert(
                "elle".into(),
                json!({"valid": checked.valid, "counts": checked.counts,
                                                "txns": checked.txns}),
            );
            details.insert("elle".into(), checked.to_json());
        }
        Err(error) => {
            result.insert("elle".into(), json!({"error": error}));
        }
    }
    result.insert("unparsed".into(), parse_errors.into());
    // Lost acknowledged appends.
    let mut lost = Vec::new();
    let mut acked = 0;
    for entry in entries.iter().filter(|e| e["type"] == "ok") {
        for mop in entry["value"].as_array().into_iter().flatten() {
            if mop[0] != "append" {
                continue;
            }
            acked += 1;
            let (k, v) = (mop[1].as_i64().unwrap_or(-1), mop[2].as_i64().unwrap_or(-1));
            if !converged.get(&k).is_some_and(|l| l.contains(&v)) {
                lost.push(
                    json!({"key": k, "value": v, "op": entry["i"], "node": entry["node"],
                                 "t1": entry["t1"]}),
                );
            }
        }
    }
    result.insert("acked_appends".into(), acked.into());
    result.insert("lost".into(), lost.len().into());
    if !lost.is_empty() {
        violation = true;
        details.insert(
            "lost".into(),
            Value::Array(lost.into_iter().take(100).collect()),
        );
    }
    // Per-node monotonic reads, in completion order.
    let mut reads: Vec<&Value> = entries
        .iter()
        .filter(|e| e["type"] == "ok" && e["value"][0][0] == "r")
        .collect();
    reads.sort_by_key(|e| s(e, "s1"));
    let mut last: BTreeMap<(String, i64), (Vec<i64>, Value)> = BTreeMap::new();
    let mut nonmonotonic = Vec::new();
    for read in &reads {
        let node = read["node"].as_str().unwrap_or("").to_string();
        let k = read["value"][0][1].as_i64().unwrap_or(-1);
        let list: Vec<i64> = read["value"][0][2]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        if let Some((previous, previous_op)) = last.get(&(node.clone(), k)) {
            if !list.starts_with(previous) {
                nonmonotonic.push(json!({"node": node, "key": k, "earlier": previous_op,
                                         "earlier_len": previous.len(), "later": read["i"],
                                         "later_len": list.len(), "t1": read["t1"]}));
            }
        }
        last.insert((node, k), (list, read["i"].clone()));
    }
    result.insert("nonmonotonic".into(), nonmonotonic.len().into());
    if !nonmonotonic.is_empty() {
        violation = true;
        details.insert(
            "nonmonotonic".into(),
            Value::Array(nonmonotonic.into_iter().take(50).collect()),
        );
    }
    // Stale reads (informational): a read that began after an append was
    // acknowledged and does not contain it.
    let mut acked_at: BTreeMap<(i64, i64), u64> = BTreeMap::new();
    for entry in entries.iter().filter(|e| e["type"] == "ok") {
        for mop in entry["value"].as_array().into_iter().flatten() {
            if mop[0] == "append" {
                acked_at.insert(
                    (mop[1].as_i64().unwrap_or(-1), mop[2].as_i64().unwrap_or(-1)),
                    s(entry, "s1"),
                );
            }
        }
    }
    let mut stale: BTreeMap<&str, u64> = BTreeMap::new();
    let mut stale_examples = Vec::new();
    for read in &reads {
        let k = read["value"][0][1].as_i64().unwrap_or(-1);
        let list: BTreeSet<i64> = read["value"][0][2]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let start = s(read, "s0");
        let missed = acked_at
            .range((k, i64::MIN)..=(k, i64::MAX))
            .filter(|(&(_, v), &at)| at < start && !list.contains(&v))
            .count();
        if missed > 0 {
            let node = read["node"].as_str().unwrap_or("");
            let role = if controllers.iter().any(|c| c == node) {
                "controller"
            } else {
                "broker"
            };
            *stale.entry(role).or_default() += 1;
            if stale_examples.len() < 10 {
                stale_examples
                    .push(json!({"read": read["i"], "node": node, "key": k, "missed": missed}));
            }
        }
    }
    result.insert("stale_reads".into(), json!(stale));
    details.insert("stale_examples".into(), Value::Array(stale_examples));
    let mut kinds = BTreeMap::<&str, u64>::new();
    for entry in entries {
        *kinds
            .entry(entry["type"].as_str().unwrap_or("?"))
            .or_default() += 1;
    }
    result.insert("ops".into(), json!(kinds));
    result.insert("keys".into(), converged.len().into());
    Checked {
        violation,
        result: Value::Object(result),
        details: Value::Object(details),
    }
}

// ---------------------------------------------------------------- topics

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TopicOp {
    Create,
    Delete,
    Probe,
}

/// `true` when the operation found the topic absent (create succeeded,
/// delete found nothing, probe found nothing); `false` when present.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopicRet {
    Created(bool),
    Deleted(bool),
    Present(bool),
}

/// A topic name's existence; initially absent.
#[derive(Default)]
pub struct Existence;

impl SequentialModel for Existence {
    type Op = TopicOp;
    type Ret = TopicRet;
    type State = bool;

    fn initial_state(&self) -> bool {
        false
    }

    fn step(&self, state: &bool, op: &TopicOp, observed: Option<&TopicRet>) -> Option<bool> {
        let present = *state;
        match (op, observed) {
            (TopicOp::Create, None) => Some(true),
            (TopicOp::Create, Some(TopicRet::Created(true))) => (!present).then_some(true),
            (TopicOp::Create, Some(TopicRet::Created(false))) => present.then_some(true),
            (TopicOp::Delete, None) => Some(false),
            (TopicOp::Delete, Some(TopicRet::Deleted(true))) => present.then_some(false),
            (TopicOp::Delete, Some(TopicRet::Deleted(false))) => (!present).then_some(false),
            (TopicOp::Probe, Some(TopicRet::Present(seen))) => {
                (*seen == present).then_some(present)
            }
            (TopicOp::Probe, None) => Some(present),
            _ => None,
        }
    }
}

fn topic_op(entry: &Value) -> Option<(TopicOp, Option<TopicRet>)> {
    let value = &entry["value"];
    let code = value["code"].as_str().unwrap_or("");
    let ok = entry["type"] == "ok";
    match value["f"].as_str()? {
        "create" => Some((
            TopicOp::Create,
            ok.then(|| TopicRet::Created(code == "NONE")),
        )),
        "delete" => Some((
            TopicOp::Delete,
            ok.then(|| TopicRet::Deleted(code == "NONE")),
        )),
        "probe" => Some((
            TopicOp::Probe,
            ok.then(|| TopicRet::Present(code != "NONE")),
        )),
        _ => None,
    }
}

pub fn check_topics(entries: &[Value]) -> Checked {
    let mut details = Map::new();
    let mut result = Map::new();
    let mut violation = false;
    // Events in stream order: (position, is_completion, entry index).
    let mut by_key: BTreeMap<i64, Vec<usize>> = BTreeMap::new();
    for (index, entry) in entries.iter().enumerate() {
        if let Some(k) = entry["value"]["key"].as_i64() {
            by_key.entry(k).or_default().push(index);
        }
    }
    let mut verdicts = BTreeMap::new();
    let mut failures = Vec::new();
    for (key, indices) in &by_key {
        let mut events: Vec<(u64, bool, usize)> = Vec::new();
        for &i in indices {
            events.push((s(&entries[i], "s0"), false, i));
            if entries[i]["type"] != "info" {
                events.push((s(&entries[i], "s1"), true, i));
            }
        }
        events.sort();
        let mut history: History<TopicOp, TopicRet> = History::new();
        let mut ids = BTreeMap::new();
        let mut order = Vec::new();
        for (_, completion, i) in events {
            let Some((op, ret)) = topic_op(&entries[i]) else {
                continue;
            };
            if !completion {
                ids.insert(i, history.invoke(op));
                order.push(i);
            } else if let Some(&id) = ids.get(&i) {
                match (entries[i]["type"].as_str(), ret) {
                    (Some("ok"), Some(ret)) => history.complete(id, ret),
                    _ => history.fail(id),
                }
            }
        }
        let verdict = check_history(&Existence, history.operations(), STEP_BUDGET);
        verdicts.insert(key.to_string(), verdict_word(&verdict));
        match &verdict {
            Verdict::Linearizable => {}
            Verdict::NotLinearizable(evidence) | Verdict::BudgetExhausted(evidence) => {
                if matches!(verdict, Verdict::NotLinearizable(_)) {
                    violation = true;
                }
                let tail: Vec<Value> = evidence
                    .longest_prefix
                    .iter()
                    .rev()
                    .take(12)
                    .rev()
                    .map(|&op| entries[order[op]].clone())
                    .collect();
                failures.push(json!({"key": key, "verdict": verdict_word(&verdict),
                                     "prefix_len": evidence.longest_prefix.len(),
                                     "prefix_tail": tail,
                                     "blocked_on": evidence.blocked_on.map(|op| entries[order[op]].clone()),
                                     "steps": evidence.steps}));
            }
        }
    }
    result.insert("linearizable".into(), json!(verdicts));
    if !failures.is_empty() {
        details.insert("nonlinearizable".into(), Value::Array(failures));
    }
    // Topic IDs are never reused.
    let mut ids: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for entry in entries.iter().filter(|e| e["type"] == "ok") {
        if let Some(id) = entry["value"]["id"].as_str() {
            ids.entry(id.into()).or_default().push(entry["i"].clone());
        }
    }
    let reused: Vec<Value> = ids
        .iter()
        .filter(|(_, ops)| ops.len() > 1)
        .map(|(id, ops)| json!({"id": id, "ops": ops}))
        .collect();
    result.insert("reused_ids".into(), reused.len().into());
    if !reused.is_empty() {
        violation = true;
        details.insert("reused_ids".into(), Value::Array(reused));
    }
    let mut kinds = BTreeMap::<String, u64>::new();
    for entry in entries {
        let f = entry["value"]["f"].as_str().unwrap_or("?");
        let code = entry["value"]["code"].as_str().unwrap_or("-");
        *kinds
            .entry(format!(
                "{f}:{}:{code}",
                entry["type"].as_str().unwrap_or("?")
            ))
            .or_default() += 1;
    }
    result.insert("ops".into(), json!(kinds));
    Checked {
        violation,
        result: Value::Object(result),
        details: Value::Object(details),
    }
}

// ---------------------------------------------------------------- ids

/// Producer IDs: every acknowledged InitProducerId returns a distinct ID.
pub fn check_ids(entries: &[Value]) -> Checked {
    let mut seen: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
    let mut kinds = BTreeMap::<String, u64>::new();
    for entry in entries {
        let kind = entry["type"].as_str().unwrap_or("?");
        *kinds
            .entry(format!("{kind}:{}", entry["error"].as_str().unwrap_or("-")))
            .or_default() += 1;
        if kind == "ok" {
            if let Some(id) = entry["value"]["id"].as_i64() {
                seen.entry(id)
                    .or_default()
                    .push(json!({"op": entry["i"], "node": entry["node"], "t1": entry["t1"]}));
            }
        }
    }
    let duplicates: Vec<Value> = seen
        .iter()
        .filter(|(_, ops)| ops.len() > 1)
        .map(|(id, ops)| json!({"id": id, "ops": ops}))
        .collect();
    let mut by_node: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for (id, ops) in &seen {
        for op in ops {
            let node = op["node"].as_str().unwrap_or("?").to_string();
            let range = by_node.entry(node).or_insert((*id, *id));
            range.0 = range.0.min(*id);
            range.1 = range.1.max(*id);
        }
    }
    let result = json!({"ids": seen.len(), "duplicates": duplicates.len(), "ops": kinds,
                        "ranges": by_node.iter().map(|(n, r)| (n.clone(), json!([r.0, r.1]))).collect::<Map<_, _>>()});
    Checked {
        violation: !duplicates.is_empty(),
        result,
        details: json!({"duplicates": duplicates.into_iter().take(50).collect::<Vec<_>>()}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(i: u64, kind: &str, f: &str, code: &str, s0: u64, s1: u64) -> Value {
        json!({"i": i, "p": 0, "type": kind, "s0": s0, "s1": s1, "t0": 0.0, "t1": 0.0,
               "value": {"f": f, "key": 0, "code": code}})
    }

    #[test]
    fn producer_ids_must_be_unique() {
        let id = |i: u64, n: i64, node: &str| json!({"i": i, "type": "ok", "node": node, "value": {"f": "id", "id": n}});
        assert!(!check_ids(&[id(0, 1, "b1"), id(1, 2, "b2")]).violation);
        assert!(check_ids(&[id(0, 1, "b1"), id(1, 1, "b2")]).violation);
    }

    #[test]
    fn existence_accepts_sequential_create_delete() {
        let h = vec![
            op(0, "ok", "create", "NONE", 0, 1),
            op(1, "ok", "probe", "TOPIC_ALREADY_EXISTS", 2, 3),
            op(2, "ok", "delete", "NONE", 4, 5),
            op(3, "ok", "delete", "UNKNOWN_TOPIC_OR_PARTITION", 6, 7),
            op(4, "ok", "probe", "NONE", 8, 9),
        ];
        let checked = check_topics(&h);
        assert!(!checked.violation, "{}", checked.details);
    }

    #[test]
    fn existence_rejects_a_stale_exists() {
        let h = vec![
            op(0, "ok", "create", "NONE", 0, 1),
            op(1, "ok", "delete", "NONE", 2, 3),
            op(2, "ok", "create", "TOPIC_ALREADY_EXISTS", 4, 5),
        ];
        assert!(check_topics(&h).violation);
        // ...but an indeterminate create in between explains it.
        let h = vec![
            op(0, "ok", "create", "NONE", 0, 1),
            op(1, "ok", "delete", "NONE", 2, 3),
            op(3, "info", "create", "NOT_CONTROLLER", 3, 99),
            op(2, "ok", "create", "TOPIC_ALREADY_EXISTS", 4, 5),
        ];
        assert!(!check_topics(&h).violation);
    }

    fn append(i: u64, kind: &str, pairs: &[(i64, i64)], node: &str, s0: u64, s1: u64) -> Value {
        json!({"i": i, "p": 0, "type": kind, "s0": s0, "s1": s1, "t0": s0 as f64, "t1": s1 as f64,
               "node": node, "value": pairs.iter().map(|(k, v)| json!(["append", k, v])).collect::<Vec<_>>()})
    }

    fn read(i: u64, k: i64, list: &[i64], node: &str, s0: u64, s1: u64) -> Value {
        json!({"i": i, "p": 1, "type": "ok", "s0": s0, "s1": s1, "t0": s0 as f64, "t1": s1 as f64,
               "node": node, "value": [["r", k, list]]})
    }

    fn finals(nodes: &[(&str, &[(i64, &[i64])])]) -> BTreeMap<String, BTreeMap<i64, Vec<i64>>> {
        nodes
            .iter()
            .map(|(n, kv)| {
                (
                    n.to_string(),
                    kv.iter().map(|(k, l)| (*k, l.to_vec())).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn append_flags_lost_divergent_and_backwards_reads() {
        let controllers = vec!["k1".to_string()];
        let h = vec![
            append(0, "ok", &[(1, 1), (2, 2)], "k1", 0, 1),
            read(1, 1, &[1], "b1", 2, 3),
            read(2, 2, &[], "b1", 4, 5),
            append(3, "ok", &[(1, 3)], "k1", 6, 7),
        ];
        let good = finals(&[
            ("k1", &[(1, &[1, 3]), (2, &[2])]),
            ("b1", &[(1, &[1, 3]), (2, &[2])]),
        ]);
        let checked = check_append(&h, &good, &controllers);
        assert!(!checked.violation, "{}", checked.details);
        assert_eq!(checked.result["stale_reads"]["broker"], 1);
        // Lost: the final value misses an acknowledged append.
        let lost = finals(&[("k1", &[(1, &[1]), (2, &[2])])]);
        assert_eq!(check_append(&h, &lost, &controllers).result["lost"], 1);
        // Divergent final values.
        let split = finals(&[
            ("k1", &[(1, &[1, 3]), (2, &[2])]),
            ("b1", &[(1, &[1]), (2, &[2])]),
        ]);
        assert_eq!(
            check_append(&h, &split, &controllers).result["divergent"],
            1
        );
        // A node going backwards.
        let mut back = h.clone();
        back.push(read(4, 1, &[1, 3], "b1", 8, 9));
        back.push(read(5, 1, &[1], "b1", 10, 11));
        assert_eq!(
            check_append(&back, &good, &controllers).result["nonmonotonic"],
            1
        );
        // Incompatible orders across nodes.
        let mut fork = h.clone();
        fork.push(read(6, 1, &[3], "k1", 12, 13));
        assert!(check_append(&fork, &good, &controllers).violation);
    }
}
