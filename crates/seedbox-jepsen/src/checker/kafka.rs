//! Checks for Jepsen's Kafka queue workload, over the client's event stream
//! (`H` lines) and its final read of every partition at read_committed.
//!
//! * lost: a send acknowledged in a committed (ok) transaction, or an
//!   acknowledged produce, missing from the final read;
//! * aborted read (G1a): a send of a failed (definitely aborted)
//!   transaction that appears in the final read or in any poll;
//! * torn transaction: an ok or failed transaction whose acknowledged sends
//!   are partly visible and partly not;
//! * inconsistent offsets (two values at one offset), duplicates (one value
//!   at two offsets) and non-monotonic offsets in the final read;
//! * Elle list-append cycles over the final read's version order. Kafka
//!   transactions routinely interleave writes (Jepsen's G0 finding), so cycles
//!   are reported but are not part of the verdict.
//!
//! The `queue` workload (acknowledged produces plus a consumer group) adds
//! Jepsen's queue checks over the consumers' polls, tracking each consumer
//! incarnation's assignment of each partition as a session:
//! * skipped: within a session, a poll jumped over records the final read has;
//! * nonmonotonic poll: within a session, an offset at or below the last one;
//! * vanished: a polled value missing from the final read;
//! * undelivered: a record below the group's final committed offset that no
//!   consumer ever polled;
//! * commit beyond end: a final committed offset past the partition's end.
//!
//! Redeliveries are counted but allowed: the group commits after it polls, so
//! a failure between the two delivers a record again (at least once).
use super::elle::{self, Atom, Mop};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

pub struct Checked {
    pub result: Value,
    pub details: Value,
    pub violation: bool,
}

fn int(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
}

fn take50(items: &[Value]) -> Vec<Value> {
    items.iter().take(50).cloned().collect()
}

