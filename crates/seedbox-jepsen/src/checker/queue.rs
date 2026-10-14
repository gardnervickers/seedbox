//! Jepsen's queue checks over a replicated log read back from every node:
//! each published value is unique (`P-C`, publisher and counter), each
//! acknowledgement records the log sequence it was given, and the final
//! reads give each node's own replica.
//!
//! * lost: acknowledged values that no node holds, classified lost-prefix,
//!   lost-middle and lost-postfix per publisher relative to its read values;
//! * divergent: acknowledged values that some nodes hold and others lack;
//! * sequence conflicts: one sequence holding different values on different nodes;
//! * acknowledged sequence moved: an acknowledged value whose sequence holds
//!   another value on some node;
//! * acknowledged sequence reuse: one sequence acknowledged for two values;
//! * stream missing: a node that no longer knows the log;
//! * unexpected: values that were never published.
use crate::kit::ranges;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};

/// One node's final read.
#[derive(Clone, Debug, PartialEq)]
pub enum Read {
    /// Sequence -> value.
    Messages(BTreeMap<u64, String>),
    /// The node, or its meta layer, no longer knows the log.
    StreamMissing,
    /// No read completed.
    Unread,
}

/// (publisher, counter) of a `P-C` value; unparseable values sort last.
pub fn value_key(value: &str) -> (i64, i64) {
    value
        .split_once('-')
        .and_then(|(p, c)| Some((p.parse().ok()?, c.parse().ok()?)))
        .unwrap_or((i64::MAX, i64::MAX))
}

fn sorted_values<'a>(values: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut out: Vec<String> = values.into_iter().cloned().collect();
    out.sort_by_key(|v| value_key(v));
    out
}

pub struct Checked {
    pub result: Value,
    pub details: Value,
    pub violation: bool,
}