pub fn check(events: &[Value], workload: &str) -> Checked {
    let final_read = events.iter().find(|e| e["type"] == "final");
    let mut logs: BTreeMap<i64, Vec<(i64, i64)>> = BTreeMap::new();
    if let Some(final_read) = final_read {
        if let Some(map) = final_read["logs"].as_object() {
            for (k, pairs) in map {
                let Some(k) = k.parse().ok() else { continue };
                let pairs = pairs
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| Some((int(&p[0])?, int(&p[1])?)))
                    .collect();
                logs.insert(k, pairs);
            }
        }
    }
    let complete = final_read.is_some_and(|f| f["status"] == "complete");
    let mut position: HashMap<(i64, i64), i64> = HashMap::new();
    let (mut duplicates, mut inconsistent, mut nonmonotonic) = (Vec::new(), Vec::new(), Vec::new());
    for (k, pairs) in &logs {
        let mut seen: HashMap<i64, i64> = HashMap::new();
        let mut last = -1;
        for &(o, v) in pairs {
            if o <= last {
                nonmonotonic.push(json!({"key": k, "offset": o, "after": last}));
            }
            last = o;
            if let Some(previous) = seen.get(&v) {
                duplicates.push(json!({"key": k, "value": v, "offsets": [previous, o]}));
            }
            seen.insert(v, o);
            position.insert((*k, v), o);
        }
    }
    let by_offset: HashMap<(i64, i64), i64> = logs
        .iter()
        .flat_map(|(k, pairs)| pairs.iter().map(move |&(o, v)| ((*k, o), v)))
        .collect();
    let observe = |k: i64, o: Option<i64>, v: i64, place: String, inconsistent: &mut Vec<Value>| {
        let Some(o) = o else { return };
        if let Some(&have) = by_offset.get(&(k, o)) {
            if have != v {
                inconsistent.push(
                    json!({"key": k, "offset": o, "value": v, "final": have, "where": place}),
                );
            }
        }
    };
    let (mut lost, mut aborted_read, mut torn, mut aborted_poll, mut committed_fail) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    // Each partition's last stable offset at the final read: an open
    // transaction below it hides later committed writes from read_committed
    // readers, so those are unseen, not lost.
    let mut unseen: Vec<Value> = Vec::new();
    let lso: HashMap<i64, i64> = final_read
        .and_then(|f| f["end-offsets"].as_object())
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| Some((k.parse().ok()?, int(v)?)))
        .collect();
    let beyond =
        |k: i64, o: Option<i64>| o.is_some_and(|o| lso.get(&k).is_some_and(|end| o >= *end));
    let mut stats: BTreeMap<String, usize> = BTreeMap::new();
    let mut txns: Vec<elle::TxnOp> = Vec::new();
    let mut failed_values: HashMap<(i64, i64), Value> = HashMap::new();
    if workload == "produce" || workload == "queue" {
        for e in events {
            if e["f"] != "send" || e["type"] == "invoke" {
                continue;
            }
            let (Some(k), Some(v)) = (int(&e["k"]), int(&e["v"])) else {
                continue;
            };
            let kind = e["type"].as_str().unwrap_or("").to_string();
            *stats.entry(kind.clone()).or_default() += 1;
            if kind == "ok" {
                observe(k, int(&e["offset"]), v, "ack".into(), &mut inconsistent);
                if !position.contains_key(&(k, v)) && beyond(k, int(&e["offset"])) {
                    unseen.push(json!({"unseen": [[k, v]], "offset": e.get("offset")}));
                } else if !position.contains_key(&(k, v)) {
                    lost.push(
                        json!({"key": k, "value": v, "offset": e.get("offset"), "t": e.get("t")}),
                    );
                }
            } else if position.contains_key(&(k, v)) {
                *stats.entry("info-visible".into()).or_default() += 1;
            }
        }
    } else {
        let ops: Vec<&Value> = events
            .iter()
            .filter(|e| {
                matches!(e["type"].as_str(), Some("ok" | "fail" | "info")) && e["f"] == "txn"
            })
            .collect();
        for e in &ops {
            let kind = e["type"].as_str().unwrap_or("");
            *stats.entry(kind.into()).or_default() += 1;
            let (mut sends, mut attempted) = (Vec::new(), Vec::new());
            for m in e["value"].as_array().into_iter().flatten() {
                match m[0].as_str() {
                    Some("send") => {
                        let Some(k) = int(&m[1]) else { continue };
                        if let Some(pair) = m[2].as_array() {
                            let (o, Some(v)) = (int(&pair[0]), int(&pair[1])) else {
                                continue;
                            };
                            attempted.push((k, o, v));
                            sends.push((k, v));
                            observe(k, o, v, format!("op {}", e["op"]), &mut inconsistent);
                        } else if let Some(v) = int(&m[2]) {
                            sends.push((k, v));
                        }
                    }
                    Some("poll") => {
                        for (k, pairs) in m[1].as_object().into_iter().flatten() {
                            let Ok(k) = k.parse::<i64>() else { continue };
                            for pair in pairs.as_array().into_iter().flatten() {
                                if let Some(v) = int(&pair[1]) {
                                    observe(
                                        k,
                                        int(&pair[0]),
                                        v,
                                        format!("poll op {}", e["op"]),
                                        &mut inconsistent,
                                    );
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            let visible: Vec<(i64, i64)> = sends
                .iter()
                .copied()
                .filter(|kv| position.contains_key(kv))
                .collect();
            let missing: Vec<(i64, i64)> = attempted
                .iter()
                .filter(|(k, _, v)| !position.contains_key(&(*k, *v)))
                .map(|(k, _, v)| (*k, *v))
                .collect();
            match kind {
                "fail" => {
                    for kv in &sends {
                        failed_values.insert(*kv, e["op"].clone());
                    }
                    if !visible.is_empty() {
                        aborted_read.push(json!({"op": e["op"], "txid": e.get("txid"), "visible": visible, "error": e.get("error")}));
                        if !missing.is_empty() {
                            torn.push(json!({"op": e["op"], "type": "fail", "visible": visible, "missing": missing}));
                        } else {
                            committed_fail.push(e["op"].clone());
                        }
                    }
                }
                "ok" if complete => {
                    let hidden: Vec<(i64, i64)> = attempted
                        .iter()
                        .filter(|(k, o, v)| !position.contains_key(&(*k, *v)) && beyond(*k, *o))
                        .map(|(k, _, v)| (*k, *v))
                        .collect();
                    if !hidden.is_empty() {
                        unseen
                            .push(json!({"op": e["op"], "txid": e.get("txid"), "unseen": hidden}));
                    }
                    let gone: Vec<(i64, i64)> = sends
                        .iter()
                        .copied()
                        .filter(|kv| !position.contains_key(kv) && !hidden.contains(kv))
                        .collect();
                    if !gone.is_empty() {
                        lost.push(json!({"op": e["op"], "txid": e.get("txid"), "lost": gone, "visible": visible}));
                        if !visible.is_empty() {
                            torn.push(json!({"op": e["op"], "type": "ok", "visible": visible, "missing": gone}));
                        }
                    }
                }
                "info" => {
                    let name = if visible.is_empty() {
                        "info-invisible"
                    } else {
                        "info-visible"
                    };
                    *stats.entry(name.into()).or_default() += 1;
                }
                _ => {}
            }
            let id = Atom::from_json(&e["op"]).unwrap_or(Atom::Str(e["op"].to_string()));
            let mops = sends
                .iter()
                .map(|(k, v)| Mop::Append(Atom::Int(*k), *v))
                .collect();
            if let Ok(op) = elle::txn(id, kind, mops, Atom::from_json(&e["process"]), 0, 0, None) {
                txns.push(op);
            }
        }
        for e in &ops {
            for m in e["value"].as_array().into_iter().flatten() {
                if m[0] != "poll" {
                    continue;
                }
                for (k, pairs) in m[1].as_object().into_iter().flatten() {
                    let Ok(k) = k.parse::<i64>() else { continue };
                    for pair in pairs.as_array().into_iter().flatten() {
                        let Some(v) = int(&pair[1]) else { continue };
                        if let Some(writer) = failed_values.get(&(k, v)) {
                            aborted_poll.push(json!({"reader": e["op"], "writer": writer, "key": k, "value": v, "offset": pair[0]}));
                        }
                    }
                }
            }
        }
    }
    let queue = (workload == "queue").then(|| check_queue(events, &logs, &by_offset, &lso));
    if let Some(queue) = &queue {
        inconsistent.extend(queue.inconsistent.iter().cloned());
    }
    let mut cycles = json!({});
    if !txns.is_empty() && final_read.is_some() {
        let reads = logs
            .iter()
            .map(|(k, pairs)| {
                Mop::Read(Atom::Int(*k), Some(pairs.iter().map(|(_, v)| *v).collect()))
            })
            .collect();
        if let Ok(reader) = elle::txn("final-read", "ok", reads, None, 0, 0, None) {
            txns.push(reader);
            cycles = match elle::analyze(&txns) {
                Ok(analyzed) => json!(analyzed
                    .anomalies
                    .iter()
                    .map(|(k, v)| (k.clone(), v.len()))
                    .collect::<BTreeMap<_, _>>()),
                Err(error) => json!({"error": error}),
            };
        }
    }
    let mut result = json!({
        "final": final_read.map(|f| f["status"].clone()),
        "records": logs.values().map(Vec::len).sum::<usize>(),
        "ops": stats,
        "lost": lost.len(), "aborted_read": aborted_read.len(), "aborted_poll": aborted_poll.len(),
        "torn": torn.len(), "committed_fail": committed_fail.len(),
        "unseen": unseen.iter().map(|u| u["unseen"].as_array().map_or(0, Vec::len)).sum::<usize>(),
        "duplicates": duplicates.len(),
        "inconsistent": inconsistent.len(), "nonmonotonic": nonmonotonic.len(), "elle": cycles,
    });
    let mut violation = [
        &lost,
        &aborted_read,
        &aborted_poll,
        &torn,
        &duplicates,
        &inconsistent,
        &nonmonotonic,
    ]
    .iter()
    .any(|items| !items.is_empty());
    let mut details = json!({
        "lost": take50(&lost), "unseen": take50(&unseen), "aborted_read": take50(&aborted_read), "aborted_poll": take50(&aborted_poll),
        "torn": take50(&torn), "duplicates": take50(&duplicates), "inconsistent": take50(&inconsistent),
        "nonmonotonic": take50(&nonmonotonic),
    });
    if let Some(queue) = queue {
        violation |= queue.violation();
        result["queue"] = queue.result();
        details["queue"] = queue.details();
    }
    Checked {
        result,
        details,
        violation,
    }
}

/// What the queue checks found.
#[derive(Default)]
struct Queue {
    drained: Option<Value>,
    committed: BTreeMap<i64, i64>,
    polled: usize,
    redelivered: usize,
    skipped: Vec<Value>,
    nonmonotonic_poll: Vec<Value>,
    vanished: Vec<Value>,
    undelivered: Vec<Value>,
    beyond_end: Vec<Value>,
    inconsistent: Vec<Value>,
    stats: BTreeMap<String, usize>,
}

impl Queue {
    fn violation(&self) -> bool {
        [
            &self.skipped,
            &self.nonmonotonic_poll,
            &self.vanished,
            &self.undelivered,
            &self.beyond_end,
        ]
        .iter()
        .any(|items| !items.is_empty())
    }

    fn result(&self) -> Value {
        json!({
            "drained": self.drained, "committed": self.committed, "polled": self.polled,
            "redelivered": self.redelivered, "skipped": self.skipped.len(),
            "nonmonotonic_poll": self.nonmonotonic_poll.len(), "vanished": self.vanished.len(),
            "undelivered": self.undelivered.len(), "commit_beyond_end": self.beyond_end.len(),
            "events": self.stats,
        })
    }

    fn details(&self) -> Value {
        json!({
            "skipped": take50(&self.skipped), "nonmonotonic_poll": take50(&self.nonmonotonic_poll),
            "vanished": take50(&self.vanished), "undelivered": take50(&self.undelivered),
            "commit_beyond_end": take50(&self.beyond_end),
        })
    }
}

fn check_queue(
    events: &[Value],
    logs: &BTreeMap<i64, Vec<(i64, i64)>>,
    by_offset: &HashMap<(i64, i64), i64>,
    lso: &HashMap<i64, i64>,
) -> Queue {
    let mut q = Queue::default();
    let values: std::collections::HashSet<(i64, i64)> = logs
        .iter()
        .flat_map(|(k, pairs)| pairs.iter().map(move |&(_, v)| (*k, v)))
        .collect();
    // (consumer, incarnation, partition) -> last polled offset in the session.
    let mut sessions: HashMap<(i64, i64, i64), Option<i64>> = HashMap::new();
    let mut deliveries: HashMap<(i64, i64), usize> = HashMap::new();
    let partitions = |e: &Value| -> Vec<i64> {
        e["partitions"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(int)
            .collect()
    };
    for e in events {
        let kind = e["type"].as_str().unwrap_or("");
        let (c, inc) = (int(&e["c"]).unwrap_or(-1), int(&e["inc"]).unwrap_or(-1));
        match kind {
            "assigned" => {
                *q.stats.entry("assigned".into()).or_default() += 1;
                for p in partitions(e) {
                    sessions.insert((c, inc, p), None);
                }
            }
            "revoked" | "lost" => {
                *q.stats.entry(kind.into()).or_default() += 1;
                for p in partitions(e) {
                    sessions.remove(&(c, inc, p));
                }
            }
            "consumer-closed" => sessions.retain(|(sc, si, _), _| (*sc, *si) != (c, inc)),
            "consumer-error" => *q.stats.entry("consumer-error".into()).or_default() += 1,
            "commit" => {
                let result = e["result"].as_str().unwrap_or("");
                *q.stats.entry(format!("commit-{result}")).or_default() += 1;
            }
            "drained" => {
                q.drained = Some(e["status"].clone());
                q.committed = e["committed"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.parse().ok()?, int(v)?)))
                    .collect();
            }
            "group-offsets" => {
                q.committed = e["committed"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.parse().ok()?, int(v)?)))
                    .collect();
            }
            "poll" => {
                for (p, pairs) in e["records"].as_object().into_iter().flatten() {
                    let Ok(p) = p.parse::<i64>() else { continue };
                    let log = logs.get(&p).map(Vec::as_slice).unwrap_or(&[]);
                    let session = sessions.get_mut(&(c, inc, p));
                    if session.is_none() {
                        *q.stats.entry("unassigned-poll".into()).or_default() += 1;
                    }
                    let mut last = session.as_ref().and_then(|s| **s);
                    for pair in pairs.as_array().into_iter().flatten() {
                        let (Some(o), Some(v)) = (int(&pair[0]), int(&pair[1])) else {
                            continue;
                        };
                        q.polled += 1;
                        let count = deliveries.entry((p, v)).or_default();
                        *count += 1;
                        if *count == 2 {
                            q.redelivered += 1;
                        }
                        match by_offset.get(&(p, o)) {
                            Some(&have) if have != v => q.inconsistent.push(json!(
                                {"key": p, "offset": o, "value": v, "final": have, "where": format!("poll c{c}/{inc}")})),
                            _ => {}
                        }
                        if !values.contains(&(p, v)) {
                            q.vanished.push(json!({"key": p, "offset": o, "value": v, "consumer": [c, inc], "t": e.get("t")}));
                        }
                        if let Some(previous) = last {
                            if o <= previous {
                                q.nonmonotonic_poll.push(json!({"key": p, "consumer": [c, inc], "offset": o, "after": previous, "t": e.get("t")}));
                            } else {
                                let start = log.partition_point(|&(lo, _)| lo <= previous);
                                let stop = log.partition_point(|&(lo, _)| lo < o);
                                if stop > start {
                                    q.skipped.push(json!({"key": p, "consumer": [c, inc], "from": previous, "to": o,
                                        "missing": log[start..stop].iter().take(10).map(|&(lo, lv)| json!([lo, lv])).collect::<Vec<_>>(),
                                        "count": stop - start, "t": e.get("t")}));
                                }
                            }
                        }
                        last = Some(o);
                    }
                    if let Some(session) = sessions.get_mut(&(c, inc, p)) {
                        *session = last;
                    }
                }
            }
            _ => {}
        }
    }
    for (p, &committed) in &q.committed {
        let log = logs.get(p).map(Vec::as_slice).unwrap_or(&[]);
        let end = lso
            .get(p)
            .copied()
            .unwrap_or_else(|| log.last().map_or(0, |&(o, _)| o + 1));
        if committed > end {
            q.beyond_end
                .push(json!({"key": p, "committed": committed, "end": end}));
        }
        for &(o, v) in log.iter().take_while(|&&(o, _)| o < committed) {
            if !deliveries.contains_key(&(*p, v)) {
                q.undelivered
                    .push(json!({"key": p, "offset": o, "value": v, "committed": committed}));
            }
        }
    }
    q
}

/// The one-line summary a VERDICT carries.
pub fn summarize(result: &Value, faults: u64) -> String {
    let queue = &result["queue"];
    let queue = if queue.is_object() {
        format!(
            "; queue: drained {}, {} polled, {} redelivered, {} skipped, {} nonmonotonic polls, {} vanished, {} undelivered, {} commits beyond end",
            queue["drained"], queue["polled"], queue["redelivered"], queue["skipped"],
            queue["nonmonotonic_poll"], queue["vanished"], queue["undelivered"], queue["commit_beyond_end"]
        )
    } else {
        String::new()
    };
    format!(
        "{} lost, {} unseen, {} aborted reads ({} in polls), {} torn, {} duplicates, {} inconsistent offsets; ops {}; elle {}; {} records; {faults} faults",
        result["lost"], result["unseen"], result["aborted_read"], result["aborted_poll"], result["torn"], result["duplicates"],
        result["inconsistent"], result["ops"], result["elle"], result["records"]
    ) + &queue
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(n: i64, kind: &str, value: Value) -> Value {
        json!({"type": kind, "f": "txn", "op": n, "process": 0, "value": value})
    }
    fn final_read(logs: Value, status: &str) -> Value {
        json!({"type": "final", "status": status, "logs": logs})
    }

    #[test]
    fn clean_history() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]], ["send", 1, [0, 11]]])),
            json!({"type": "fail", "f": "txn", "op": 1, "process": 0, "error": "intentional",
                   "value": [["send", 0, [1, 12]], ["send", 1, 13]]}),
            op(2, "ok", json!([["poll", {"0": [[0, 10]]}]])),
            final_read(json!({"0": [[0, 10]], "1": [[0, 11]]}), "complete"),
        ];
        let checked = check(&events, "txn");
        assert!(!checked.violation, "{}", checked.details);
        assert_eq!(checked.result["records"], 2);
    }

    #[test]
    fn aborted_read_from_delayed_commit() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]]])),
            op(
                1,
                "fail",
                json!([["send", 0, [1, 11]], ["send", 1, [0, 12]]]),
            ),
            op(2, "ok", json!([["poll", {"0": [[1, 11]]}]])),
            final_read(json!({"0": [[0, 10], [1, 11]], "1": []}), "complete"),
        ];
        let checked = check(&events, "txn");
        assert!(checked.violation);
        assert_eq!(checked.result["aborted_read"], 1);
        assert_eq!(checked.result["aborted_poll"], 1);
        assert_eq!(checked.result["torn"], 1);
        assert_eq!(
            checked.details["aborted_read"][0]["visible"],
            json!([[0, 11]])
        );
        assert_eq!(checked.result["elle"]["G1a"], 1);
    }

    #[test]
    fn lost_write_from_delayed_abort() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]], ["send", 1, [0, 11]]])),
            final_read(json!({"0": [], "1": [[0, 11]]}), "complete"),
        ];
        let checked = check(&events, "txn");
        assert!(checked.violation);
        assert_eq!(
            (
                checked.result["lost"].clone(),
                checked.result["torn"].clone()
            ),
            (json!(1), json!(1))
        );
        assert_eq!(checked.details["lost"][0]["lost"], json!([[0, 10]]));
    }

    #[test]
    fn writes_beyond_the_last_stable_offset_are_unseen() {
        // An open transaction at offset 1 holds the LSO: the later committed write is unseen.
        let mut last = final_read(json!({"0": [[0, 10]]}), "complete");
        last["end-offsets"] = json!({"0": 1});
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]]])),
            op(1, "info", json!([["send", 0, [1, 11]]])),
            op(2, "ok", json!([["send", 0, [2, 12]]])),
            last,
        ];
        let checked = check(&events, "txn");
        assert!(!checked.violation);
        assert_eq!(
            (
                checked.result["lost"].clone(),
                checked.result["unseen"].clone()
            ),
            (json!(0), json!(1))
        );
    }

    #[test]
    fn info_may_go_either_way() {
        let events = [
            op(0, "info", json!([["send", 0, [0, 10]]])),
            op(1, "info", json!([["send", 0, 11]])),
            final_read(json!({"0": [[0, 10]]}), "complete"),
        ];
        let checked = check(&events, "txn");
        assert!(!checked.violation);
        assert_eq!(
            checked.result["ops"],
            json!({"info": 2, "info-visible": 1, "info-invisible": 1})
        );
    }

    #[test]
    fn inconsistent_offsets_and_duplicates() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]]])),
            op(1, "ok", json!([["send", 0, [1, 11]]])),
            final_read(json!({"0": [[0, 11], [1, 11]]}), "complete"),
        ];
        let checked = check(&events, "txn");
        assert!(checked.violation);
        assert_eq!(checked.result["duplicates"], 1);
        assert_eq!(checked.result["lost"], 1);
        assert!(checked.result["inconsistent"].as_u64().unwrap() >= 1);
    }

    #[test]
    fn incomplete_final_read_does_not_claim_loss() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]]])),
            final_read(json!({"0": []}), "timeout"),
        ];
        let checked = check(&events, "txn");
        assert!(!checked.violation);
        assert_eq!(checked.result["final"], "timeout");
    }

    #[test]
    fn g0_is_reported_but_not_a_violation() {
        let events = [
            op(0, "ok", json!([["send", 0, [0, 10]], ["send", 1, [1, 11]]])),
            op(1, "ok", json!([["send", 0, [1, 12]], ["send", 1, [0, 13]]])),
            final_read(
                json!({"0": [[0, 10], [1, 12]], "1": [[0, 13], [1, 11]]}),
                "complete",
            ),
        ];
        let checked = check(&events, "txn");
        assert!(!checked.violation);
        assert_eq!(checked.result["elle"]["G0"], 1);
    }

    fn ack(k: i64, v: i64, o: i64) -> Value {
        json!({"type": "ok", "f": "send", "k": k, "v": v, "offset": o})
    }

    fn queue_final(committed: Value) -> [Value; 3] {
        let mut last = final_read(json!({"0": [[0, 1], [1, 2], [2, 3]]}), "complete");
        last["end-offsets"] = json!({"0": 3});
        [
            json!({"type": "drained", "status": "complete", "committed": committed}),
            json!({"type": "group-offsets", "committed": committed}),
            last,
        ]
    }

    #[test]
    fn queue_redelivery_after_a_rebalance_is_allowed() {
        let mut events = vec![
            ack(0, 1, 0),
            ack(0, 2, 1),
            ack(0, 3, 2),
            json!({"type": "assigned", "c": 0, "inc": 1, "partitions": [0]}),
            json!({"type": "poll", "c": 0, "inc": 1, "records": {"0": [[0, 1], [1, 2]]}}),
            json!({"type": "revoked", "c": 0, "inc": 1, "partitions": [0]}),
            json!({"type": "assigned", "c": 1, "inc": 1, "partitions": [0]}),
            json!({"type": "poll", "c": 1, "inc": 1, "records": {"0": [[1, 2], [2, 3]]}}),
        ];
        events.extend(queue_final(json!({"0": 3})));
        let checked = check(&events, "queue");
        assert!(!checked.violation, "{}", checked.details);
        assert_eq!(checked.result["queue"]["redelivered"], 1);
        assert_eq!(checked.result["queue"]["drained"], "complete");
    }

    #[test]
    fn queue_skip_within_a_session_and_undelivered_records() {
        let mut events = vec![
            ack(0, 1, 0),
            ack(0, 2, 1),
            ack(0, 3, 2),
            json!({"type": "assigned", "c": 0, "inc": 1, "partitions": [0]}),
            json!({"type": "poll", "c": 0, "inc": 1, "records": {"0": [[0, 1]]}}),
            json!({"type": "poll", "c": 0, "inc": 1, "records": {"0": [[2, 3]]}}),
        ];
        events.extend(queue_final(json!({"0": 3})));
        let checked = check(&events, "queue");
        assert!(checked.violation);
        assert_eq!(checked.result["queue"]["skipped"], 1);
        assert_eq!(checked.result["queue"]["undelivered"], 1);
        assert_eq!(checked.details["queue"]["undelivered"][0]["value"], 2);
    }

    #[test]
    fn queue_nonmonotonic_vanished_and_commit_beyond_end() {
        let mut events = vec![
            ack(0, 1, 0),
            ack(0, 2, 1),
            ack(0, 3, 2),
            json!({"type": "assigned", "c": 0, "inc": 1, "partitions": [0]}),
            json!({"type": "poll", "c": 0, "inc": 1, "records": {"0": [[0, 1], [1, 2], [2, 3], [3, 4]]}}),
            json!({"type": "poll", "c": 0, "inc": 1, "records": {"0": [[1, 2]]}}),
        ];
        events.extend(queue_final(json!({"0": 4})));
        let checked = check(&events, "queue");
        assert!(checked.violation);
        let queue = &checked.result["queue"];
        assert_eq!(
            (
                queue["nonmonotonic_poll"].clone(),
                queue["vanished"].clone(),
                queue["commit_beyond_end"].clone()
            ),
            (json!(1), json!(1), json!(1))
        );
    }

    #[test]
    fn lost_acknowledged_produce() {
        let events = [
            json!({"type": "ok", "f": "send", "k": 0, "v": 1, "offset": 0}),
            json!({"type": "ok", "f": "send", "k": 0, "v": 2, "offset": 1}),
            json!({"type": "info", "f": "send", "k": 0, "v": 3}),
            final_read(json!({"0": [[0, 1]]}), "complete"),
        ];
        let checked = check(&events, "produce");
        assert!(checked.violation);
        assert_eq!(checked.result["lost"], 1);
        assert_eq!(checked.details["lost"][0]["value"], 2);
    }
}