/// `acked` maps a value to its acknowledged sequence and acknowledgement time.
pub fn check(
    attempted: &BTreeSet<String>,
    acked: &BTreeMap<String, (u64, f64)>,
    reads: &[(String, Read)],
) -> Checked {
    let read_ok: Vec<(&String, &BTreeMap<u64, String>)> = reads
        .iter()
        .filter_map(|(node, read)| match read {
            Read::Messages(messages) => Some((node, messages)),
            _ => None,
        })
        .collect();
    let mut missing_stream: Vec<&String> = reads
        .iter()
        .filter(|(_, r)| *r == Read::StreamMissing)
        .map(|(n, _)| n)
        .collect();
    missing_stream.sort();
    let mut unread: Vec<&String> = reads
        .iter()
        .filter(|(_, r)| *r == Read::Unread)
        .map(|(n, _)| n)
        .collect();
    unread.sort();
    let held: Vec<(&String, BTreeSet<&String>)> = read_ok
        .iter()
        .map(|(node, messages)| (*node, messages.values().collect()))
        .collect();
    let union: BTreeSet<&String> = held
        .iter()
        .flat_map(|(_, values)| values.iter().copied())
        .collect();
    let acked_values: BTreeSet<&String> = acked.keys().collect();
    let lost: BTreeSet<&String> = acked_values.difference(&union).copied().collect();
    let unexpected: Vec<&String> = union
        .iter()
        .copied()
        .filter(|v| !attempted.contains(*v))
        .collect();
    let per_node_missing: Vec<(&String, Vec<String>)> = held
        .iter()
        .map(|(node, values)| {
            (
                *node,
                sorted_values(acked_values.iter().copied().filter(|v| !values.contains(v))),
            )
        })
        .collect();
    let divergent: BTreeSet<&String> = per_node_missing
        .iter()
        .flat_map(|(_, values)| values.iter())
        .filter(|v| !lost.contains(v))
        .collect();
    let mut conflicts: BTreeMap<u64, BTreeMap<&String, Vec<&String>>> = BTreeMap::new();
    for (node, messages) in &read_ok {
        for (seq, value) in messages.iter() {
            conflicts
                .entry(*seq)
                .or_default()
                .entry(value)
                .or_default()
                .push(node);
        }
    }
    let seq_conflicts: BTreeMap<u64, BTreeMap<&String, Vec<&String>>> = conflicts
        .into_iter()
        .filter(|(_, by)| by.len() > 1)
        .collect();
    let acked_seq_moved = acked_values
        .iter()
        .filter(|v| union.contains(*v))
        .filter(|v| {
            let seq = acked[**v].0;
            read_ok
                .iter()
                .any(|(_, m)| m.get(&seq).is_some_and(|have| have != **v))
        })
        .count();
    let mut by_seq: BTreeMap<u64, Vec<&String>> = BTreeMap::new();
    for (value, (seq, _)) in acked {
        by_seq.entry(*seq).or_default().push(value);
    }
    let seq_reused: BTreeMap<u64, Vec<String>> = by_seq
        .into_iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(seq, values)| (seq, sorted_values(values)))
        .collect();
    let classes = classify(&acked_values, &union, &lost);
    let per_node: Map<String, Value> = read_ok
        .iter()
        .zip(&per_node_missing)
        .map(|((node, messages), (_, missing))| {
            let distinct: BTreeSet<&String> = messages.values().collect();
            (
                (*node).clone(),
                json!({
                    "messages": messages.len(),
                    "missing_acked": missing.len(),
                    "duplicates": messages.len() - distinct.len(),
                    "last_seq": messages.keys().next_back().copied().unwrap_or(0),
                }),
            )
        })
        .collect();
    let result = json!({
        "acked": acked_values.len(),
        "read_union": union.len(),
        "lost": lost.len(),
        "lost_classes": classes,
        "divergent": divergent.len(),
        "unexpected": unexpected.len(),
        "seq_conflicts": seq_conflicts.len(),
        "acked_seq_moved": acked_seq_moved,
        "acked_seq_reused": seq_reused.len(),
        "stream_missing": missing_stream,
        "unread": unread,
        "per_node": per_node,
    });
    let lost_times: Vec<f64> = lost.iter().map(|v| acked[*v].1).collect();
    let examples = |map: &BTreeMap<u64, Value>| -> Value {
        Value::Object(
            map.iter()
                .take(5)
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        )
    };
    let details = json!({
        "lost_seqs": ranges(lost.iter().map(|v| acked[*v].0)),
        "lost_values": sorted_values(lost.iter().copied()).into_iter().take(40).collect::<Vec<_>>(),
        "lost_ack_time": if lost.is_empty() { Value::Null } else {
            json!([lost_times.iter().copied().fold(f64::INFINITY, f64::min),
                   lost_times.iter().copied().fold(f64::NEG_INFINITY, f64::max)])
        },
        "missing_by_node": per_node_missing.iter().filter(|(_, m)| !m.is_empty())
            .map(|(node, m)| ((*node).clone(), Value::from(ranges(m.iter().map(|v| acked[v].0)))))
            .collect::<Map<_, _>>(),
        "seq_conflict_ranges": ranges(seq_conflicts.keys().copied()),
        "seq_conflict_examples": examples(&seq_conflicts.iter().map(|(s, by)| (*s, json!(by))).collect()),
        "unexpected_values": sorted_values(unexpected.iter().copied()).into_iter().take(20).collect::<Vec<_>>(),
        "acked_seq_reused_ranges": ranges(seq_reused.keys().copied()),
        "acked_seq_reused_examples": examples(&seq_reused.iter().map(|(s, v)| (*s, json!(v))).collect()),
    });
    let violation = !lost.is_empty()
        || !divergent.is_empty()
        || !unexpected.is_empty()
        || !seq_conflicts.is_empty()
        || !missing_stream.is_empty()
        || acked_seq_moved > 0
        || !seq_reused.is_empty();
    Checked {
        result,
        details,
        violation,
    }
}

fn classify(
    acked: &BTreeSet<&String>,
    union: &BTreeSet<&String>,
    lost: &BTreeSet<&String>,
) -> Value {
    let mut by_publisher: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for value in acked {
        let (publisher, counter) = value_key(value);
        by_publisher.entry(publisher).or_default().push(counter);
    }
    let mut classes: BTreeMap<&str, usize> = BTreeMap::new();
    for (publisher, counters) in by_publisher {
        let name = |c: i64| format!("{publisher}-{c}");
        let read: Vec<i64> = counters
            .iter()
            .copied()
            .filter(|c| union.contains(&name(*c)))
            .collect();
        for counter in &counters {
            if !lost.contains(&name(*counter)) {
                continue;
            }
            let class = match (read.iter().min(), read.iter().max()) {
                (_, None) => "lost-postfix",
                (_, Some(max)) if counter > max => "lost-postfix",
                (Some(min), _) if counter < min => "lost-prefix",
                _ => "lost-middle",
            };
            *classes.entry(class).or_default() += 1;
        }
    }
    json!(classes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acked(pairs: &[(&str, u64)]) -> BTreeMap<String, (u64, f64)> {
        pairs
            .iter()
            .map(|(v, s)| (v.to_string(), (*s, *s as f64)))
            .collect()
    }
    fn messages(pairs: &[(u64, &str)]) -> Read {
        Read::Messages(pairs.iter().map(|(s, v)| (*s, v.to_string())).collect())
    }
    fn nodes(read: Read, names: &[&str]) -> Vec<(String, Read)> {
        names
            .iter()
            .map(|n| (n.to_string(), read.clone()))
            .collect()
    }
    fn attempted(acks: &BTreeMap<String, (u64, f64)>) -> BTreeSet<String> {
        acks.keys().cloned().collect()
    }

    #[test]
    fn every_node_holds_every_ack() {
        let acks = acked(&[("0-0", 1), ("1-0", 2)]);
        let checked = check(
            &attempted(&acks),
            &acks,
            &nodes(messages(&[(1, "0-0"), (2, "1-0")]), &["n1", "n2"]),
        );
        assert!(!checked.violation);
        assert_eq!(checked.result["lost"], 0);
    }

    #[test]
    fn lost_everywhere_is_lost_not_divergent() {
        let acks = acked(&[("0-0", 1), ("0-1", 2), ("0-2", 3)]);
        let checked = check(
            &attempted(&acks),
            &acks,
            &nodes(messages(&[(1, "0-0"), (3, "0-2")]), &["n1", "n2"]),
        );
        assert!(checked.violation);
        assert_eq!(
            (
                checked.result["lost"].clone(),
                checked.result["divergent"].clone()
            ),
            (json!(1), json!(0))
        );
        assert_eq!(checked.result["lost_classes"], json!({"lost-middle": 1}));
        assert_eq!(checked.details["lost_seqs"], json!([2]));
    }

    #[test]
    fn divergence_and_sequence_conflicts() {
        let acks = acked(&[("0-0", 1), ("0-1", 2)]);
        let mut tried = attempted(&acks);
        tried.insert("1-0".into());
        let reads = vec![
            ("n1".to_string(), messages(&[(1, "0-0"), (2, "0-1")])),
            ("n2".to_string(), messages(&[(1, "0-0"), (2, "1-0")])),
        ];
        let checked = check(&tried, &acks, &reads);
        assert!(checked.violation);
        assert_eq!(
            [
                &checked.result["lost"],
                &checked.result["divergent"],
                &checked.result["seq_conflicts"]
            ],
            [&json!(0), &json!(1), &json!(1)]
        );
        assert_eq!(checked.details["missing_by_node"], json!({"n2": [2]}));
    }

    #[test]
    fn one_sequence_acknowledged_twice() {
        let acks: BTreeMap<String, (u64, f64)> =
            [("0-0".to_string(), (1, 0.0)), ("1-0".to_string(), (1, 1.0))].into();
        let checked = check(
            &attempted(&acks),
            &acks,
            &[("n1".into(), messages(&[(1, "1-0")]))],
        );
        assert!(checked.violation);
        assert_eq!(checked.result["acked_seq_reused"], 1);
        assert_eq!(
            checked.details["acked_seq_reused_examples"],
            json!({"1": ["0-0", "1-0"]})
        );
    }

    #[test]
    fn missing_stream() {
        let acks = acked(&[("0-0", 1)]);
        let checked = check(
            &attempted(&acks),
            &acks,
            &[("n1".into(), Read::StreamMissing)],
        );
        assert!(checked.violation);
        assert_eq!(checked.result["stream_missing"], json!(["n1"]));
        assert_eq!(checked.result["lost"], 1);
    }
}
